//! Native config backend: the data directory and
//! `~/.config/krabink/config.toml`.

use std::io::Write;
use std::path::{Path, PathBuf};

use krabink_core::{DeviceId, PairInfo};

use super::{FileConfig, Identity, RuntimeConfig};
use crate::errors::{Error, Report, Result, ResultExt};

/// Files in the data directory.
pub struct NativePaths {
    pub store_path: PathBuf,
    /// Separate redb for the node's mirror of every doc.
    pub node_store_path: PathBuf,
    pub node_key_path: PathBuf,
    /// Last UDP port the node was bound to; rebound on the next start so
    /// pairings and mDNS hints survive a restart.
    pub node_port_path: PathBuf,
    /// Where the running app takes markdown files a later launch opens.
    pub open_socket_path: PathBuf,
}

impl NativePaths {
    pub fn under(data_dir: &Path) -> Self {
        Self {
            store_path: data_dir.join("krabink.redb"),
            node_store_path: data_dir.join("node.redb"),
            node_key_path: data_dir.join("node_key"),
            node_port_path: data_dir.join("node_port"),
            open_socket_path: data_dir.join("open.sock"),
        }
    }
}

pub fn resolve(
    data_dir: Option<PathBuf>,
    relay: Option<String>,
    token: Option<String>,
) -> Result<RuntimeConfig> {
    let dirs = project_dirs()?;
    let file = read_config(dirs.config_dir())?;
    let data_dir = data_dir.unwrap_or_else(|| dirs.data_dir().to_path_buf());
    std::fs::create_dir_all(&data_dir)
        .change_context(Error)
        .attach_with(|| format!("creating {}", data_dir.display()))?;
    let id = Identity {
        device: load_device_id(&data_dir.join("device_id"))?,
        workspace_token: load_workspace_token(&data_dir)?,
        default_name: default_device_name(),
        paths: NativePaths::under(&data_dir),
    };
    RuntimeConfig::from_file(file, relay, token, id)
}

/// `krabink pair <uri>`: persist the pairing URI to config.toml so the
/// next launch dials it.
pub fn adopt_pair(uri: &str) -> Result<()> {
    let info = PairInfo::parse(uri)
        .ok_or_else(|| Report::new(Error).attach("not a krabink://pair URI"))?;
    let path = super::persist_pair(&info)?;
    writeln!(
        std::io::stdout(),
        "paired with node {} -> {path}",
        info.node
    )
    .change_context(Error)?;
    Ok(())
}

/// The machine's hostname: what a desktop is called until renamed.
pub fn default_device_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "darksailor", "krabink")
        .ok_or_else(|| Report::new(Error).attach("no home directory"))
}

/// Read-modify-write config.toml; returns the path written.
pub(super) fn update_config(edit: impl FnOnce(&mut FileConfig)) -> Result<String> {
    let dirs = project_dirs()?;
    let config_dir = dirs.config_dir();
    std::fs::create_dir_all(config_dir)
        .change_context(Error)
        .attach_with(|| format!("creating {}", config_dir.display()))?;
    let mut file = read_config(config_dir)?;
    edit(&mut file);
    let path = config_dir.join("config.toml");
    let raw = toml::to_string_pretty(&file).change_context(Error)?;
    std::fs::write(&path, raw)
        .change_context(Error)
        .attach_with(|| format!("writing {}", path.display()))?;
    Ok(path.display().to_string())
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

/// The port saved by [`save_node_port`], if any and well-formed.
pub fn load_node_port(path: &Path) -> Option<u16> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
}

/// Remember the bound port for the next start. Best effort: a failure
/// only costs a fresh port next time.
pub fn save_node_port(path: &Path, port: u16) {
    if let Err(err) = std::fs::write(path, format!("{port}\n")) {
        tracing::warn!(%err, path = %path.display(), "saving node port failed");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_port_roundtrips_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node_port");
        assert_eq!(load_node_port(&path), None);
        save_node_port(&path, 38651);
        assert_eq!(load_node_port(&path), Some(38651));
        std::fs::write(&path, "nope\n").unwrap();
        assert_eq!(load_node_port(&path), None);
        std::fs::write(&path, "0\n").unwrap();
        assert_eq!(load_node_port(&path), None);
    }
}
