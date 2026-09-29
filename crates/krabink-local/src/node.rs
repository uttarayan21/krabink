//! [`Node`]: the endpoint, the hub and the set of peers it keeps dialling.

use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::net::Ipv4Addr;
use std::net::SocketAddr;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use iroh::endpoint::presets;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMode, RelayUrl, SecretKey, Watcher,
};
use krabink_core::{DEFAULT_RELAY, DeviceId, Store};
use n0_future::task::{self, JoinHandle};
use n0_future::time;
use tokio::sync::{mpsc, watch};

use crate::docs::{IdleDocs, ServerDocs};
use crate::hub::{Hub, OutCmd, PeerSink};
use crate::inbound::accept_loop;
use crate::local::{LocalLink, spawn_local};
use crate::outbound::dial_loop;
use crate::peers::{PeerState, PeerStatus, PeerTarget};
use crate::{ALPN, Error, Result};

/// How often open docs are checkpointed to disk and idle ones unloaded.
pub const MAINTAIN_EVERY: Duration = Duration::from_secs(30);
/// `Endpoint::close` waits for peers to acknowledge; never hang a suspend.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// The home relay: handshake broker and fallback path (the only path for a
/// browser node).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTarget {
    pub url: RelayUrl,
    /// Sent as the relay's auth token; a self-hosted relay admits workspace
    /// tokens. `None` for the open default relay, which must never see one.
    pub token: Option<String>,
}

impl RelayTarget {
    /// The open relay every node homes on unless configured otherwise.
    pub fn default_relay() -> Self {
        Self {
            url: DEFAULT_RELAY.parse().expect("DEFAULT_RELAY is a valid url"),
            token: None,
        }
    }

    pub fn is_default(&self) -> bool {
        self.url == Self::default_relay().url
    }

    /// The relay at `url` for a workspace with `token`: self-hosted relays
    /// gate on the token, the default relay gets none.
    pub fn for_workspace(url: RelayUrl, token: &str) -> Self {
        let token = (url != Self::default_relay().url).then(|| token.to_owned());
        Self { url, token }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A desktop or tablet: serves its app over the local link and dials
    /// the peers it was paired with.
    Device,
    /// Headless mirror: accepts everyone with a valid token, dials nobody.
    Replica,
}

/// A node whose key and store live in files.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// ed25519 secret key file (created on first run).
    pub key_path: PathBuf,
    /// redb file holding every doc this node mirrors.
    pub store_path: PathBuf,
    /// CRDT device id used in `Hello` and the device registry.
    pub device: DeviceId,
    /// Tokens accepted in `Hello` from peers.
    pub tokens: Vec<String>,
    /// `None` disables relaying: direct paths (LAN) only.
    pub relay: Option<RelayTarget>,
    /// Pin the UDP port (replicas, so direct addresses survive restarts).
    pub bind_port: Option<u16>,
    pub role: Role,
}

/// Everything but the key and the store, for [`Node::start_with`].
#[derive(Debug, Clone)]
pub struct NodeOptions {
    /// CRDT device id used in `Hello` and the device registry.
    pub device: DeviceId,
    /// Tokens accepted in `Hello` from peers.
    pub tokens: Vec<String>,
    /// `None` disables relaying: direct paths (LAN) only. Browsers have no
    /// direct paths, so there it is an error.
    pub relay: Option<RelayTarget>,
    /// Pin the UDP port (replicas, so direct addresses survive restarts).
    /// Ignored in the browser.
    pub bind_port: Option<u16>,
    pub role: Role,
}

/// Relay connectivity as last reported by the endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelayHealth {
    pub configured: bool,
    pub connected: bool,
    /// Last failure, e.g. an auth denial ("not authorized").
    pub error: Option<String>,
}

struct Dial {
    cmd: mpsc::UnboundedSender<OutCmd>,
    task: JoinHandle<()>,
}

/// Everything that dies with the endpoint on suspend.
struct Net {
    endpoint: Endpoint,
    accept: JoinHandle<()>,
    relay_watch: JoinHandle<()>,
    dials: HashMap<EndpointId, Dial>,
}

