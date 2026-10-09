//! A multi-account Lightning wallet and Lightning Address server for test
//! networks (Mutinynet, signet, testnet, regtest). It refuses to start on
//! Bitcoin mainnet. Custodial and unaudited: never use it with real bitcoin.

mod abuse;
mod auth;
mod blocklist;
mod config;
mod db;
mod error;
mod handoff;
mod ledger;
mod lnd;
mod lnurl;
mod metrics;
mod nostr;
mod password_work;
mod pow;
mod qr;
mod ratelimit;
#[cfg(test)]
mod tests;
mod util;
mod wallet;
mod web;

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use argon2::PasswordHash;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::db::Db;
use crate::lnd::{Lightning, LndRest};
use crate::wallet::Wallet;
use crate::web::App;

/// The name shown in page titles and headers.
pub(crate) const APP_NAME: &str = "Satchel";

#[derive(Parser)]
#[command(
    name = "satchel",
    version,
    about = "Multi-account Lightning wallet and Lightning Address server for test networks only"
)]
struct Cli {
    #[arg(
        long,
        short = 'c',
        env = "SATCHEL_CONFIG",
        help = "Path to the TOML configuration file"
    )]
    config: Option<PathBuf>,
    #[arg(long, env = "SATCHEL_LOG_JSON", help = "Write logs as JSON lines")]
    log_json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Read a password on standard input and print its argon2id hash for the operator login.
    HashPassword,
}

fn init_logging(json: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if json {
        tracing_subscriber::fmt().with_env_filter(filter).json().init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

fn hash_password_from_stdin() -> Result<()> {
    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .context("cannot read the password from standard input")?;
    let password = password.trim_end_matches(['\r', '\n']);
    if password.chars().count() < auth::MIN_PASSWORD_LEN {
        bail!("use at least {} characters", auth::MIN_PASSWORD_LEN);
    }
    println!("{}", auth::hash_password(password)?);
    Ok(())
}

/// The operator hash comes from the environment or a credential file.
fn admin_hash(config: &Config, credentials_dir: &Path) -> Result<Option<String>> {
    let hash = match std::env::var("SATCHEL_ADMIN_PASSWORD_HASH") {
        Ok(hash) => Some(hash),
        Err(_) => config
            .server
            .admin_password_hash_file
            .as_ref()
            .map(|path| std::fs::read_to_string(credentials_dir.join(path)))
            .transpose()
            .context("cannot read the operator password hash")?,
    };
    let Some(hash) = hash.map(|hash| hash.trim().to_owned()).filter(|hash| !hash.is_empty()) else {
        warn!("no operator password hash configured; the operator pages are off");
        return Ok(None);
    };
    PasswordHash::new(&hash).map_err(|error| anyhow::anyhow!("invalid operator password hash: {error}"))?;
    Ok(Some(hash))
}

async fn run(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.to_path_buf());
    let origin = config::public_origin(&config.server.public_url)?;

    // Check the network before anything else exists: no database, no listener.
    let lnd: Arc<dyn Lightning> = Arc::new(LndRest::new(&config.lnd, &credentials_dir)?);
    let (info, network) = lnd::require_test_network(lnd.as_ref(), config.lnd.expected_network.as_deref()).await?;
    if !info.synced_to_chain {
        warn!("LND is not synced to the chain yet");
    }
    info!(%network, alias = %info.alias, node = %info.identity_pubkey, "LND is on a test network");

    let db = Db::open(&config.server.database_path).await?;
    let admin_hash = admin_hash(&config, &credentials_dir)?;
    let wallet = Arc::new(Wallet::new(db, lnd, network, &config, origin));
    let app = Arc::new(App::new(Arc::clone(&wallet), &config, admin_hash)?);
    app.blocks
        .reload(&wallet.db)
        .await
        .context("cannot load operator blocks")?;

    tokio::spawn(Arc::clone(&wallet).follow_invoices());
    tokio::spawn(Arc::clone(&wallet).reconcile_forever());
    tokio::spawn(web::maintain_protections(Arc::clone(&app)));

    if let Some(address) = config.server.metrics_address {
        let listener = TcpListener::bind(address)
            .await
            .context("cannot bind the metrics listener")?;
        let router = web::metrics_router(Arc::clone(&app));
        info!(%address, "metrics listener ready");
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(%error, "metrics listener stopped");
            }
        });
    }

    let listener = TcpListener::bind(config.server.bind_address)
        .await
        .context("cannot bind the HTTP listener")?;
    info!(address = %listener.local_addr()?, domain = %wallet.domain, "satchel is listening");
    axum::serve(
        listener,
        web::router(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_json);
    if let Some(Command::HashPassword) = cli.command {
        return hash_password_from_stdin();
    }
    let config = cli.config.context("--config (or SATCHEL_CONFIG) is required")?;
    run(&config).await
}

async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
