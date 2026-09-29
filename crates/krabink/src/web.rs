//! Browser entry: IndexedDB instead of a data directory, the page's origin
//! as the relay (the relay host serves the web build), no sockets, no
//! mDNS. A `#pair=…` link joins a workspace on load.

use bevy::prelude::*;
use krabink_core::{PairInfo, Store};
use krabink_idb::{Db, Ns};
use krabink_local::{Node, NodeOptions, Role, SecretKey};
use wasm_bindgen::prelude::*;

use crate::app::{self, AppInputs};
use crate::config;
use crate::docs::Docs;
use crate::errors::{Error, Report, Result, ResultExt};
use crate::node::SyncNode;
use crate::runtime::Runtime;
use crate::settings::{PairAdopted, Settings};
use crate::sync::{self, SyncTransport};
use crate::theme;

const NODE_KEY: &str = "node_key";

#[wasm_bindgen(inline_js = r#"
export function acquire_instance_lock(name) {
    if (!navigator.locks) return Promise.resolve(true);
    return new Promise((resolve) => {
        navigator.locks.request(name, { ifAvailable: true }, (lock) => {
            resolve(lock !== null);
            // Hold it for the page's lifetime.
            return lock === null ? undefined : new Promise(() => {});
        });
    });
}

export function show_status(text) {
    const el = document.getElementById("status");
    if (el) {
        el.textContent = text;
        el.hidden = false;
    }
}
"#)]
extern "C" {
    fn acquire_instance_lock(name: &str) -> js_sys::Promise;
    fn show_status(text: &str);
}

pub fn main() {
    console_error_panic_hook::set_once();
    // Errors end up as page text, not in a terminal.
    error_stack::Report::set_color_mode(error_stack::fmt::ColorMode::None);
    wasm_bindgen_futures::spawn_local(async {
        match boot().await {
            Ok(mut app) => {
                app.run();
            }
            Err(err) => {
                show_status(&format!("krabink could not start: {err:?}"));
            }
        }
    });
}

async fn boot() -> Result<App> {
    // Two tabs would run the same node key and race on the store.
    let locked = wasm_bindgen_futures::JsFuture::from(acquire_instance_lock("krabink"))
        .await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !locked {
        return Err(Report::new(Error).attach("krabink is already open in another tab"));
    }

    let db = Db::open()
        .await
        .change_context(Error)
        .attach("opening IndexedDB")?;
    let config = config::resolve(&db, query_param("relay"), origin()).await?;
    sync::set_local_device_name(config.device_name.clone());
    // Best effort: ask the browser not to evict our notes under pressure.
    if let Some(window) = web_sys::window()
        && let Ok(storage) = window.navigator().storage().persist()
    {
        drop(wasm_bindgen_futures::JsFuture::from(storage));
    }

    let store = Store::memory_from(
        db.preload(Ns::App)
            .await
            .change_context(Error)
            .attach("loading notes")?,
        Some(db.sink(Ns::App)),
    );
    let docs = Docs::load(store)
        .change_context(Error)
        .attach("loading workspace")?;
    let node_store = Store::memory_from(
        db.preload(Ns::Node)
            .await
            .change_context(Error)
            .attach("loading the node's mirror")?,
        Some(db.sink(Ns::Node)),
    );

    let relay = config
        .relay_target()
        .ok_or_else(|| Report::new(Error).attach("a browser needs a relay"))?;
    let node = Node::start_with(
        node_key(&db).await?,
        node_store,
        NodeOptions {
            device: config.device,
            tokens: [config.workspace_token.clone(), config.token.clone()]
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect(),
            relay: Some(relay),
            bind_port: None,
            role: Role::Device,
        },
    )
    .await
    .change_context(Error)
    .attach("starting sync node")?;
    node.set_peers(config.peer_targets()).await;

    let transport = SyncTransport::new(node.clone(), config.workspace_token.clone());
    let pair = PairInfo {
        node: node.id().to_string(),
        token: config.pair_token(),
        relay: config.relay_url(),
        addrs: Vec::new(),
        replica: config.replica.map(|id| id.to_string()),
    };
    let own_relay = config.relay_url();
    let mut app = app::build(AppInputs {
        docs,
        inbox: crate::import::Inbox::empty(),
        theme: theme::Theme::new(config.theme, config.paper),
        sync_theme: config.sync_theme,
        runtime: Runtime::new(),
        sync_node: SyncNode::new(node),
        transport,
        settings: Settings::new(pair, config.workspace_token.clone(), own_relay),
        // `?follow-latest`: open whichever note changed last, like the
        // desktop's --follow-latest (demos, `krabink replay`).
        follow_latest: query_param("follow-latest").is_some(),
    });
    if let Some(info) = take_pair_link() {
        app.add_systems(Startup, move |mut adopted: MessageWriter<PairAdopted>| {
            adopted.write(PairAdopted(info.clone()));
        });
    }
    Ok(app)
}

/// This browser's node key, created on first run.
async fn node_key(db: &Db) -> Result<SecretKey> {
    let stored = db
        .kv_get(NODE_KEY)
        .await
        .change_context(Error)
        .attach("reading the node key")?
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
    Ok(match stored {
        Some(bytes) => SecretKey::from_bytes(&bytes),
        None => {
            let key = SecretKey::generate();
            db.kv_put(NODE_KEY, key.to_bytes().to_vec());
            key
        }
    })
}

/// The page's origin: the relay the web build is served next to.
fn origin() -> Option<String> {
    web_sys::window()?.location().origin().ok()
}

/// `?name=value` from the page URL (e.g. `?relay=` during development).
fn query_param(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    search
        .trim_start_matches('?')
        .split('&')
        .map(|kv| kv.split_once('=').unwrap_or((kv, "")))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| js_sys::decode_uri_component(value).ok())
        .map(String::from)
}

/// A `#pair=…` web link, stripped from the address bar and history so the
/// token does not linger there.
fn take_pair_link() -> Option<PairInfo> {
    let window = web_sys::window()?;
    let location = window.location();
    let info = PairInfo::from_web_fragment(&location.hash().ok()?)?;
    let clean = format!(
        "{}{}",
        location.pathname().unwrap_or_default(),
        location.search().unwrap_or_default()
    );
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&clean));
    }
    Some(info)
}
