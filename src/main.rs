use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tokio::{signal, sync::mpsc};
use tracing::info;

mod events;
mod handlers;
mod metadata;
mod votes;

#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// GitHub App ID.
    #[clap(long)]
    app_id: u64,

    /// GitHub App private key path.
    #[clap(long, parse(from_os_str))]
    app_private_key: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "gitvote=debug,tower_http=debug")
    }
    tracing_subscriber::fmt::init();

    // Setup and launch votes manager
    let (cmds_tx, cmds_rx) = mpsc::channel(100);
    let manager = votes::Manager::new(args, cmds_rx)?;

    // Setup and launch HTTP server
    let router = handlers::setup_router(cmds_tx).await?;
    let addr = SocketAddr::from(([0, 0, 0, 0], 9000));
    info!("gitvote service started - listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(router.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("gitvote service stopped");

    // Stop votes manager
    manager.stop();

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
