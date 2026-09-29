//! IndexedDB persistence for the browser build.
//!
//! One database (`krabink`, version 1) holds three object stores with
//! out-of-line string keys and `Uint8Array` values: `kv` for small settings,
//! and `app` / `node` mirroring the two [`krabink_core::Store`]s. The doc
//! stores key snapshots as `s/{key:032x}` and updates as
//! `u/{key:032x}/{seq:016x}`, so a cursor walks them in [`StoreImage`] order.
//!
//! Reads are plain async calls. Writes go through a single ordered writer
//! task that batches everything queued into one readwrite transaction.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use js_sys::{Array, Promise, Uint8Array};
use krabink_core::{DocKey, StoreImage, StoreOp, StoreSink};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DomException, IdbCursorWithValue, IdbDatabase, IdbFactory, IdbKeyRange, IdbObjectStore,
    IdbOpenDbRequest, IdbRequest, IdbTransaction, IdbTransactionMode,
};

/// Database name used by [`Db::open`].
pub const DB_NAME: &str = "krabink";
const DB_VERSION: u32 = 1;
const KV: &str = "kv";
const STORES: [&str; 3] = [KV, Ns::App.store(), Ns::Node.store()];

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("IndexedDB is not available in this context")]
    Unavailable,
    #[error("IndexedDB {op} failed: {msg}")]
    Js { op: &'static str, msg: String },
    #[error("unexpected IndexedDB key {0:?}")]
    BadKey(String),
    #[error("IndexedDB writer is gone")]
    WriterGone,
}

/// Which doc store an op or preload targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ns {
    /// The app's own library.
    App,
    /// The embedded sync node's replica.
    Node,
}

impl Ns {
    const fn store(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Node => "node",
        }
    }
}

/// Last write failure seen by the writer, for surfacing in the UI. Writes
/// are fire-and-forget, so this is the only place their errors land.
#[derive(Debug, Clone, Default)]
pub struct PersistHealth(Arc<Mutex<Option<String>>>);

impl PersistHealth {
    /// The most recent write error since open, if any.
    pub fn last_error(&self) -> Option<String> {
        self.lock().clone()
    }

