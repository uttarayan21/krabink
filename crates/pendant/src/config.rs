//! Runtime configuration: data directory (store, device id, relay token)
//! and dedicated-relay settings from `~/.config/pendant/config.toml`, both
//! overridable from the CLI so several instances can run side by side.

use std::io::Write;
use std::path::{Path, PathBuf};

use pendant_core::{DeviceId, PairInfo};

use crate::errors::{Error, Report, Result, ResultExt};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct FileConfig {
    server: Option<String>,
    token: Option<String>,
    fallback: Option<String>,
}

/// Everything the app needs to start.
pub struct RuntimeConfig {
    pub store_path: PathBuf,
    /// Separate redb for the embedded relay's copy of the docs.
    pub relay_store_path: PathBuf,
    pub device: DeviceId,
    /// Per-install token the embedded relay always accepts.
    pub relay_token: String,
    /// Dedicated relay (`ws://host:port/ws`), joined workspace's direct
    /// path, or absent: then this desktop only serves its own relay.
    pub server: Option<String>,
    /// Second remote relay from a joined workspace's pairing URI.
    pub fallback: Option<String>,
    /// Token for the remote relays; empty when none configured.
    pub token: String,
}

impl RuntimeConfig {
    pub fn resolve(
        data_dir: Option<PathBuf>,
        server: Option<String>,
        token: Option<String>,
    ) -> Result<Self> {
        let dirs = directories::ProjectDirs::from("dev", "darksailor", "pendant")
            .ok_or_else(|| Report::new(Error).attach("no home directory"))?;

        let file = read_config(dirs.config_dir())?;
        let data_dir = data_dir.unwrap_or_else(|| dirs.data_dir().to_path_buf());
        std::fs::create_dir_all(&data_dir)
            .change_context(Error)
            .attach_with(|| format!("creating {}", data_dir.display()))?;

        Ok(Self {
            store_path: data_dir.join("pendant.redb"),
            relay_store_path: data_dir.join("relay.redb"),
            device: load_device_id(&data_dir.join("device_id"))?,
            relay_token: load_secret(&data_dir.join("relay_token"))?,
            server: server.or(file.server),
            fallback: file.fallback,
            token: token.or(file.token).unwrap_or_default(),
        })
    }

    /// Every remote relay this desktop connects to, with the shared token.
    pub fn remote_relays(&self) -> Vec<String> {
        self.server
            .iter()
            .chain(self.fallback.iter())
            .cloned()
            .collect()
    }

    /// Token other devices use to pair: the remote relays' token when there
    /// are any (it must open both paths), else the embedded relay's own.
    pub fn pair_token(&self) -> String {
        if self.token.is_empty() {
            self.relay_token.clone()
        } else {
            self.token.clone()
        }
    }
}

/// `pendant pair <uri>`: persist the pairing URI's server + token to
/// config.toml so the next launch syncs against it.
pub fn adopt_pair(uri: &str) -> Result<()> {
    let info = PairInfo::parse(uri)
        .ok_or_else(|| Report::new(Error).attach("not a pendant://pair URI"))?;
    let path = persist_pair(&info)?;
    writeln!(
        std::io::stdout(),
        "paired: {} -> {}",
        info.server,
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
        server: Some(info.server.clone()),
        token: Some(info.token.clone()),
        fallback: info.fallback.clone(),
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
    toml::from_str(&raw)
        .change_context(Error)
        .attach_with(|| format!("parsing {}", path.display()))
}

/// Stable per-install random secret (the embedded relay's token), created
/// on first run. A ULID carries 80 random bits, plenty for a bearer token.
fn load_secret(path: &Path) -> Result<String> {
    if let Ok(raw) = std::fs::read_to_string(path)
        && !raw.trim().is_empty()
    {
        return Ok(raw.trim().to_string());
    }
    let secret = ulid::Ulid::new().to_string();
    std::fs::write(path, &secret)
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
