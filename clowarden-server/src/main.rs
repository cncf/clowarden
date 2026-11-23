#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown, clippy::similar_names)]

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, pin::pin, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use cfg::{Config, LogFormat};
use clap::Parser;
use db::DynDB;
use deadpool_postgres::Runtime;
use futures::future;
use github::DynGH;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use tokio::{
    net::TcpListener,
    signal,
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use clowarden_core::{
    self as core,
    github::{ClientPool, RateLimitPolicy},
    services::{self, DynServiceHandler, ServiceName},
};

use crate::db::PgDB;

mod cfg;
mod db;
mod github;
mod handlers;
mod jobs;
mod tmpl;

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// Config file path
    #[clap(short, long)]
    config: PathBuf,
}

/// Maximum time allowed for active HTTP connections to finish once a shutdown
/// signal is received before continuing to background worker drain.
const HTTP_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum time allowed for acknowledged PR work to drain during graceful
/// shutdown before cancelling underlying GitHub HTTP waits.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// Additional grace period after `cancel_jobs` fires before cancelling
/// `transport_cancel` and forcefully aborting any remaining wedged background
/// tasks so shutdown stays bounded while giving interrupted jobs time to post
/// fail-closed check runs and partial reconciliation comments.
const SHUTDOWN_FORCE_ABORT_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup configuration
    let cfg = Config::new(&args.config).context("error setting up configuration")?;

    // Setup logging
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("clowarden=debug"));
    let ts = tracing_subscriber::fmt().with_env_filter(env_filter);
    match cfg.log.format {
        LogFormat::Json => ts.json().init(),
        LogFormat::Pretty => ts.init(),
    }

    // Setup database
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    let connector = MakeTlsConnector::new(builder.build());
    let pool = cfg.db.create_pool(Some(Runtime::Tokio1), connector)?;
    let db: DynDB = Arc::new(PgDB::new(pool));

    // Setup ordered shutdown tokens and shared GitHub client pool:
    // 1. stop_intake stops periodic sweeps and new triage/retry intake
    // 2. stop_workers lets router and org workers exit after triage finishes
    // 3. cancel_jobs interrupts in-flight triage and PR jobs if drain times out
    // 4. transport_cancel aborts remaining HTTP waits once cleanup grace elapses
    let stop_intake = CancellationToken::new();
    let stop_workers = CancellationToken::new();
    let cancel_jobs = CancellationToken::new();
    let transport_cancel = CancellationToken::new();
    let gh_app = &cfg.server.github_app;
    let gh_pool = Arc::new(
        ClientPool::new_with_app_creds_and_options(
            gh_app,
            RateLimitPolicy::default(),
            transport_cancel.clone(),
        )
        .context("error setting up shared github client pool")?,
    );
    let gh: DynGH = Arc::new(github::GHApi::new(gh_pool.clone()));
    let ghc: core::github::DynGH = Arc::new(core::github::GHApi::new(gh_pool.clone()));

    // Setup services handlers
    let mut services: HashMap<ServiceName, DynServiceHandler> = HashMap::new();
    if cfg.services.github.enabled {
        let svc = Arc::new(services::github::service::SvcApi::new(gh_pool));
        services.insert(
            services::github::SERVICE_NAME,
            Arc::new(services::github::Handler::new(svc)),
        );
    }

    // Setup and launch jobs workers
    let orgs = cfg.organizations.clone().unwrap_or_default();
    let reconcile_frequency = Duration::from_secs(cfg.server.reconcile_frequency);
    let clean_tracker = jobs::CleanReconcileTracker::new();
    let deferred_retries = jobs::DeferredRetries::new();
    let latest_pr_heads = jobs::LatestPrHeads::new();
    let (pr_jobs_tx, pr_jobs_rx) = mpsc::unbounded_channel();
    let (periodic_tx, periodic_rx) = mpsc::channel(1);
    let (triage_tx, triage_rx) = mpsc::channel(jobs::WEBHOOK_TRIAGE_QUEUE_CAPACITY);
    let jobs_ctx = jobs::JobsContext {
        cancel_jobs: cancel_jobs.clone(),
        clean_tracker: clean_tracker.clone(),
        db: db.clone(),
        deferred_retries: deferred_retries.clone(),
        gh,
        ghc,
        latest_pr_heads: latest_pr_heads.clone(),
        pr_jobs_tx,
        reconcile_frequency,
        services,
        stop_intake: stop_intake.clone(),
        stop_workers: stop_workers.clone(),
        transport_cancel: transport_cancel.clone(),
    };
    let jobs_handler = jobs::handler(&jobs_ctx, pr_jobs_rx, periodic_rx, &orgs);
    let jobs_scheduler = jobs::scheduler(
        db.clone(),
        clean_tracker,
        periodic_tx,
        stop_intake.clone(),
        &orgs,
        reconcile_frequency,
    );
    let triage_worker = jobs::triage_worker(jobs::TriageContext::from(&jobs_ctx), triage_rx);

    // Setup and launch HTTP server with a bounded graceful shutdown so a stuck
    // HTTP handler cannot delay or prevent worker draining
    let router = handlers::setup_router(&cfg, db.clone(), latest_pr_heads, triage_tx)
        .context("error setting up http server router")?;
    let addr: SocketAddr = cfg.server.addr.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("server started");
    info!(%addr, "listening");
    let drain_start = serve_until_shutdown(listener, router, &stop_intake).await?;

    // Drain pipeline in order under a shared shutdown budget started at signal
    drain_workers(
        jobs_handler,
        jobs_scheduler,
        triage_worker,
        deferred_retries,
        &stop_intake,
        &stop_workers,
        &cancel_jobs,
        &transport_cancel,
        drain_start,
    )
    .await;
    info!("server stopped");

    Ok(())
}

