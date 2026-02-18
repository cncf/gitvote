#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use deadpool_postgres::Runtime;
use octocrab::Octocrab;
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio::{net::TcpListener, signal};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

use crate::{
    cfg_svc::{Cfg, LogFormat},
    db::PgDB,
    github::GHApi,
};

mod cfg_repo;
mod cfg_svc;
mod cmd;
mod db;
mod github;
mod handlers;
mod processor;
mod results;
#[cfg(test)]
mod testutil;
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
    let cfg = Cfg::new(&args.config).context("error setting up configuration")?;

    // Setup logging
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gitvote=debug"));
    let ts = tracing_subscriber::fmt().with_env_filter(env_filter);
    match cfg.log.format {
        LogFormat::Json => ts.json().init(),
        LogFormat::Pretty => ts.init(),
    }

    // Setup database
    let tls_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
        .with_no_client_auth();
    let connector = MakeRustlsConnect::new(tls_config);
    let pool = cfg.db.create_pool(Some(Runtime::Tokio1), connector)?;
    let db = Arc::new(PgDB::new(pool));

    // Setup GitHub client
    let app_id = cfg.github.app_id as u64;
    let app_private_key = cfg.github.app_private_key.clone();
    let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(app_private_key.as_bytes())?;
    let app_client = Octocrab::builder().app(app_id.into(), app_private_key).build()?;
    let gh = Arc::new(GHApi::new(app_client));

    // Setup and launch votes processor
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let votes_processor = processor::Processor::new(db.clone(), gh.clone(), cmds_tx.clone(), cmds_rx);
    let votes_processor_tasks = votes_processor.run(&cancel_token);
    debug!("[votes processor] started");

    // Setup and launch HTTP server
    let router = handlers::setup_router(&cfg, db, gh, cmds_tx);
    let addr: SocketAddr = cfg.addr.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "gitvote service started");
    axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await.unwrap();

    // Ask votes processor to stop and wait for it to finish
    cancel_token.cancel();
    votes_processor_tasks.await;
    debug!("[votes processor] stopped");
    info!("gitvote service stopped");

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

/// Custom certificate verifier that does not perform any verification.
#[derive(Debug)]
struct InsecureCertVerifier;

impl ServerCertVerifier for InsecureCertVerifier {
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .expect("crypto provider not installed")
            .signature_verification_algorithms
            .supported_schemes()
    }

    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
}
