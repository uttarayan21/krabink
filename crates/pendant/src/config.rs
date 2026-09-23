//! Runtime configuration: data directory (stores, node key, device id,
//! workspace token) and the pairing (relay, token, peers) from
//! `~/.config/pendant/config.toml`, overridable from the CLI so several
//! instances can run side by side.

use std::io::Write;
use std::path::{Path, PathBuf};

use pendant_core::{DeviceId, PairInfo};
use pendant_local::{EndpointId, PeerKind, PeerTarget, RelayTarget, RelayUrl};

use crate::errors::{Error, Report, Result, ResultExt};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct FileConfig {
    /// Home relay URL.
    relay: Option<String>,
    /// Workspace token adopted from a pairing URI.
    token: Option<String>,
    /// Endpoint id of the workspace's cloud replica.
    replica: Option<String>,
    /// Nodes this desktop dials.
    #[serde(default)]
    peers: Vec<PeerFile>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PeerFile {
    node: String,
    #[serde(default)]
    addrs: Vec<String>,
}

/// Everything the app needs to start.
pub struct RuntimeConfig {
    pub store_path: PathBuf,
    /// Separate redb for the node's mirror of every doc.
    pub node_store_path: PathBuf,
    pub node_key_path: PathBuf,
    pub device: DeviceId,
    /// Per-install token this node always accepts; what our QR carries
    /// until we adopt a workspace.
    pub workspace_token: String,
    pub relay: Option<RelayUrl>,
    /// Adopted workspace token; empty when none.
    pub token: String,
    pub replica: Option<EndpointId>,
    /// Peers from the config, without the replica.
    pub peers: Vec<PeerTarget>,
}

impl RuntimeConfig {
    pub fn resolve(
        data_dir: Option<PathBuf>,
        relay: Option<String>,
        token: Option<String>,
    ) -> Result<Self> {
        let dirs = directories::ProjectDirs::from("dev", "darksailor", "pendant")
            .ok_or_else(|| Report::new(Error).attach("no home directory"))?;

        let file = read_config(dirs.config_dir())?;
        let data_dir = data_dir.unwrap_or_else(|| dirs.data_dir().to_path_buf());
        std::fs::create_dir_all(&data_dir)
            .change_context(Error)
            .attach_with(|| format!("creating {}", data_dir.display()))?;

        let relay = relay
            .or(file.relay)
            .map(|url| {
                url.parse::<RelayUrl>()
                    .change_context(Error)
                    .attach_with(|| format!("relay url {url:?}"))
            })
            .transpose()?;
        let token = token.or(file.token).unwrap_or_default();
        let replica = file
            .replica
            .map(|id| {
                id.parse::<EndpointId>()
                    .change_context(Error)
                    .attach_with(|| format!("replica id {id:?}"))
            })
            .transpose()?;
        let peers = file
            .peers
            .iter()
            .filter_map(|p| {
                let id = p.node.parse::<EndpointId>().ok()?;
                Some(PeerTarget {
                    id,
                    relay: relay.clone(),
                    addrs: p.addrs.iter().filter_map(|a| a.parse().ok()).collect(),
                    token: token.clone(),
                    kind: PeerKind::Desktop,
                })
            })
            .collect();

        Ok(Self {
            store_path: data_dir.join("pendant.redb"),
            node_store_path: data_dir.join("node.redb"),
            node_key_path: data_dir.join("node_key"),
            device: load_device_id(&data_dir.join("device_id"))?,
            workspace_token: load_workspace_token(&data_dir)?,
            relay,
            token,
            replica,
            peers,
        })
    }

    /// Token other devices use to pair: the adopted workspace token when
    /// there is one, else our own.
    pub fn pair_token(&self) -> String {
        if self.token.is_empty() {
            self.workspace_token.clone()
        } else {
            self.token.clone()
        }
    }

    pub fn relay_target(&self) -> Option<RelayTarget> {
        self.relay.clone().map(|url| RelayTarget {
            url,
            token: self.pair_token(),
        })
    }

    /// Every node to dial: configured peers plus the replica.
    pub fn peer_targets(&self) -> Vec<PeerTarget> {
        let mut peers: Vec<PeerTarget> = self
            .peers
            .iter()
            .cloned()
            .map(|mut p| {
                p.token = self.pair_token();
                p
            })
            .collect();
        if let Some(id) = self.replica {
            peers.push(PeerTarget {
                id,
                relay: self.relay.clone(),
                addrs: Vec::new(),
                token: self.pair_token(),
                kind: PeerKind::Replica,
            });
        }
        peers
    }
}

/// `pendant pair <uri>`: persist the pairing URI to config.toml so the
/// next launch dials it.
pub fn adopt_pair(uri: &str) -> Result<()> {
    let info = PairInfo::parse(uri)
        .ok_or_else(|| Report::new(Error).attach("not a pendant://pair URI"))?;
    let path = persist_pair(&info)?;
    writeln!(
        std::io::stdout(),
        "paired with node {} -> {}",
        info.node,
        path.display()
    )
    .change_context(Error)?;
    Ok(())
}

/// Write the pairing coordinates to config.toml; returns the path written.
/// Shared by the CLI and the in-app join flow.
pub fn persist_pair(info: &PairInfo) -> Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "darksailor", "pendant")
        .ok_or_else(|| Report::new(Error).attach("no home directory"))?;
    let config_dir = dirs.config_dir();
    std::fs::create_dir_all(config_dir)
        .change_context(Error)
        .attach_with(|| format!("creating {}", config_dir.display()))?;
    let path = config_dir.join("config.toml");
    let raw = toml::to_string_pretty(&FileConfig {
        relay: info.relay.clone(),
        token: Some(info.token.clone()),
        replica: info.replica.clone(),
        peers: vec![PeerFile {
            node: info.node.clone(),
            addrs: info.addrs.clone(),
        }],
    })
    .change_context(Error)?;
    std::fs::write(&path, raw)
        .change_context(Error)
        .attach_with(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn read_config(config_dir: &Path) -> Result<FileConfig> {
    let path = config_dir.join("config.toml");
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let raw = std::fs::read_to_string(&path)
        .change_context(Error)
        .attach_with(|| format!("reading {}", path.display()))?;
    match toml::from_str(&raw) {
        Ok(file) => Ok(file),
        Err(err) => {
            // A pre-P2P config (server/fallback keys) is simply stale.
            tracing::warn!(%err, path = %path.display(), "ignoring unreadable config");
            Ok(FileConfig::default())
        }
    }
}

/// Stable per-install random secret, created on first run (migrating the
/// pre-P2P `relay_token` file). A ULID carries 80 random bits, plenty for
/// a bearer token.
fn load_workspace_token(data_dir: &Path) -> Result<String> {
    let path = data_dir.join("workspace_token");
    let legacy = data_dir.join("relay_token");
    for candidate in [&path, &legacy] {
        if let Ok(raw) = std::fs::read_to_string(candidate)
            && !raw.trim().is_empty()
        {
            let token = raw.trim().to_string();
            if candidate != &path {
                let _ = std::fs::write(&path, &token);
            }
            return Ok(token);
        }
    }
    let secret = ulid::Ulid::new().to_string();
    std::fs::write(&path, &secret)
        .change_context(Error)
        .attach_with(|| format!("writing {}", path.display()))?;
    Ok(secret)
}

/// Stable per-install id, created on first run.
fn load_device_id(path: &Path) -> Result<DeviceId> {
    if let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(id) = raw.trim().parse()
    {
        return Ok(id);
    }
    let id = DeviceId::new();
    std::fs::write(path, id.to_string())
        .change_context(Error)
        .attach_with(|| format!("writing {}", path.display()))?;
    Ok(id)
}