/// Abort all unfinished task handles in `handles`.
fn abort_handles(handles: &[JoinHandle<()>]) {
    for handle in handles {
        handle.abort();
    }
}

/// Await all task handles in `handles`, draining completed entries as they finish
/// so aborted or dropped futures leave only still-running handles behind.
async fn await_handles(handles: &mut Vec<JoinHandle<()>>) {
    while let Some(handle) = handles.last_mut() {
        let _ = handle.await;
        handles.pop();
    }
}

/// Drain background workers in pipeline order under a bounded shutdown budget:
/// 1. Stop periodic sweeps and intake; wait for scheduler, triage, and
///    abandoned deferred-retry checks to finish.
/// 2. Signal router and org workers to drain remaining PR jobs and exit.
/// 3. Cancel in-flight triage and PR jobs if the drain deadline expires while
///    keeping GitHub transport open for a short cleanup grace period so
///    interrupted jobs can publish fail-closed check runs and partial
///    reconciliation comments, then cancel transport and abort any wedged
///    tasks so shutdown always completes before SIGKILL.
#[allow(clippy::too_many_arguments)]
async fn drain_workers(
    jobs_handler: Vec<JoinHandle<()>>,
    jobs_scheduler: Vec<JoinHandle<()>>,
    triage_worker: JoinHandle<()>,
    deferred_retries: jobs::DeferredRetries,
    stop_intake: &CancellationToken,
    stop_workers: &CancellationToken,
    cancel_jobs: &CancellationToken,
    transport_cancel: &CancellationToken,
    drain_start: Instant,
) {
    drain_workers_with_timeouts(
        jobs_handler,
        jobs_scheduler,
        triage_worker,
        deferred_retries,
        stop_intake,
        stop_workers,
        cancel_jobs,
        transport_cancel,
        drain_start + SHUTDOWN_DRAIN_TIMEOUT,
        SHUTDOWN_FORCE_ABORT_TIMEOUT,
    )
    .await;
}

/// Implementation of [`drain_workers`] parameterized by drain deadline and
/// force-abort grace period for deterministic testing.
#[allow(clippy::too_many_arguments)]
async fn drain_workers_with_timeouts(
    jobs_handler: Vec<JoinHandle<()>>,
    jobs_scheduler: Vec<JoinHandle<()>>,
    triage_worker: JoinHandle<()>,
    deferred_retries: jobs::DeferredRetries,
    stop_intake: &CancellationToken,
    stop_workers: &CancellationToken,
    cancel_jobs: &CancellationToken,
    transport_cancel: &CancellationToken,
    drain_deadline: Instant,
    force_abort_timeout: Duration,
) {
    stop_intake.cancel();

    // Wait for intake tasks (scheduler, triage, deferred retries) while keeping
    // handles abortable and never re-polling a completed JoinHandle
    let mut scheduler_handles = jobs_scheduler;
    let mut triage_handles = vec![triage_worker];
    let mut handler_handles = jobs_handler;
    let intake_timed_out = {
        let mut intake_drain = pin!(async {
            let _ = future::join3(
                await_handles(&mut scheduler_handles),
                await_handles(&mut triage_handles),
                deferred_retries.join_all(),
            )
            .await;
        });
        timeout_at(drain_deadline, &mut intake_drain).await.is_err()
    };

    if intake_timed_out {
        warn!("graceful intake drain timed out; cancelling in-flight jobs for cleanup");
        cancel_jobs.cancel();
        let abort_deadline = Instant::now() + force_abort_timeout;
        let final_timed_out = {
            let mut remaining = pin!(async {
                let _ = future::join3(
                    await_handles(&mut scheduler_handles),
                    async {
                        future::join(await_handles(&mut triage_handles), deferred_retries.join_all()).await;
                        stop_workers.cancel();
                    },
                    await_handles(&mut handler_handles),
                )
                .await;
            });
            timeout_at(abort_deadline, &mut remaining).await.is_err()
        };
        stop_workers.cancel();
        transport_cancel.cancel();
        if final_timed_out {
            warn!("shutdown force-abort grace elapsed; aborting wedged background tasks");
            abort_handles(&scheduler_handles);
            abort_handles(&triage_handles);
            deferred_retries.abort_all();
            abort_handles(&handler_handles);
        }
        return;
    }

    // Drain router and organization workers once no new PR jobs can arrive
    stop_workers.cancel();
    let workers_timed_out = {
        let mut workers_drain = pin!(await_handles(&mut handler_handles));
        timeout_at(drain_deadline, &mut workers_drain).await.is_err()
    };
    if workers_timed_out {
        warn!("graceful worker drain timed out; cancelling in-flight jobs for cleanup");
        cancel_jobs.cancel();
        let abort_deadline = Instant::now() + force_abort_timeout;
        let final_timed_out = {
            let mut remaining = pin!(await_handles(&mut handler_handles));
            timeout_at(abort_deadline, &mut remaining).await.is_err()
        };
        transport_cancel.cancel();
        if final_timed_out {
            warn!("worker force-abort grace elapsed; aborting wedged worker tasks");
            abort_handles(&handler_handles);
        }
    }
}

