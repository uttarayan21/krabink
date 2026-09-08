//! Runtime configuration: data directory (store, device id) and sync server
//! settings from `~/.config/pendant/config.toml`, both overridable from the
//! CLI so several instances can run side by side.

use std::path::{Path, PathBuf};

use pendant_core::DeviceId;

use crate::errors::{Error, Report, Result, ResultExt};

#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    server: Option<String>,
    token: Option<String>,
}

/// Everything the app needs to start.
pub struct RuntimeConfig {
    pub store_path: PathBuf,
    pub device: DeviceId,
    /// `ws://host:port/ws`; sync is disabled when absent.
    pub server: Option<String>,
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
            device: load_device_id(&data_dir.join("device_id"))?,
            server: server.or(file.server),
            token: token.or(file.token).unwrap_or_default(),
        })
    }
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
