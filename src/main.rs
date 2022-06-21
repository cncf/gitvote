use crate::{db::PgDB, github::GHApi};
use anyhow::{Context, Result};
use clap::Parser;
use config::{Config, File};
use deadpool_postgres::{Config as DbConfig, Runtime};
use octocrab::Octocrab;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{signal, sync::broadcast};
use tracing::{debug, info};

mod db;
mod github;
mod handlers;
mod tmpl;
mod votes;

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// Config file path
    #[clap(short, long, parse(from_os_str))]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup configuration
    let cfg = Config::builder()
        .set_default("addr", "127.0.0.1:9000")?
        .add_source(File::from(args.config))
        .build()
        .context("error setting up configuration")?;
    let cfg = Arc::new(cfg);

    // Setup logging
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "gitvote=debug")
    }
    tracing_subscriber::fmt::init();

    // Setup database
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    let connector = MakeTlsConnector::new(builder.build());
    let db_cfg: DbConfig = cfg.get("db")?;
    let pool = db_cfg.create_pool(Some(Runtime::Tokio1), connector)?;
    let db = Arc::new(PgDB::new(pool));

    // Setup GitHub client
    let app_id = cfg.get_int("github.appID")? as u64;
    let app_private_key = cfg.get_string("github.appPrivateKey")?;
    let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(app_private_key.as_bytes())?;
    let app_client = Octocrab::builder()
        .app(app_id.into(), app_private_key)
        .build()?;
    let gh = Arc::new(GHApi::new(app_client));

    // Setup and launch votes processor
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
    let votes_processor = votes::Processor::new(db, gh);
    let votes_processor_done = votes_processor.start(cmds_rx, stop_tx.clone());
    debug!("[votes processor] started");

    // Setup and launch HTTP server
    let router = handlers::setup_router(cfg.clone(), cmds_tx).await?;
    let addr: SocketAddr = cfg.get_string("addr")?.parse()?;
    info!("gitvote service started - listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(router.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    // Ask votes processor to stop and wait for it to finish
    drop(stop_tx);
    votes_processor_done.await;
    debug!("[votes processor] stopped");
    info!("gitvote service stopped");

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
