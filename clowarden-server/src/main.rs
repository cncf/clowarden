#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown, clippy::similar_names)]

use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use cfg::{Config, LogFormat};
use clap::Parser;
use db::DynDB;
use deadpool_postgres::Runtime;
use futures::future;
use github::DynGH;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use tokio::{net::TcpListener, signal, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use clowarden_core::{
    self as core,
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup configuration
    let cfg = Config::new(&args.config).context("error setting up configuration")?;

    // Setup logging
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "clowarden=debug");
    }
    let ts = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match cfg.log.format {
        LogFormat::Json => ts.json().init(),
        LogFormat::Pretty => ts.init(),
    };

    // Setup database
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    let connector = MakeTlsConnector::new(builder.build());
    let pool = cfg.db.create_pool(Some(Runtime::Tokio1), connector)?;
    let db: DynDB = Arc::new(PgDB::new(pool));

    // Setup GitHub clients
    let gh_app = &cfg.server.github_app;
    let gh: DynGH = Arc::new(github::GHApi::new(gh_app).context("error setting up github client")?);
    let ghc: core::github::DynGH = Arc::new(
        core::github::GHApi::new_with_app_creds(gh_app).context("error setting up core github client")?,
    );

    // Setup services handlers
    let mut services: HashMap<ServiceName, DynServiceHandler> = HashMap::new();
    if cfg.services.github.enabled {
        let svc = Arc::new(services::github::service::SvcApi::new_with_app_creds(gh_app)?);
        services.insert(
            services::github::SERVICE_NAME,
            Arc::new(services::github::Handler::new(ghc.clone(), svc)),
        );
    }

    // Setup and launch jobs workers
    let orgs = cfg.organizations.clone().unwrap_or_default();
    let cancel_token = CancellationToken::new();
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
    let jobs_handler = jobs::handler(&db, &gh, &ghc, &services, jobs_rx, cancel_token.clone(), &orgs);
    let jobs_scheduler = jobs::scheduler(jobs_tx.clone(), cancel_token.clone(), &orgs);
    let jobs_workers_done = future::join_all([jobs_handler, jobs_scheduler]);

    // Setup and launch HTTP server
    let router = handlers::setup_router(&cfg, db.clone(), gh.clone(), jobs_tx)
        .context("error setting up http server router")?;
    let addr: SocketAddr = cfg.server.addr.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("server started");
    info!(%addr, "listening");
    if let Err(err) = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await {
        error!(?err, "server error");
        return Err(err.into());
    }

    // Ask jobs workers to stop and wait for them to finish
    cancel_token.cancel();
    jobs_workers_done.await;
    info!("server stopped");

    Ok(())
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