struct Inner {
    secret: SecretKey,
    bind_port: Option<u16>,
    role: Role,
    hub: Arc<Hub>,
    relay: Mutex<Option<RelayTarget>>,
    targets: Mutex<Vec<PeerTarget>>,
    net: tokio::sync::Mutex<Option<Net>>,
    /// A close that outlived [`CLOSE_TIMEOUT`], still finishing.
    closing: Mutex<Option<JoinHandle<()>>>,
    relay_health: watch::Sender<RelayHealth>,
    maintenance: JoinHandle<()>,
}

/// A running sync node. Cheap to clone; every clone is the same node.
#[derive(Clone)]
pub struct Node(Arc<Inner>);

// Apps keep the node in a Bevy resource, on every target.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Node>();
};

impl Node {
    /// Load the key, bind the endpoint, then open the store. Must run on
    /// the tokio runtime that will drive the node. Binding first means a
    /// taken port fails before anything else is held open, so the caller
    /// can retry with another port.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn start(cfg: NodeConfig) -> Result<Self> {
        let secret = crate::identity::load_or_create_secret_key(&cfg.key_path)?;
        let endpoint = bind_endpoint(&secret, cfg.relay.clone(), cfg.bind_port).await?;
        let store = match Store::open(&cfg.store_path) {
            Ok(store) => store,
            Err(err) => {
                // Dropping an open endpoint aborts it ungracefully.
                endpoint.close().await;
                return Err(err.into());
            }
        };
        let opts = NodeOptions {
            device: cfg.device,
            tokens: cfg.tokens,
            relay: cfg.relay,
            bind_port: cfg.bind_port,
            role: cfg.role,
        };
        Ok(Self::assemble(secret, store, opts, endpoint).await)
    }

    /// Start with a key and store the caller already holds (the browser
    /// keeps both in IndexedDB).
    pub async fn start_with(secret: SecretKey, store: Store, opts: NodeOptions) -> Result<Self> {
        let endpoint = bind_endpoint(&secret, opts.relay.clone(), opts.bind_port).await?;
        Ok(Self::assemble(secret, store, opts, endpoint).await)
    }

    async fn assemble(
        secret: SecretKey,
        store: Store,
        cfg: NodeOptions,
        endpoint: Endpoint,
    ) -> Self {
        let hub = Hub::new(cfg.device, ServerDocs::new(store), cfg.tokens);
        let maintenance = task::spawn(maintenance(hub.clone()));
        let relay_health = watch::Sender::new(RelayHealth {
            configured: cfg.relay.is_some(),
            ..RelayHealth::default()
        });
        let node = Self(Arc::new(Inner {
            secret,
            bind_port: cfg.bind_port,
            role: cfg.role,
            hub,
            relay: Mutex::new(cfg.relay),
            targets: Mutex::new(Vec::new()),
            net: tokio::sync::Mutex::new(None),
            closing: Mutex::new(None),
            relay_health,
            maintenance,
        }));
        node.attach(endpoint).await;
        node
    }

    pub fn id(&self) -> EndpointId {
        self.0.secret.public()
    }

    pub fn device(&self) -> DeviceId {
        self.0.hub.device
    }

    pub fn role(&self) -> Role {
        self.0.role
    }

    /// Our current address (relay + every direct address the endpoint
    /// knows), for the pairing QR. Empty addrs while suspended.
    pub async fn addr(&self) -> EndpointAddr {
        match &*self.0.net.lock().await {
            Some(net) => net.endpoint.addr(),
            None => EndpointAddr::new(self.id()),
        }
    }

    /// UDP port the endpoint is bound to, for mDNS; `None` while suspended
    /// and always in the browser (no sockets).
    pub async fn bound_port(&self) -> Option<u16> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let net = self.0.net.lock().await;
            net.as_ref()
                .and_then(|n| n.endpoint.bound_sockets().first().map(|a| a.port()))
        }
        #[cfg(target_arch = "wasm32")]
        None
    }

    pub fn relay(&self) -> Option<RelayTarget> {
        self.0.relay.lock().expect("relay poisoned").clone()
    }

    /// The app's in-process link; call once per app. Needs the runtime
    /// (spawns the serving task).
    pub fn local_link(&self) -> LocalLink {
        spawn_local(self.0.hub.clone())
    }

    pub fn add_token(&self, token: String) {
        self.0.hub.add_token(token);
    }

    /// Stop accepting `token` in `Hello`; pair with [`Node::set_peers`] to
    /// drop the connections that used it.
    pub fn remove_token(&self, token: &str) {
        self.0.hub.remove_token(token);
    }

    /// A direct address for `peer` learned out of band (mDNS). A new hint
    /// wakes that peer's dial loop out of its backoff, so a desktop that
    /// came back on a fresh port is reached at once, not after the wait.
    pub fn add_addr_hint(&self, peer: EndpointId, addr: SocketAddr) {
        if self.0.hub.add_hint(peer, addr) {
            tracing::info!(peer = %peer.fmt_short(), %addr, "direct address hint");
            self.0.hub.wake_dial(peer);
        }
    }

    pub fn peers(&self) -> Vec<PeerStatus> {
        self.0.hub.statuses()
    }

    pub fn watch_peers(&self) -> watch::Receiver<Vec<PeerStatus>> {
        self.0.hub.watch_status()
    }

    /// Stream of peers that unpaired from us over the wire. The app clears
    /// its pairing when the endpoint matches.
    pub fn watch_unpaired(&self) -> tokio::sync::broadcast::Receiver<crate::hub::Unpaired> {
        self.0.hub.watch_unpaired()
    }

    /// Unpair from the peer with CRDT `device`: tell it over the wire, drop
    /// the connection, and stop dialing it. The peer forgets us in turn.
    pub async fn unpair(&self, device: DeviceId) {
        let found = self.0.hub.peers_for_device(device);
        let mut endpoints = Vec::new();
        for (peer_id, endpoint, inbound) in found {
            let ep = self.0.hub.send_unpair(peer_id);
            endpoints.extend(ep.or(endpoint));
            if inbound {
                // Inbound peers have no dial to cancel; drop the row so it
                // does not linger after the remote disconnects.
                self.0.hub.deregister(peer_id);
            }
        }
        if endpoints.is_empty() {
            return;
        }
        // Stop dialing the unpaired endpoints and forget them as targets.
        self.0
            .targets
            .lock()
            .expect("targets poisoned")
            .retain(|t| !endpoints.contains(&t.id));
        if let Some(net) = &mut *self.0.net.lock().await {
            for id in &endpoints {
                if let Some(dial) = net.dials.remove(id) {
                    let _ = dial.cmd.send(OutCmd::Close);
                }
            }
        }
    }

    pub fn relay_health(&self) -> RelayHealth {
        self.0.relay_health.borrow().clone()
    }

    pub fn watch_relay(&self) -> watch::Receiver<RelayHealth> {
        self.0.relay_health.subscribe()
    }

    /// Replace the set of peers this node dials. Idempotent: existing
    /// dials to peers still in the list keep their connection.
    pub async fn set_peers(&self, peers: Vec<PeerTarget>) {
        *self.0.targets.lock().expect("targets poisoned") = peers.clone();
        let mut net = self.0.net.lock().await;
        if let Some(net) = net.as_mut() {
            self.reconcile_dials(net, &peers);
        }
    }

    /// Change the home relay; rebinds the endpoint when it is up.
    pub async fn set_relay(&self, relay: Option<RelayTarget>) -> Result<()> {
        let changed = {
            let mut current = self.0.relay.lock().expect("relay poisoned");
            let changed = *current != relay;
            *current = relay;
            changed
        };
        self.0
            .relay_health
            .send_modify(|h| h.configured = self.0.relay.lock().expect("relay poisoned").is_some());
        if changed && self.0.net.lock().await.is_some() {
            self.suspend().await;
            self.resume().await?;
        }
        Ok(())
    }

    /// Close every connection and the endpoint (app went to background).
    /// The local link stays alive; the key and store are kept.
    pub async fn suspend(&self) {
        let Some(net) = self.0.net.lock().await.take() else {
            return;
        };
        for (_, dial) in net.dials {
            let _ = dial.cmd.send(OutCmd::Close);
            dial.task.abort();
        }
        net.accept.abort();
        net.relay_watch.abort();
        // The close runs as its own task: cancelling it half-way leaves
        // the endpoint to abort ungracefully when dropped.
        let endpoint = net.endpoint;
        let mut close = task::spawn(async move { endpoint.close().await });
        if time::timeout(CLOSE_TIMEOUT, &mut close).await.is_err() {
            tracing::warn!("endpoint close timed out; finishing in the background");
            *self.0.closing.lock().expect("closing poisoned") = Some(close);
        }
        // Dial loops are aborted: drop their registry rows ourselves.
        let stale: Vec<u64> = {
            let peers = self.0.hub.peers.lock().expect("peer registry poisoned");
            peers
                .peers
                .iter()
                .filter(|(_, p)| matches!(p.sink, PeerSink::Outbound { .. }))
                .map(|(id, _)| *id)
                .collect()
        };
        for id in stale {
            self.0.hub.deregister(id);
        }
        self.0.relay_health.send_modify(|h| {
            h.connected = false;
        });
    }

    /// Bind a fresh endpoint with the same key and redial every peer.
    pub async fn resume(&self) -> Result<()> {
        if self.0.net.lock().await.is_some() {
            return Ok(());
        }
        // A close still in flight holds the old socket (and a pinned port).
        let closing = self.0.closing.lock().expect("closing poisoned").take();
        if let Some(close) = closing {
            let _ = close.await;
        }
        let endpoint = bind_endpoint(&self.0.secret, self.relay(), self.0.bind_port).await?;
        self.attach(endpoint).await;
        Ok(())
    }

    /// Put a bound endpoint to work: accept, watch the relay, dial peers.
    async fn attach(&self, endpoint: Endpoint) {
        let mut net = self.0.net.lock().await;
        if net.is_some() {
            return;
        }
        tracing::info!(id = %endpoint.id(), "node endpoint bound");
        let accept = task::spawn(accept_loop(endpoint.clone(), self.0.hub.clone()));
        let relay_watch = task::spawn(watch_relay(endpoint.clone(), self.0.relay_health.clone()));
        let mut fresh = Net {
            endpoint,
            accept,
            relay_watch,
            dials: HashMap::new(),
        };
        let targets = self.0.targets.lock().expect("targets poisoned").clone();
        self.reconcile_dials(&mut fresh, &targets);
        *net = Some(fresh);
    }

    /// Tell the endpoint the network changed (interface up/down, Wi-Fi
    /// hop): it re-probes paths instead of waiting for timeouts.
    pub async fn network_changed(&self) {
        if let Some(net) = &*self.0.net.lock().await {
            net.endpoint.network_change().await;
        }
    }

    /// Durable checkpoint of every open doc.
    pub fn checkpoint(&self) -> Result<()> {
        self.0
            .hub
            .docs
            .lock()
            .expect("doc registry poisoned")
            .maintain(IdleDocs::Keep)?;
        Ok(())
    }

    /// Suspend, stop maintenance, final checkpoint.
    pub async fn shutdown(&self) -> Result<()> {
        self.suspend().await;
        self.0.maintenance.abort();
        let hub = self.0.hub.clone();
        run_blocking(move || {
            hub.docs
                .lock()
                .expect("doc registry poisoned")
                .maintain(IdleDocs::Keep)
        })
        .await
        .map_err(|err| Error::Bind(format!("checkpoint task: {err}")))??;
        Ok(())
    }

    fn reconcile_dials(&self, net: &mut Net, targets: &[PeerTarget]) {
        if self.0.role == Role::Replica {
            return; // replicas never dial
        }
        // Peers no longer wanted (or wanted differently) go first.
        let wanted: HashMap<EndpointId, &PeerTarget> = targets.iter().map(|t| (t.id, t)).collect();
        let stale: Vec<EndpointId> = net
            .dials
            .keys()
            .filter(|id| !wanted.contains_key(*id))
            .copied()
            .collect();
        for id in stale {
            if let Some(dial) = net.dials.remove(&id) {
                let _ = dial.cmd.send(OutCmd::Close);
            }
        }
        // Peers paired without a relay share ours: every node on the
        // default relay is reachable through it.
        let fallback = self.relay().map(|r| r.url);
        for target in targets {
            if target.id == self.id() || net.dials.contains_key(&target.id) {
                continue;
            }
            let mut target = target.clone();
            if target.relay.is_none() {
                target.relay.clone_from(&fallback);
            }
            self.0.hub.add_token(target.token.clone());
            let (tx, rx) = mpsc::unbounded_channel();
            let peer_id = self.0.hub.register(
                PeerSink::Outbound { cmd: tx.clone() },
                PeerStatus {
                    id: Some(target.id),
                    kind: target.kind,
                    state: PeerState::Connecting,
                    inbound: false,
                    device: None,
                },
            );
            let task = task::spawn(dial_loop(
                net.endpoint.clone(),
                self.0.hub.clone(),
                peer_id,
                target.clone(),
                rx,
            ));
            net.dials.insert(target.id, Dial { cmd: tx, task });
        }
    }
}

