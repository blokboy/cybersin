use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;
use cybersin_runtime::{serve_server, ServerConfig};

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Enable Postgres-backed TCP+mTLS multi-worker mode.
    #[arg(long)]
    pub server: bool,
    #[arg(long, default_value = "127.0.0.1:7443", requires = "server")]
    pub listen: SocketAddr,
    #[arg(long, env = "DATABASE_URL", requires = "server")]
    pub database_url: Option<String>,
    #[arg(long, requires = "server")]
    pub tls_cert: Option<PathBuf>,
    #[arg(long, requires = "server")]
    pub tls_key: Option<PathBuf>,
    #[arg(long, requires = "server")]
    pub client_ca: Option<PathBuf>,
    #[arg(long, default_value_t = 4, requires = "server")]
    pub workers: usize,
}

pub async fn execute(args: DaemonArgs) -> anyhow::Result<()> {
    if !args.server {
        anyhow::bail!("daemon currently requires --server");
    }
    let config = ServerConfig {
        listen: args.listen,
        database_url: args
            .database_url
            .ok_or_else(|| anyhow::anyhow!("--database-url or DATABASE_URL is required"))?,
        tls_cert: args
            .tls_cert
            .ok_or_else(|| anyhow::anyhow!("--tls-cert is required"))?,
        tls_key: args
            .tls_key
            .ok_or_else(|| anyhow::anyhow!("--tls-key is required"))?,
        client_ca: args
            .client_ca
            .ok_or_else(|| anyhow::anyhow!("--client-ca is required"))?,
        workers: args.workers,
    };
    eprintln!(
        "cybersin server listening on {} with {} workers",
        config.listen, config.workers
    );
    serve_server(config, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
