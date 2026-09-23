//! [`Node`]: the endpoint, the hub and the set of peers it keeps dialling.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use iroh::endpoint::presets;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayConfig, RelayMode, RelayUrl, SecretKey, Watcher,
};
use pendant_core::{DeviceId, Store};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

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

/// The home relay: handshake broker and fallback path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTarget {
    pub url: RelayUrl,
    /// Sent as the relay's auth token; the relay admits workspace tokens.
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A desktop or tablet: serves its app over the local link and dials
    /// the peers it was paired with.
    Device,
    /// Headless mirror: accepts everyone with a valid token, dials nobody.
    Replica,
}

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
    pub relay: Option<RelayTarget>,
    /// Pin the UDP port (replicas, so direct addresses survive restarts).
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
    relay_health: watch::Sender<RelayHealth>,
    maintenance: JoinHandle<()>,
}

/// A running sync node. Cheap to clone; every clone is the same node.
#[derive(Clone)]
pub struct Node(Arc<Inner>);

impl Node {
    /// Open the store, load the key, bind the endpoint. Must run on the
    /// tokio runtime that will drive the node.
    pub async fn start(cfg: NodeConfig) -> Result<Self> {
        let secret = crate::identity::load_or_create_secret_key(&cfg.key_path)?;
        let store = Store::open(&cfg.store_path)?;
        let hub = Hub::new(cfg.device, ServerDocs::new(store), cfg.tokens);
        let maintenance = tokio::spawn(maintenance(hub.clone()));
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
            relay_health,
            maintenance,
        }));
        node.resume().await?;
        Ok(node)
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

    /// UDP port the endpoint is bound to, for mDNS; `None` while suspended.
    pub async fn bound_port(&self) -> Option<u16> {
        let net = self.0.net.lock().await;
        net.as_ref()
            .and_then(|n| n.endpoint.bound_sockets().first().map(|a| a.port()))
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

    /// A direct address for `peer` learned out of band (mDNS); used on the
    /// next dial attempt.
    pub fn add_addr_hint(&self, peer: EndpointId, addr: SocketAddr) {
        if self.0.hub.add_hint(peer, addr) {
            tracing::info!(peer = %peer.fmt_short(), %addr, "direct address hint");
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
        if tokio::time::timeout(CLOSE_TIMEOUT, net.endpoint.close())
            .await
            .is_err()
        {
            tracing::warn!("endpoint close timed out");
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
        let mut net = self.0.net.lock().await;
        if net.is_some() {
            return Ok(());
        }
        let endpoint = self.bind().await?;
        tracing::info!(id = %endpoint.id(), "node endpoint bound");
        let accept = tokio::spawn(accept_loop(endpoint.clone(), self.0.hub.clone()));
        let relay_watch = tokio::spawn(watch_relay(endpoint.clone(), self.0.relay_health.clone()));
        let mut fresh = Net {
            endpoint,
            accept,
            relay_watch,
            dials: HashMap::new(),
        };
        let targets = self.0.targets.lock().expect("targets poisoned").clone();
        self.reconcile_dials(&mut fresh, &targets);
        *net = Some(fresh);
        Ok(())
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
        tokio::task::spawn_blocking(move || {
            hub.docs
                .lock()
                .expect("doc registry poisoned")
                .maintain(IdleDocs::Keep)
        })
        .await
        .map_err(|err| Error::Bind(format!("checkpoint task: {err}")))??;
        Ok(())
    }

    async fn bind(&self) -> Result<Endpoint> {
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(self.0.secret.clone())
            .alpns(vec![ALPN.to_vec()]);
        builder = match self.relay() {
            Some(relay) => builder.relay_mode(RelayMode::Custom(
                RelayConfig::new(relay.url, None)
                    .with_auth_token(relay.token)
                    .into(),
            )),
            None => builder.relay_mode(RelayMode::Disabled),
        };
        if let Some(port) = self.0.bind_port {
            builder = builder
                .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
                .map_err(|err| Error::Bind(err.to_string()))?;
        }
        builder
            .bind()
            .await
            .map_err(|err| Error::Bind(err.to_string()))
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
        for target in targets {
            if target.id == self.id() || net.dials.contains_key(&target.id) {
                continue;
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
            let task = tokio::spawn(dial_loop(
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

/// Periodic checkpoint + idle unload; runs until aborted. The checkpoint
/// hits redb, so it runs on the blocking pool.
async fn maintenance(hub: Arc<Hub>) {
    let mut tick = tokio::time::interval(MAINTAIN_EVERY);
    tick.tick().await; // immediate first tick: nothing to do yet
    loop {
        tick.tick().await;
        let hub = hub.clone();
        let result = tokio::task::spawn_blocking(move || {
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