/// The endpoint's relay setup: a token only goes to a relay that asked for
/// one (self-hosted), never to the open default relay.
fn relay_mode(relay: Option<RelayTarget>) -> Result<RelayMode> {
    match relay {
        Some(RelayTarget { url, token }) => {
            let config = RelayConfig::new(url, None);
            let config = match token {
                Some(token) => config.with_auth_token(token),
                None => config,
            };
            Ok(RelayMode::Custom(config.into()))
        }
        None if cfg!(target_arch = "wasm32") => Err(Error::Bind(
            "a browser node needs a relay: it has no direct paths".into(),
        )),
        None => Ok(RelayMode::Disabled),
    }
}

/// Bind an endpoint for `secret` on `bind_port` (or an ephemeral one).
async fn bind_endpoint(
    secret: &SecretKey,
    relay: Option<RelayTarget>,
    bind_port: Option<u16>,
) -> Result<Endpoint> {
    let builder = Endpoint::builder(presets::Minimal)
        .secret_key(secret.clone())
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(relay_mode(relay)?);
    #[cfg(not(target_arch = "wasm32"))]
    let builder = bind_port_on(builder, bind_port)?;
    #[cfg(target_arch = "wasm32")]
    let _ = bind_port;
    builder
        .bind()
        .await
        .map_err(|err| Error::Bind(err.to_string()))
}

