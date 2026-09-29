//! Runtime configuration: identity (device id, workspace token, node key),
//! the pairing (relay, token, peers) and the look. Natively it lives in the
//! data directory and `~/.config/krabink/config.toml`, overridable from the
//! CLI so several instances can run side by side; in the browser it lives
//! in IndexedDB.

use bevy_egui::egui::Color32;
use krabink_core::{DeviceId, PairInfo};
use krabink_local::{EndpointId, PeerKind, PeerTarget, RelayTarget, RelayUrl};

use crate::errors::{Error, Result, ResultExt};
use crate::theme::{self, Flavor};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
pub use native::{
    NativePaths, adopt_pair, default_device_name, load_node_port, resolve, save_node_port,
};
#[cfg(target_arch = "wasm32")]
pub use web::{default_device_name, resolve};

#[cfg(not(target_arch = "wasm32"))]
use native::update_config;
#[cfg(target_arch = "wasm32")]
use web::update_config;

/// The config file's `relay` value that turns relaying off.
const RELAY_OFF: &str = "off";

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct FileConfig {
    /// How this device shows up in every device's registry; hostname
    /// (browser name on the web) when unset.
    device_name: Option<String>,
    /// Home relay URL; `off` for LAN only; the default relay when unset.
    relay: Option<String>,
    /// Workspace token adopted from a pairing URI.
    token: Option<String>,
    /// Endpoint id of the workspace's cloud replica.
    replica: Option<String>,
    /// Catppuccin flavour the UI is drawn in; Mocha when unset.
    theme: Option<Flavor>,
    /// Sketch paper as `#rrggbb`; the flavour's card colour when unset.
    paper: Option<String>,
    /// Follow (and share) the workspace's look; on when unset.
    sync_theme: Option<bool>,
    /// Nodes this device dials.
    #[serde(default)]
    peers: Vec<PeerFile>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PeerFile {
    node: String,
    #[serde(default)]
    addrs: Vec<String>,
}

/// Where the node homes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaySetting {
    /// [`krabink_core::DEFAULT_RELAY`], sent no token.
    Default,
    /// Self-hosted, gated on the workspace token.
    Custom(RelayUrl),
    /// No relay: direct paths (LAN) only.
    Off,
}

impl RelaySetting {
    fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.map(str::trim) {
            None | Some("") => Ok(Self::Default),
            Some(RELAY_OFF) => Ok(Self::Off),
            Some(url) => url
                .parse::<RelayUrl>()
                .map(|url| {
                    if url == RelayTarget::default_relay().url {
                        Self::Default
                    } else {
                        Self::Custom(url)
                    }
                })
                .change_context(Error)
                .attach_with(|| format!("relay url {url:?}")),
        }
    }
}

/// Everything the app needs to start.
pub struct RuntimeConfig {
    /// User-facing name for the synced device registry (and mDNS).
    pub device_name: String,
    #[cfg(not(target_arch = "wasm32"))]
    pub paths: NativePaths,
    pub device: DeviceId,
    /// Per-install token this node always accepts; what our QR carries
    /// until we adopt a workspace.
    pub workspace_token: String,
    pub relay: RelaySetting,
    /// Adopted workspace token; empty when none.
    pub token: String,
    pub replica: Option<EndpointId>,
    /// Peers from the config, without the replica.
    pub peers: Vec<PeerTarget>,
    /// Catppuccin flavour the UI starts in.
    pub theme: Flavor,
    /// Picked sketch paper colour; `None` follows the flavour.
    pub paper: Option<Color32>,
    /// Whether the theme follows the workspace's shared look.
    pub sync_theme: bool,
}

/// What a platform backend read before the shared part of the config.
struct Identity {
    device: DeviceId,
    workspace_token: String,
    default_name: String,
    #[cfg(not(target_arch = "wasm32"))]
    paths: NativePaths,
}

