use anyhow::{Context, Result};
use clap::Parser;
use config::{Config, File};
use deadpool_postgres::{Config as DbConfig, Runtime};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{
    signal,
    sync::{broadcast, mpsc},
};
use tracing::info;

mod conf;
mod github;
mod handlers;
mod templates;
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
        std::env::set_var("RUST_LOG", "gitvote=debug,tower_http=debug")
    }
    tracing_subscriber::fmt::init();

    // Setup database
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    let connector = MakeTlsConnector::new(builder.build());
    let db_cfg: DbConfig = cfg.get("db")?;
    let db = db_cfg.create_pool(Some(Runtime::Tokio1), connector)?;

    // Setup and launch votes processor
    let (cmds_tx, cmds_rx) = mpsc::channel(100);
    let (stop_tx, _): (broadcast::Sender<()>, _) = broadcast::channel(1);
    let votes_processor = votes::Processor::new(cfg.clone(), db)?;
    let votes_processor_done = votes_processor.start(cmds_rx, stop_tx.clone());

    // Setup and launch HTTP server
    let router = handlers::setup_router(cmds_tx).await?;
    let addr: SocketAddr = cfg.get_string("addr")?.parse()?;
    info!("gitvote service started - listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(router.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("gitvote service stopped");

    // Ask votes processor to stop and wait for it to finish
    drop(stop_tx);
    votes_processor_done.await;

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
