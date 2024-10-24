#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown, clippy::similar_names)]

use crate::db::PgDB;
use anyhow::{Context, Result};
use clap::Parser;
use clowarden_core::{
    self as core,
    services::{self, DynServiceHandler, ServiceName},
};
use config::{Config, File};
use deadpool_postgres::{Config as DbConfig, Runtime};
use futures::future;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{
    net::TcpListener,
    signal,
    sync::{broadcast, mpsc},
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

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
        std::env::set_var("RUST_LOG", "clowarden=debug");
    }
    let s = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match cfg.get_string("log.format").as_deref() {
        Ok("json") => s.json().init(),
        _ => s.init(),
    };

    // Setup database
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    let connector = MakeTlsConnector::new(builder.build());
    let db_cfg: DbConfig = cfg.get("db")?;
    let pool = db_cfg.create_pool(Some(Runtime::Tokio1), connector)?;
    let db = Arc::new(PgDB::new(pool));

    // Setup GitHub clients
    let gh_app: core::cfg::GitHubApp = cfg.get("server.githubApp")?;
    let gh = Arc::new(github::GHApi::new(&gh_app).context("error setting up github client")?);
    let ghc = Arc::new(
        core::github::GHApi::new_with_app_creds(&gh_app).context("error setting up core github client")?,
    );

    // Setup services handlers
    let mut services: HashMap<ServiceName, DynServiceHandler> = HashMap::new();
    if cfg.get_bool("services.github.enabled").unwrap_or_default() {
        let svc = Arc::new(services::github::service::SvcApi::new_with_app_creds(&gh_app)?);
        services.insert(
            services::github::SERVICE_NAME,
            Box::new(services::github::Handler::new(ghc.clone(), svc)),
        );
    }

    // Setup and launch jobs workers
    let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
    let jobs_handler = jobs::Handler::new(db.clone(), gh.clone(), ghc.clone(), services);
    let jobs_workers_done = future::join_all([
        jobs_handler.start(jobs_rx, &stop_tx, cfg.get("organizations")?),
        jobs::scheduler(jobs_tx.clone(), stop_tx.subscribe(), cfg.get("organizations")?),
    ]);

    // Setup and launch HTTP server
    let router = handlers::setup_router(&cfg, db.clone(), gh.clone(), jobs_tx)
        .context("error setting up http server router")?;
    let addr: SocketAddr = cfg.get_string("server.addr")?.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("server started");
    info!(%addr, "listening");
    if let Err(err) = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await {
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
    cfg.get_string("server.staticPath")?;
    let _: core::cfg::GitHubApp = cfg.get("server.githubApp")?;
    let _: Vec<core::cfg::Organization> = cfg.get("organizations")?;

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