impl RuntimeConfig {
    /// The platform-independent part of resolving: CLI/URL overrides over
    /// the file.
    fn from_file(
        file: FileConfig,
        relay: Option<String>,
        token: Option<String>,
        id: Identity,
    ) -> Result<Self> {
        let relay = RelaySetting::parse(relay.or(file.relay).as_deref())?;
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
                    relay: None, // filled in by `peer_targets`
                    addrs: p.addrs.iter().filter_map(|a| a.parse().ok()).collect(),
                    token: token.clone(),
                    kind: PeerKind::Desktop,
                })
            })
            .collect();

        Ok(Self {
            device_name: file
                .device_name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(id.default_name),
            #[cfg(not(target_arch = "wasm32"))]
            paths: id.paths,
            device: id.device,
            workspace_token: id.workspace_token,
            relay,
            token,
            replica,
            peers,
            theme: file.theme.unwrap_or_default(),
            paper: file.paper.as_deref().and_then(|raw| {
                let paper = theme::parse_hex(raw);
                if paper.is_none() {
                    tracing::warn!(raw, "ignoring unreadable paper colour");
                }
                paper
            }),
            sync_theme: file.sync_theme.unwrap_or(true),
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

    /// The node's relay; `None` when switched off.
    pub fn relay_target(&self) -> Option<RelayTarget> {
        match &self.relay {
            RelaySetting::Default => Some(RelayTarget::default_relay()),
            RelaySetting::Custom(url) => {
                Some(RelayTarget::for_workspace(url.clone(), &self.pair_token()))
            }
            RelaySetting::Off => None,
        }
    }

    /// The relay URL the pairing QR advertises.
    pub fn relay_url(&self) -> Option<String> {
        self.relay_target().map(|r| r.url.to_string())
    }

    /// Every node to dial: configured peers plus the replica.
    pub fn peer_targets(&self) -> Vec<PeerTarget> {
        let relay = self.relay_target().map(|r| r.url);
        let mut peers: Vec<PeerTarget> = self
            .peers
            .iter()
            .cloned()
            .map(|mut p| {
                p.token = self.pair_token();
                p.relay.clone_from(&relay);
                p
            })
            .collect();
        if let Some(id) = self.replica {
            peers.push(PeerTarget {
                id,
                relay,
                addrs: Vec::new(),
                token: self.pair_token(),
                kind: PeerKind::Replica,
            });
        }
        peers
    }
}

/// Write the pairing coordinates to the config (keeping the device name);
/// returns where it was written. Shared by the CLI and the in-app join
/// flow.
pub fn persist_pair(info: &PairInfo) -> Result<String> {
    update_config(|file| {
        // The default relay is implied; storing it would pin its URL.
        file.relay = info
            .relay
            .clone()
            .filter(|url| RelaySetting::parse(Some(url)).ok() != Some(RelaySetting::Default));
        file.token = Some(info.token.clone());
        file.replica = info.replica.clone();
        file.peers = vec![PeerFile {
            node: info.node.clone(),
            addrs: info.addrs.clone(),
        }];
    })
}

/// Forget the adopted workspace: the next launch runs on this device's own
/// token and relay, nothing to dial.
pub fn persist_unpair() -> Result<String> {
    update_config(|file| {
        if file.relay.as_deref() != Some(RELAY_OFF) {
            file.relay = None;
        }
        file.token = None;
        file.replica = None;
        file.peers.clear();
    })
}

/// Remember the user-chosen device name across launches.
pub fn persist_device_name(name: &str) -> Result<String> {
    let name = name.trim();
    update_config(|file| file.device_name = (!name.is_empty()).then(|| name.to_string()))
}

/// Remember the chosen Catppuccin flavour across launches.
pub fn persist_theme(theme: Flavor) -> Result<String> {
    update_config(|file| file.theme = Some(theme))
}

/// Remember the picked paper colour (`None`: follow the flavour).
pub fn persist_paper(paper: Option<Color32>) -> Result<String> {
    update_config(|file| file.paper = paper.map(theme::to_hex))
}

/// Remember a look adopted from the workspace, in one write.
pub fn persist_appearance(flavor: Flavor, paper: Option<Color32>) -> Result<String> {
    update_config(|file| {
        file.theme = Some(flavor);
        file.paper = paper.map(theme::to_hex);
    })
}

/// Remember whether this device syncs its theme with the workspace.
pub fn persist_sync_theme(enabled: bool) -> Result<String> {
    update_config(|file| file.sync_theme = Some(enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_setting_parses() {
        assert_eq!(RelaySetting::parse(None).unwrap(), RelaySetting::Default);
        assert_eq!(RelaySetting::parse(Some("off")).unwrap(), RelaySetting::Off);
        assert_eq!(
            RelaySetting::parse(Some(krabink_core::DEFAULT_RELAY)).unwrap(),
            RelaySetting::Default
        );
        assert!(matches!(
            RelaySetting::parse(Some("https://relay.example.org")).unwrap(),
            RelaySetting::Custom(_)
        ));
        assert!(RelaySetting::parse(Some("not a url")).is_err());
    }

    fn config(relay: Option<&str>) -> RuntimeConfig {
        RuntimeConfig::from_file(
            FileConfig {
                relay: relay.map(Into::into),
                token: Some("workspace".into()),
                ..FileConfig::default()
            },
            None,
            None,
            Identity {
                device: DeviceId::new(),
                workspace_token: "own".into(),
                default_name: "test".into(),
                #[cfg(not(target_arch = "wasm32"))]
                paths: NativePaths::under(std::path::Path::new("/nonexistent")),
            },
        )
        .unwrap()
    }

    #[test]
    fn default_relay_never_sees_the_token() {
        let target = config(None).relay_target().unwrap();
        assert!(target.is_default());
        assert_eq!(target.token, None);

        let target = config(Some("https://relay.example.org"))
            .relay_target()
            .unwrap();
        assert_eq!(target.token.as_deref(), Some("workspace"));

        assert_eq!(config(Some("off")).relay_target(), None);
    }
}
