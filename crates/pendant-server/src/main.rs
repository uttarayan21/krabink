mod errors;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use errors::{Error, Result, ResultExt};
use pendant_core::Store;
use pendant_server::app_state;

/// How often open docs are checkpointed to disk and idle ones unloaded.
const MAINTAIN_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, clap::Parser)]
#[clap(version, about = "pendant sync relay")]
struct Cli {
    /// TOML config file (listen, db, tokens). Flags below override it.
    #[clap(long)]
    config: Option<PathBuf>,
    /// Address to listen on.
    #[clap(long)]
    listen: Option<std::net::SocketAddr>,
    /// redb database file.
    #[clap(long)]
    db: Option<PathBuf>,
    /// Accepted bearer token (repeatable).
    #[clap(long = "token")]
    tokens: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    listen: Option<std::net::SocketAddr>,
    db: Option<PathBuf>,
    #[serde(default)]
    tokens: Vec<String>,
}

struct Config {
    listen: std::net::SocketAddr,
    db: PathBuf,
    tokens: Vec<String>,
}

impl Config {
    fn resolve(cli: Cli) -> Result<Self> {
        let file = match &cli.config {
            Some(path) => {
                let raw = std::fs::read_to_string(path)
                    .change_context(Error)
                    .attach_with(|| format!("reading config {}", path.display()))?;
                toml::from_str::<FileConfig>(&raw)
                    .change_context(Error)
                    .attach("parsing config")?
            }
            None => FileConfig::default(),
        };
        let tokens = if cli.tokens.is_empty() {
            file.tokens
        } else {
            cli.tokens
        };
        if tokens.is_empty() {
            return Err(errors::Report::new(Error)
                .attach("no tokens configured; pass --token or set tokens = [...] in the config"));
        }
        Ok(Self {
            listen: cli
                .listen
                .or(file.listen)
                .unwrap_or_else(|| "127.0.0.1:8722".parse().expect("valid literal")),
            db: cli
                .db
                .or(file.db)
                .unwrap_or_else(|| "pendant-server.redb".into()),
            tokens,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::resolve(Cli::parse())?;
    let store = Store::open(&config.db)
        .change_context(Error)
        .attach_with(|| format!("opening store {}", config.db.display()))?;

    let state = app_state(store, config.tokens);

    let maintenance = {
        let docs = Arc::clone(&state.docs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(MAINTAIN_EVERY);
            loop {
                tick.tick().await;
                let result = docs.lock().expect("doc registry poisoned").maintain(true);
                if let Err(err) = result {
                    tracing::error!(%err, "maintenance failed");
                }
            }
        })
    };

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .change_context(Error)
        .attach_with(|| format!("binding {}", config.listen))?;
    tracing::info!(listen = %config.listen, "pendant-server up");

    axum::serve(listener, pendant_server::router(state.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .change_context(Error)?;

    maintenance.abort();
    // Final durable checkpoint before exit.
    state
        .docs
        .lock()
        .expect("doc registry poisoned")
        .maintain(false)
        .change_context(Error)
        .attach("final checkpoint")?;
    Ok(())
}
