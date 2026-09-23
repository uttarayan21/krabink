//! TOML config + CLI overrides, resolved into what the relay and replica
//! need to start.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use iroh::RelayUrl;
use serde::Deserialize;

use crate::errors::{Error, Report, Result, ResultExt};

/// Port `--dev` serves plain HTTP on.
pub const DEV_HTTP_PORT: u16 = 3340;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub relay: RelayFile,
    pub replica: ReplicaFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayFile {
    /// Plain-HTTP listener (also serves the ACME challenge with
    /// LetsEncrypt).
    pub http_listen: Option<SocketAddr>,
    /// UDP listener for QUIC address discovery; needs TLS.
    pub quic_listen: Option<SocketAddr>,
    /// The URL devices put in their QR / config. Derived from TLS
    /// hostname or the dev listener when absent.
    pub public_url: Option<String>,
    /// Workspace tokens: gate the relay and the replica's `Hello`.
    pub tokens: Vec<String>,
    pub tls: Option<TlsFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFile {
    pub mode: TlsMode,
    pub https_listen: Option<SocketAddr>,
    /// LetsEncrypt: the certificate hostname.
    pub hostname: Option<String>,
    /// LetsEncrypt: contact email.
    pub contact: Option<String>,
    /// LetsEncrypt: production (true) or staging directory.
    #[serde(default = "default_true")]
    pub prod: bool,
    /// LetsEncrypt: where certificates are cached.
    pub cache_dir: Option<PathBuf>,
    /// Manual: PEM certificate chain.
    pub cert: Option<PathBuf>,
    /// Manual: PEM private key.
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    LetsEncrypt,
    Manual,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReplicaFile {
    /// Default on.
    pub enable: Option<bool>,
    pub db: Option<PathBuf>,
    pub key: Option<PathBuf>,
    /// Pin the replica's UDP port so its direct address survives restarts.
    pub udp_port: Option<u16>,
}

fn default_true() -> bool {
    true
}

/// How the relay listens.
#[derive(Debug, Clone)]
pub struct RelayOpts {
    pub http_listen: SocketAddr,
    pub quic_listen: Option<SocketAddr>,
    pub tls: Option<TlsOpts>,
}

impl RelayOpts {
    /// Plain HTTP on `addr`, no TLS, no QUIC address discovery. LAN and
    /// tests only: without TLS there is no public-address discovery, so
    /// peers behind different NATs stay on the relay path.
    pub fn dev(addr: SocketAddr) -> Self {
        Self {
            http_listen: addr,
            quic_listen: None,
            tls: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TlsOpts {
    LetsEncrypt {
        https_listen: SocketAddr,
        hostname: String,
        contact: String,
        prod: bool,
        cache_dir: PathBuf,
    },
    Manual {
        https_listen: SocketAddr,
        cert: PathBuf,
        key: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct ReplicaConfig {
    pub db: PathBuf,
    pub key: PathBuf,
    pub udp_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub relay: RelayOpts,
    pub public_url: RelayUrl,
    pub tokens: Vec<String>,
    pub replica: Option<ReplicaConfig>,
}

/// CLI overrides, in the same shape the binary parses.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub dev: bool,
    pub http_listen: Option<SocketAddr>,
    pub quic_listen: Option<SocketAddr>,
    pub public_url: Option<String>,
    pub tokens: Vec<String>,
    pub replica_db: Option<PathBuf>,
    pub no_replica: bool,
}

impl Config {
    pub fn load(path: Option<&std::path::Path>, cli: Overrides) -> Result<Self> {
        let file = match path {
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
        Self::resolve(file, cli)
    }

    pub fn resolve(file: FileConfig, cli: Overrides) -> Result<Self> {
        let tokens = if cli.tokens.is_empty() {
            file.relay.tokens
        } else {
            cli.tokens
        };
        if tokens.is_empty() {
            return Err(Report::new(Error)
                .attach("no tokens configured; pass --token or set relay.tokens in the config"));
        }

        let http_listen = cli
            .http_listen
            .or(file.relay.http_listen)
            .unwrap_or_else(|| {
                if cli.dev {
                    (Ipv4Addr::LOCALHOST, DEV_HTTP_PORT).into()
                } else {
                    (Ipv4Addr::UNSPECIFIED, 80).into()
                }
            });
        let tls = if cli.dev {
            None
        } else {
            file.relay.tls.map(resolve_tls).transpose()?
        };
        let quic_listen = match &tls {
            Some(_) => Some(
                cli.quic_listen
                    .or(file.relay.quic_listen)
                    .unwrap_or_else(|| (Ipv4Addr::UNSPECIFIED, 7842).into()),
            ),
            None => None,
        };

        let public_url = match cli.public_url.or(file.relay.public_url) {
            Some(url) => url,
            None => match &tls {
                Some(TlsOpts::LetsEncrypt { hostname, .. }) => format!("https://{hostname}"),
                Some(TlsOpts::Manual { .. }) => {
                    return Err(
                        Report::new(Error).attach("relay.public_url is required with manual TLS")
                    );
                }
                None => format!("http://{http_listen}"),
            },
        };
        let public_url: RelayUrl = public_url
            .parse()
            .change_context(Error)
            .attach_with(|| format!("relay.public_url {public_url:?}"))?;

        let replica = if cli.no_replica || file.replica.enable == Some(false) {
            None
        } else {
            let db = cli
                .replica_db
                .or(file.replica.db)
                .unwrap_or_else(|| "krabink-replica.redb".into());
            let key = file.replica.key.unwrap_or_else(|| {
                let mut key = db.clone();
                key.set_extension("key");
                key
            });
            Some(ReplicaConfig {
                db,
                key,
                udp_port: file.replica.udp_port,
            })
        };

        Ok(Self {
            relay: RelayOpts {
                http_listen,
                quic_listen,
                tls,
            },
            public_url,
            tokens,
            replica,
        })
    }
}

fn resolve_tls(tls: TlsFile) -> Result<TlsOpts> {
    let https_listen = tls
        .https_listen
        .unwrap_or_else(|| (Ipv4Addr::UNSPECIFIED, 443).into());
    match tls.mode {
        TlsMode::LetsEncrypt => Ok(TlsOpts::LetsEncrypt {
            https_listen,
            hostname: tls
                .hostname
                .ok_or_else(|| Report::new(Error).attach("relay.tls.hostname is required"))?,
            contact: tls
                .contact
                .ok_or_else(|| Report::new(Error).attach("relay.tls.contact is required"))?,
            prod: tls.prod,
            cache_dir: tls.cache_dir.unwrap_or_else(|| "acme-cache".into()),
        }),
        TlsMode::Manual => Ok(TlsOpts::Manual {
            https_listen,
            cert: tls
                .cert
                .ok_or_else(|| Report::new(Error).attach("relay.tls.cert is required"))?,
            key: tls
                .key
                .ok_or_else(|| Report::new(Error).attach("relay.tls.key is required"))?,
        }),
    }
}
