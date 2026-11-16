//! This module is the entry point of the `GitVote` service. It sets up the
//! configuration, logging, database, GitHub client, votes processor and HTTP server.

#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use deadpool_postgres::Runtime;
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::aws_lc_rs,
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
    github::{GHApi, retrying_client_builder, single_attempt_client_builder},
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

// The doc comments of `Args` and its fields are used by clap as the help text. The
// trailing period of single paragraph doc comments is removed, and the explicit `about`
// attribute takes precedence over the struct doc comment.

/// Command line arguments.
#[derive(Debug, Parser)]
#[clap(author, version, about)]
struct Args {
    /// Config file path.
    #[clap(short, long)]
    config: PathBuf,
}

/// Runs the `GitVote` service until it is asked to stop.
#[tokio::main]
async fn main() -> Result<()> {
    // Install a process-wide rustls provider before building any TLS config, which
    // only fails when a provider has already been installed
    let _ = aws_lc_rs::default_provider().install_default();

    // Parse command line arguments
    let args = Args::parse();

    // Setup configuration
    let cfg = Cfg::new(&args.config).context("error setting up configuration")?;

    // Setup logging
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gitvote=debug"));
    let subscriber = tracing_subscriber::fmt().with_env_filter(env_filter);
    match cfg.log.format {
        LogFormat::Json => subscriber.json().init(),
        LogFormat::Pretty => subscriber.init(),
    }

    // Setup database
    let tls_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
        .with_no_client_auth();
    let connector = MakeRustlsConnect::new(tls_config);
    let pool = cfg
        .db
        .create_pool(Some(Runtime::Tokio1), connector)
        .context("error creating database pool")?;
    let db = Arc::new(PgDB::new(pool));

    // Setup GitHub client
    let app_id = cfg.github.app_id as u64;
    let app_private_key = cfg.github.app_private_key.clone();
    let app_private_key = jsonwebtoken::EncodingKey::from_rsa_pem(app_private_key.as_bytes())
        .context("error parsing github app private key")?;
    let retrying_client = retrying_client_builder()
        .app(app_id.into(), app_private_key.clone())
        .build()
        .context("error building retrying github client")?;
    let single_attempt_client = single_attempt_client_builder()
        .app(app_id.into(), app_private_key)
        .build()
        .context("error building single attempt github client")?;
    let gh = Arc::new(GHApi::new(app_id, retrying_client, single_attempt_client));

    // Setup and launch votes processor
    let (cmds_tx, cmds_rx) = async_channel::unbounded();
    let cancel_token = CancellationToken::new();
    let votes_processor = processor::Processor::new(db.clone(), gh.clone(), cmds_tx.clone(), cmds_rx);
    let votes_processor_tasks = votes_processor.run(&cancel_token);
    debug!("[votes processor] started");

    // Setup and launch HTTP server
    let router = handlers::setup_router(&cfg, db, gh, cmds_tx);
    let addr: SocketAddr = cfg.addr.parse().context("error parsing server address")?;
    let listener = TcpListener::bind(addr).await.context("error binding server address")?;
    info!(%addr, "gitvote service started");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("error running http server")?;

    // Ask votes processor to stop and wait for it to finish
    cancel_token.cancel();
    votes_processor_tasks.await;
    debug!("[votes processor] stopped");
    info!("gitvote service stopped");

    Ok(())
}

/// Returns a future that will complete when the program is asked to stop via a
/// ctrl+c or terminate signal.
async fn shutdown_signal() {
    // Setup signal handlers
    let ctrl_c = async {
        signal::ctrl_c().await.expect("ctrl+c signal handler should be installable at startup");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("terminate signal handler should be installable at startup")
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
    /// [`ServerCertVerifier::supported_verify_schemes`].
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .expect("crypto provider must be installed at startup")
            .signature_verification_algorithms
            .supported_schemes()
    }

    /// [`ServerCertVerifier::verify_server_cert`].
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

    /// [`ServerCertVerifier::verify_tls12_signature`].
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    /// [`ServerCertVerifier::verify_tls13_signature`].
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
}
