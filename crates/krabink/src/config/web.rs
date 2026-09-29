//! Browser config backend: the same `FileConfig` TOML, the device id and
//! the workspace token, as entries of the IndexedDB `kv` store.

use std::cell::RefCell;

use krabink_core::DeviceId;
use krabink_idb::Db;

use super::{FileConfig, Identity, RuntimeConfig};
use crate::errors::{Error, Report, Result, ResultExt};

const CONFIG_KEY: &str = "config";
const DEVICE_KEY: &str = "device_id";
const TOKEN_KEY: &str = "workspace_token";

thread_local! {
    /// The database and the config as last written: edits are applied here
    /// and queued to IndexedDB (the browser has one thread).
    static STATE: RefCell<Option<(Db, FileConfig)>> = const { RefCell::new(None) };
}

/// Read (or create) this browser's identity and config. `relay` (from the
/// page URL) overrides the stored relay like `--relay` does natively;
/// `origin`, where the page came from, is the fallback: the relay host
/// serves the web build.
pub async fn resolve(
    db: &Db,
    relay: Option<String>,
    origin: Option<String>,
) -> Result<RuntimeConfig> {
    let file = match kv_text(db, CONFIG_KEY).await? {
        Some(raw) => toml::from_str(&raw).unwrap_or_else(|err| {
            tracing::warn!(%err, "ignoring unreadable config");
            FileConfig::default()
        }),
        None => FileConfig::default(),
    };
    let device = match kv_text(db, DEVICE_KEY)
        .await?
        .and_then(|raw| raw.parse().ok())
    {
        Some(id) => id,
        None => {
            let id = DeviceId::new();
            db.kv_put(DEVICE_KEY, id.to_string().into_bytes());
            id
        }
    };
    let workspace_token = match kv_text(db, TOKEN_KEY).await?.filter(|t| !t.is_empty()) {
        Some(token) => token,
        None => {
            // A ULID carries 80 random bits, plenty for a bearer token.
            let token = ulid::Ulid::new().to_string();
            db.kv_put(TOKEN_KEY, token.clone().into_bytes());
            token
        }
    };
    let id = Identity {
        device,
        workspace_token,
        default_name: default_device_name(),
    };
    STATE.with(|state| *state.borrow_mut() = Some((db.clone(), file.clone())));
    let relay = relay.or_else(|| file.relay.clone()).or(origin);
    RuntimeConfig::from_file(file, relay, None, id)
}

/// Edit the config and queue the write; returns where it went.
pub(super) fn update_config(edit: impl FnOnce(&mut FileConfig)) -> Result<String> {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        let (db, file) = state
            .as_mut()
            .ok_or_else(|| Report::new(Error).attach("config not loaded"))?;
        edit(file);
        let raw = toml::to_string_pretty(file).change_context(Error)?;
        db.kv_put(CONFIG_KEY, raw.into_bytes());
        Ok("browser storage".to_string())
    })
}

/// "Firefox on Linux" and the like: what a browser is called until renamed.
pub fn default_device_name() -> String {
    let agent = web_sys::window()
        .and_then(|w| w.navigator().user_agent().ok())
        .unwrap_or_default();
    let browser = [
        ("Firefox/", "Firefox"),
        ("Edg/", "Edge"),
        ("Chrome/", "Chrome"),
        ("Safari/", "Safari"),
    ]
    .into_iter()
    .find(|(tag, _)| agent.contains(tag))
    .map_or("Browser", |(_, name)| name);
    let os = [
        ("Android", "Android"),
        ("iPhone", "iOS"),
        ("iPad", "iPadOS"),
        ("Mac OS X", "macOS"),
        ("Windows", "Windows"),
        ("CrOS", "ChromeOS"),
        ("Linux", "Linux"),
    ]
    .into_iter()
    .find(|(tag, _)| agent.contains(tag))
    .map(|(_, name)| name);
    match os {
        Some(os) => format!("{browser} on {os}"),
        None => browser.to_string(),
    }
}

async fn kv_text(db: &Db, key: &str) -> Result<Option<String>> {
    let bytes = db
        .kv_get(key)
        .await
        .change_context(Error)
        .attach_with(|| format!("reading {key}"))?;
    Ok(bytes.and_then(|b| String::from_utf8(b).ok()))
}