    fn record(&self, err: &Error) {
        tracing::warn!("persistence: {err}");
        *self.lock() = Some(err.to_string());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

enum Msg {
    Store { ns: Ns, op: StoreOp },
    Kv { key: String, value: Vec<u8> },
    Flush(oneshot::Sender<Result<(), Error>>),
}

/// An open connection plus its ordered writer. Cheap to clone; the
/// connection closes once every clone and sink is dropped.
#[derive(Clone)]
pub struct Db {
    db: IdbDatabase,
    tx: mpsc::UnboundedSender<Msg>,
    health: PersistHealth,
}

impl Db {
    /// Open (or create) the `krabink` database.
    pub async fn open() -> Result<Self, Error> {
        Self::open_named(DB_NAME).await
    }

    /// Open (or create) a database by name; tests use throwaway names.
    pub async fn open_named(name: &str) -> Result<Self, Error> {
        let req = factory()?
            .open_with_u32(name, DB_VERSION)
            .map_err(js("open"))?;
        let upgrade = Closure::<dyn FnMut()>::new({
            let req = req.clone();
            move || {
                if let Err(err) = create_stores(&req) {
                    tracing::warn!("IndexedDB upgrade failed: {}", describe(&err));
                    if let Some(txn) = req.transaction() {
                        let _ = txn.abort();
                    }
                }
            }
        });
        req.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
        let result = request(&req, "open").await;
        req.set_onupgradeneeded(None);
        drop(upgrade);
        let db: IdbDatabase = result?.unchecked_into();

        let (tx, rx) = mpsc::unbounded();
        let health = PersistHealth::default();
        wasm_bindgen_futures::spawn_local(writer(db.clone(), rx, health.clone()));
        Ok(Self { db, tx, health })
    }

    /// Delete a database by name. Waits for open connections to close.
    pub async fn delete_named(name: &str) -> Result<(), Error> {
        let req = factory()?.delete_database(name).map_err(js("delete"))?;
        request(&req, "delete").await.map(drop)
    }

    /// Everything stored for `ns`, read with one cursor.
    pub async fn preload(&self, ns: Ns) -> Result<StoreImage, Error> {
        let txn = self
            .db
            .transaction_with_str_and_mode(ns.store(), IdbTransactionMode::Readonly)
            .map_err(js("preload"))?;
        let req = txn
            .object_store(ns.store())
            .and_then(|store| store.open_cursor())
            .map_err(js("preload"))?;
        let image = Rc::new(RefCell::new(Ok(StoreImage::default())));
        let step = Closure::<dyn FnMut()>::new({
            let (req, image) = (req.clone(), image.clone());
            move || {
                let mut image = image.borrow_mut();
                let Ok(acc) = image.as_mut() else { return };
                if let Err(err) = cursor_step(&req, acc) {
                    *image = Err(err);
                }
            }
        });
        req.set_onsuccess(Some(step.as_ref().unchecked_ref()));
        let done = settled(&txn, "preload").await;
        drop(step);
        done?;
        image.replace(Ok(StoreImage::default()))
    }

    /// A sink that mirrors a memory store's ops into `ns`.
    pub fn sink(&self, ns: Ns) -> Arc<dyn StoreSink> {
        Arc::new(Sink {
            ns,
            tx: self.tx.clone(),
            health: self.health.clone(),
        })
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let req = self
            .db
            .transaction_with_str_and_mode(KV, IdbTransactionMode::Readonly)
            .and_then(|txn| txn.object_store(KV))
            .and_then(|store| store.get(&JsValue::from_str(key)))
            .map_err(js("kv_get"))?;
        let value = request(&req, "kv_get").await?;
        Ok((!value.is_undefined()).then(|| Uint8Array::new(&value).to_vec()))
    }

    /// Queue a kv write behind every earlier write.
    pub fn kv_put(&self, key: &str, value: Vec<u8>) {
        send(
            &self.tx,
            &self.health,
            Msg::Kv {
                key: key.to_owned(),
                value,
            },
        );
    }

    /// Resolves once everything queued so far is committed.
    pub async fn flush(&self) -> Result<(), Error> {
        let (done, wait) = oneshot::channel();
        self.tx
            .unbounded_send(Msg::Flush(done))
            .map_err(|_| Error::WriterGone)?;
        wait.await.map_err(|_| Error::WriterGone)?
    }

    pub fn health(&self) -> PersistHealth {
        self.health.clone()
    }
}

struct Sink {
    ns: Ns,
    tx: mpsc::UnboundedSender<Msg>,
    health: PersistHealth,
}

impl StoreSink for Sink {
    fn apply(&self, op: StoreOp) {
        send(&self.tx, &self.health, Msg::Store { ns: self.ns, op });
    }
}

fn send(tx: &mpsc::UnboundedSender<Msg>, health: &PersistHealth, msg: Msg) {
    if tx.unbounded_send(msg).is_err() {
        health.record(&Error::WriterGone);
    }
}

/// Takes one message, drains whatever else is queued, commits the writes in
/// one transaction, then answers the flushes. Runs until every sender drops.
async fn writer(db: IdbDatabase, mut rx: mpsc::UnboundedReceiver<Msg>, health: PersistHealth) {
    while let Some(first) = rx.next().await {
        let mut writes = Vec::new();
        let mut flushes = Vec::new();
        let mut batch = vec![first];
        while let Ok(msg) = rx.try_recv() {
            batch.push(msg);
        }
        for msg in batch {
            match msg {
                Msg::Flush(done) => flushes.push(done),
                write => writes.push(write),
            }
        }
        let result = match writes.is_empty() {
            true => Ok(()),
            false => commit(&db, &writes).await,
        };
        if let Err(err) = &result {
            health.record(err);
        }
        for done in flushes {
            let _ = done.send(result.clone());
        }
    }
    db.close();
}

async fn commit(db: &IdbDatabase, writes: &[Msg]) -> Result<(), Error> {
    let names: Array = STORES
        .into_iter()
        .filter(|name| writes.iter().any(|msg| target(msg) == Some(*name)))
        .map(JsValue::from_str)
        .collect();
    let txn = db
        .transaction_with_str_sequence_and_mode(&names, IdbTransactionMode::Readwrite)
        .map_err(js("transaction"))?;
    // Issue every request synchronously: awaiting anything else here would
    // let the transaction auto-commit early.
    if let Err(err) = writes.iter().try_for_each(|msg| issue(&txn, msg)) {
        let _ = txn.abort();
        return Err(js("write")(err));
    }
    settled(&txn, "commit").await
}

fn target(msg: &Msg) -> Option<&'static str> {
    match msg {
        Msg::Store { ns, .. } => Some(ns.store()),
        Msg::Kv { .. } => Some(KV),
        Msg::Flush(_) => None,
    }
}

fn issue(txn: &IdbTransaction, msg: &Msg) -> Result<(), JsValue> {
    let Some(name) = target(msg) else {
        return Ok(());
    };
    let store = txn.object_store(name)?;
    match msg {
        Msg::Kv { key, value } => put(&store, key, value),
        Msg::Store {
            op: StoreOp::Append {
                key, seq, bytes, ..
            },
            ..
        } => put(&store, &update_key(*key, *seq), bytes),
        Msg::Store {
            op: StoreOp::Checkpoint { key, snapshot },
            ..
        } => {
            // '/' < hex digits < '~', so this covers exactly key's log.
            let prefix = format!("u/{:032x}/", key.to_u128());
            let range = IdbKeyRange::bound(
                &JsValue::from_str(&prefix),
                &JsValue::from_str(&format!("{prefix}~")),
            )?;
            store.delete(&range)?;
            put(&store, &snapshot_key(*key), snapshot)
        }
        Msg::Flush(_) => Ok(()),
    }
}

fn put(store: &IdbObjectStore, key: &str, bytes: &[u8]) -> Result<(), JsValue> {
    store
        .put_with_key(&Uint8Array::from(bytes), &JsValue::from_str(key))
        .map(drop)
}

fn snapshot_key(key: DocKey) -> String {
    format!("s/{:032x}", key.to_u128())
}

fn update_key(key: DocKey, seq: u64) -> String {
    format!("u/{:032x}/{seq:016x}", key.to_u128())
}

/// Folds the cursor's current record into `image` and advances it.
fn cursor_step(req: &IdbRequest, image: &mut StoreImage) -> Result<(), Error> {
    let result = req.result().map_err(js("preload"))?;
    if result.is_null() {
        return Ok(());
    }
    let cursor: IdbCursorWithValue = result.unchecked_into();
    let key = cursor
        .key()
        .map_err(js("preload"))?
        .as_string()
        .ok_or_else(|| Error::BadKey("<non-string>".into()))?;
    let bytes = Uint8Array::new(&cursor.value().map_err(js("preload"))?).to_vec();
    match parse_key(&key).ok_or(Error::BadKey(key))? {
        (doc, None) => image.snapshots.push((doc, bytes)),
        (doc, Some(seq)) => image.updates.push((doc, seq, bytes)),
    }
    cursor.continue_().map_err(js("preload"))
}

/// `s/{key}` or `u/{key}/{seq}` back to its parts.
fn parse_key(raw: &str) -> Option<(DocKey, Option<u64>)> {
    let hex_u128 = |s: &str| {
        let mut buf = [0u8; 16];
        hex::decode_to_slice(s, &mut buf).ok()?;
        Some(DocKey::from_u128(u128::from_be_bytes(buf)))
    };
    let hex_u64 = |s: &str| {
        let mut buf = [0u8; 8];
        hex::decode_to_slice(s, &mut buf).ok()?;
        Some(u64::from_be_bytes(buf))
    };
    match raw.split('/').collect::<Vec<_>>()[..] {
        ["s", key] => Some((hex_u128(key)?, None)),
        ["u", key, seq] => Some((hex_u128(key)?, Some(hex_u64(seq)?))),
        _ => None,
    }
}

fn factory() -> Result<IdbFactory, Error> {
    js_sys::Reflect::get(&js_sys::global(), &"indexedDB".into())
        .ok()
        .and_then(|v| v.dyn_into::<IdbFactory>().ok())
        .ok_or(Error::Unavailable)
}

fn create_stores(req: &IdbOpenDbRequest) -> Result<(), JsValue> {
    let db: IdbDatabase = req.result()?.unchecked_into();
    let existing = db.object_store_names();
    STORES
        .into_iter()
        .filter(|name| !existing.contains(name))
        .try_for_each(|name| db.create_object_store(name).map(drop))
}

/// Resolves with the request's result on success, its error otherwise.
async fn request(req: &IdbRequest, op: &'static str) -> Result<JsValue, Error> {
    let promise = Promise::new(&mut |resolve, reject| {
        req.set_onsuccess(Some(&resolve));
        req.set_onerror(Some(&reject));
    });
    let outcome = JsFuture::from(promise).await;
    req.set_onsuccess(None);
    req.set_onerror(None);
    match outcome {
        Ok(_) => req.result().map_err(js(op)),
        Err(_) => Err(Error::Js {
            op,
            msg: match req.error() {
                Ok(Some(err)) => describe(&err),
                _ => "unknown error".into(),
            },
        }),
    }
}

/// Resolves when the transaction commits; errors if it aborts.
async fn settled(txn: &IdbTransaction, op: &'static str) -> Result<(), Error> {
    let promise = Promise::new(&mut |resolve, reject| {
        txn.set_oncomplete(Some(&resolve));
        txn.set_onabort(Some(&reject));
    });
    let outcome = JsFuture::from(promise).await;
    txn.set_oncomplete(None);
    txn.set_onabort(None);
    outcome.map(drop).map_err(|_| Error::Js {
        op,
        msg: txn
            .error()
            .map_or_else(|| "transaction aborted".into(), |err| describe(&err)),
    })
}

fn js(op: &'static str) -> impl Fn(JsValue) -> Error {
    move |err| Error::Js {
        op,
        msg: describe(&err),
    }
}

fn describe(err: &JsValue) -> String {
    if let Some(dom) = err.dyn_ref::<DomException>() {
        format!("{}: {}", dom.name(), dom.message())
    } else if let Some(err) = err.dyn_ref::<js_sys::Error>() {
        String::from(err.message())
    } else {
        format!("{err:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn keys_round_trip_and_sort() {
        let a = DocKey::from_u128(0xab);
        let b = DocKey::from_u128(u128::MAX);
        assert_eq!(parse_key(&snapshot_key(a)), Some((a, None)));
        assert_eq!(parse_key(&update_key(b, 7)), Some((b, Some(7))));
        assert_eq!(parse_key("x/1"), None);

        // Lexicographic key order equals (doc, seq) order, and the checkpoint
        // range bounds bracket a doc's whole log.
        let mut keys = vec![
            update_key(b, 0),
            update_key(a, 16),
            update_key(a, 2),
            snapshot_key(b),
        ];
        keys.sort();
        assert_eq!(
            keys,
            [
                snapshot_key(b),
                update_key(a, 2),
                update_key(a, 16),
                update_key(b, 0)
            ]
        );
        let prefix = format!("u/{:032x}/", a.to_u128());
        let upper = format!("{prefix}~");
        let max = update_key(a, u64::MAX);
        assert!(prefix < update_key(a, 0) && max < upper);
        assert!(update_key(b, 0) > upper);
    }
}