/// Serve HTTP requests until a shutdown signal arrives, bounding HTTP
/// connection draining so a stuck request cannot delay worker shutdown.
async fn serve_until_shutdown(
    listener: TcpListener,
    router: axum::Router,
    stop_intake: &CancellationToken,
) -> Result<Instant> {
    let shutdown_triggered = CancellationToken::new();
    let server_shutdown = shutdown_triggered.clone();
    let mut server_task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                server_shutdown.cancelled().await;
            })
            .await
    });

    tokio::select! {
        res = &mut server_task => {
            if let Ok(Err(err)) = res {
                error!(?err, "server error");
                return Err(err.into());
            }
            Ok(Instant::now())
        }
        () = shutdown_signal() => {
            let started_at = Instant::now();
            shutdown_triggered.cancel();
            stop_intake.cancel();
            if timeout_at(started_at + HTTP_SHUTDOWN_DRAIN_TIMEOUT, &mut server_task)
                .await
                .is_err()
            {
                warn!("graceful http server drain timed out; aborting remaining http connections");
                server_task.abort();
            }
            Ok(started_at)
        }
    }
}

/// Return a future that will complete when the program is asked to stop via a
/// ctrl+c or terminate signal.
async fn shutdown_signal() {
    // Setup signal handlers
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install ctrl+c signal handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install terminate signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // Wait for any of the signals
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_drain_workers_aborts_wedged_tasks_after_bounded_deadlines() {
        // Setup shutdown tokens and background tasks including wedged ones
        let stop_intake = CancellationToken::new();
        let stop_workers = CancellationToken::new();
        let cancel_jobs = CancellationToken::new();
        let transport_cancel = CancellationToken::new();
        let deferred_retries = jobs::DeferredRetries::new();
        let wedged_worker = tokio::spawn(std::future::pending::<()>());
        let wedged_scheduler = tokio::spawn(std::future::pending::<()>());
        let fast_triage = tokio::spawn(async {});
        deferred_retries.push(tokio::spawn(std::future::pending::<()>()));

        // Run bounded worker drain
        let start = Instant::now();
        drain_workers_with_timeouts(
            vec![wedged_worker],
            vec![wedged_scheduler],
            fast_triage,
            deferred_retries,
            &stop_intake,
            &stop_workers,
            &cancel_jobs,
            &transport_cancel,
            Instant::now() + Duration::from_millis(30),
            Duration::from_millis(30),
        )
        .await;

        // Check all tokens fired and shutdown finished within the bounded budget
        assert!(stop_intake.is_cancelled());
        assert!(stop_workers.is_cancelled());
        assert!(cancel_jobs.is_cancelled());
        assert!(transport_cancel.is_cancelled());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "expected bounded shutdown, elapsed={:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_drain_workers_keeps_transport_open_during_cleanup_grace_after_cancel_jobs() {
        // Setup a worker that waits for cancel_jobs, then verifies transport_cancel
        // is still uncancelled so it can publish fail-closed checks or PR comments
        let stop_intake = CancellationToken::new();
        let stop_workers = CancellationToken::new();
        let cancel_jobs = CancellationToken::new();
        let transport_cancel = CancellationToken::new();
        let cleanup_saw_open_transport = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel_jobs = cancel_jobs.clone();
        let worker_transport = transport_cancel.clone();
        let worker_flag = cleanup_saw_open_transport.clone();
        let worker = tokio::spawn(async move {
            worker_cancel_jobs.cancelled().await;
            if !worker_transport.is_cancelled() {
                worker_flag.store(true, std::sync::atomic::Ordering::Release);
            }
        });

        // Drain workers with a short drain deadline and generous cleanup grace
        drain_workers_with_timeouts(
            vec![worker],
            vec![],
            tokio::spawn(async {}),
            jobs::DeferredRetries::new(),
            &stop_intake,
            &stop_workers,
            &cancel_jobs,
            &transport_cancel,
            Instant::now() + Duration::from_millis(20),
            Duration::from_secs(2),
        )
        .await;

        // Check transport stayed open for cleanup before both tokens finished cancelled
        assert!(cleanup_saw_open_transport.load(std::sync::atomic::Ordering::Acquire));
        assert!(cancel_jobs.is_cancelled());
        assert!(transport_cancel.is_cancelled());
    }
}
