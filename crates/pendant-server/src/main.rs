use std::path::PathBuf;

use clap::Parser;
use pendant_server::config::Overrides;
use pendant_server::errors::{Error, Result, ResultExt};
use pendant_server::{Config, relay, replica};

#[derive(Debug, clap::Parser)]
#[clap(version, about = "pendant cloud server: relay + replica")]
struct Cli {
    /// TOML config file. Flags below override it.
    #[clap(long)]
    config: Option<PathBuf>,
    /// Plain HTTP on 127.0.0.1:3340, no TLS, replica on: LAN development.
    #[clap(long)]
    dev: bool,
    /// Relay HTTP listener.
    #[clap(long)]
    listen: Option<std::net::SocketAddr>,
    /// Relay QUIC (address discovery) listener; needs TLS.
    #[clap(long)]
    quic_listen: Option<std::net::SocketAddr>,
    /// URL devices reach the relay at.
    #[clap(long)]
    public_url: Option<String>,
    /// Accepted workspace token (repeatable).
    #[clap(long = "token")]
    tokens: Vec<String>,
    /// Replica store file.
    #[clap(long)]
    replica_db: Option<PathBuf>,
    /// Relay only, no replica.
    #[clap(long)]
    no_replica: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(
        cli.config.as_deref(),
        Overrides {
            dev: cli.dev,
            http_listen: cli.listen,
            quic_listen: cli.quic_listen,
            public_url: cli.public_url,
            tokens: cli.tokens,
            replica_db: cli.replica_db,
            no_replica: cli.no_replica,
        },
    )?;

    let server = relay::spawn(config.relay.clone(), config.tokens.clone()).await?;
    tracing::info!(url = %config.public_url, http = ?server.http_addr(), https = ?server.https_addr(), quic = ?server.quic_addr(), "relay up");

    let replica = match &config.replica {
        Some(cfg) => {
            let node =
                replica::start(cfg, config.public_url.clone(), config.tokens.clone()).await?;
            tracing::info!(id = %node.id(), db = %cfg.db.display(), "replica up");
            println!("relay:   {}", config.public_url);
            println!("replica: {}", node.id());
            println!("token:   {}", config.tokens[0]);
            Some(node)
        }
        None => None,
    };

    tokio::signal::ctrl_c()
        .await
        .change_context(Error)
        .attach("waiting for ctrl-c")?;
    tracing::info!("shutting down");
    if let Some(node) = replica {
        node.shutdown()
            .await
            .change_context(Error)
            .attach("replica shutdown")?;
    }
    server
        .shutdown()
        .await
        .change_context(Error)
        .attach("relay shutdown")?;
    Ok(())
}
