use crate::{
    github::GHApi,
    plugins::{DynPlugin, PluginName},
};
use anyhow::{Context, Result};
use clap::Parser;
use config::{Config, File};
use futures::future;
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{
    signal,
    sync::{broadcast, mpsc},
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod directory;
mod github;
mod handlers;
mod jobs;
mod multierror;
mod plugins;
mod tmpl;

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// Config file path
    #[clap(short, long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup configuration
    let cfg = Config::builder()
        .set_default("log.format", "pretty")?
        .set_default("server.addr", "127.0.0.1:9000")?
        .add_source(File::from(args.config))
        .build()
        .context("error setting up configuration")?;
    validate_config(&cfg).context("error validating configuration")?;
    let cfg = Arc::new(cfg);

    // Setup logging
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "clowarden=trace,tower_http=debug")
    }
    let s = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match cfg.get_string("log.format").as_deref() {
        Ok("json") => s.json().init(),
        _ => s.init(),
    };

    // Setup GitHub client
    let gh = Arc::new(GHApi::new(cfg.clone()).context("error setting up github client")?);

    // Setup plugins
    let mut plugins: HashMap<PluginName, DynPlugin> = HashMap::new();
    if cfg.get_bool("plugins.github.enabled").unwrap_or_default() {
        plugins.insert(
            "GitHub",
            Box::new(plugins::github::Plugin::new(cfg.clone(), gh.clone())),
        );
    }

    // Setup and launch jobs workers
    let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
    let jobs_handler = jobs::Handler::new(cfg.clone(), gh.clone(), plugins);
    let jobs_scheduler = jobs::Scheduler::new();
    let jobs_workers_done = future::join_all([
        jobs_handler.start(jobs_rx, stop_tx.subscribe()),
        jobs_scheduler.start(jobs_tx.clone(), stop_tx.subscribe()),
    ]);

    // Setup and launch HTTP server
    let router = handlers::setup_router(cfg.clone(), gh.clone(), jobs_tx);
    let addr: SocketAddr = cfg.get_string("server.addr").unwrap().parse()?;
    info!("server started");
    info!(%addr, "listening");
    if let Err(err) = axum::Server::bind(&addr)
        .serve(router.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        error!(?err, "server error");
        return Err(err.into());
    }

    // Ask jobs workers to stop and wait for them to finish
    drop(stop_tx);
    jobs_workers_done.await;
    info!("server stopped");

    Ok(())
}

/// Check if the configuration provided is valid.
fn validate_config(cfg: &Config) -> Result<()> {
    // Required fields
    cfg.get_string("server.addr")?;
    cfg.get_int("githubApp.appId")?;
    cfg.get_int("githubApp.installationId")?;
    cfg.get_string("githubApp.privateKey")?;
    cfg.get_string("githubApp.webhookSecret")?;
    cfg.get_string("config.organization")?;
    cfg.get_string("config.repository")?;
    cfg.get_string("config.branch")?;

    // Required fields when legacy config is used
    if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
        cfg.get_string("config.legacy.sheriff.permissionsPath")?;
        cfg.get_string("config.legacy.cncf.peoplePath")?;
    }

    Ok(())
}

/// Return a future that will complete when the program is asked to stop via a
/// ctrl+c or terminate signal.
async fn shutdown_signal() {
    // Setup signal handlers
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install ctrl+c signal handler");
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
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
