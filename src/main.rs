use anyhow::{Context, Result};
use clap::Parser;
use config::{Config, File};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{signal, sync::mpsc};
use tracing::info;

mod events;
mod handlers;
mod metadata;
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

    // Setup and launch votes manager and commands dispatcher
    let (cmds_tx, cmds_rx) = mpsc::channel(100);
    let votes_manager = Arc::new(votes::Manager::new(cfg.clone())?);
    let commands_dispatcher = votes::commands_dispatcher(cmds_rx, votes_manager.clone());

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

    // Stop commands dispatcher
    commands_dispatcher.abort();

    Ok(())
}

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