#[cfg(not(target_arch = "wasm32"))]
fn bind_port_on(
    mut builder: iroh::endpoint::Builder,
    bind_port: Option<u16>,
) -> Result<iroh::endpoint::Builder> {
    if let Some(port) = bind_port {
        builder = builder
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
            .map_err(|err| Error::Bind(err.to_string()))?;
    }
    Ok(builder)
}

/// Run a store-bound job off the async threads where there are any; the
/// browser has one thread, so it runs inline there.
async fn run_blocking<R: Send + 'static>(
    job: impl FnOnce() -> R + Send + 'static,
) -> Result<R, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::task::spawn_blocking(job)
            .await
            .map_err(|err| err.to_string())
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(job())
    }
}

/// Periodic checkpoint + idle unload; runs until aborted. The checkpoint
/// hits redb, so it runs on the blocking pool.
async fn maintenance(hub: Arc<Hub>) {
    let mut tick = time::interval(MAINTAIN_EVERY);
    tick.tick().await; // immediate first tick: nothing to do yet
    loop {
        tick.tick().await;
        let hub = hub.clone();
        let result = run_blocking(move || {
            hub.docs
                .lock()
                .expect("doc registry poisoned")
                .maintain(IdleDocs::Unload)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::error!(%err, "maintenance failed"),
            Err(err) => tracing::error!(%err, "maintenance task panicked"),
        }
    }
}

async fn watch_relay(endpoint: Endpoint, health: watch::Sender<RelayHealth>) {
    let mut statuses = endpoint.home_relay_status().stream();
    while let Some(list) = statuses.next().await {
        let connected = list.iter().any(|s| s.is_connected());
        let error = list
            .iter()
            .filter_map(|s| s.last_error())
            .next()
            .map(|err| format!("{err:#}"));
        if let Some(err) = &error {
            tracing::warn!(err, "relay");
        }
        health.send_modify(|h| {
            h.connected = connected;
            h.error = error;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_relay_never_gets_the_token() {
        let default = RelayTarget::for_workspace(RelayTarget::default_relay().url, "secret");
        assert_eq!(default, RelayTarget::default_relay());
        assert!(default.is_default());

        let own =
            RelayTarget::for_workspace("https://relay.example.org".parse().unwrap(), "secret");
        assert_eq!(own.token.as_deref(), Some("secret"));
        assert!(!own.is_default());
    }

    #[test]
    fn no_relay_is_lan_only() {
        assert!(matches!(relay_mode(None), Ok(RelayMode::Disabled)));
        assert!(matches!(
            relay_mode(Some(RelayTarget::default_relay())),
            Ok(RelayMode::Custom(_))
        ));
    }
}
