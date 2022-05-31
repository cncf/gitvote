use anyhow::Result;
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tracing::info;

mod events;
mod handlers;

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
    // Setup logging
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "gitvote=debug,tower_http=debug")
    }
    tracing_subscriber::fmt::init();

    // Setup and launch HTTP server
    let args = Args::parse();
    let router = handlers::setup_router(args)?;
    let addr = SocketAddr::from(([0, 0, 0, 0], 9000));
    info!("gitvote service started - listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(router.into_make_service())
        .await
        .unwrap();

    Ok(())
}
