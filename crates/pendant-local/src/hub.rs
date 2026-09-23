//! The hub: local doc store, peer registry and the fan-out between peers.
//! Locks are only ever taken one at a time and never held across an await.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use iroh::EndpointId;
use pendant_core::{DeviceId, DocKey, DocProvider, ServerMsg};
use tokio::sync::{mpsc, watch};

use crate::docs::ServerDocs;
use crate::peers::{PeerState, PeerStatus, Route};

/// What an outbound dial loop is told to do by the hub.
pub(crate) enum OutCmd {
    Update {
        doc: DocKey,
        payload: Vec<u8>,
    },
    Ephemeral {
        doc: DocKey,
        payload: Vec<u8>,
    },
    /// A doc appeared locally; subscribe to it on this peer too.
    Subscribe(DocKey),
    /// Tell the peer we are unpairing, then end the dial loop for good.
    Unpair,
    Close,
}

pub(crate) enum PeerSink {
    /// Served with a `ServerSession`: raw `ServerMsg` frames per lane.
    Inbound {
        docs: mpsc::UnboundedSender<Vec<u8>>,
        eph: mpsc::UnboundedSender<Vec<u8>>,
        subscribed: HashSet<DocKey>,
    },
    /// Driven with a `ClientSession` in its dial loop.
    Outbound { cmd: mpsc::UnboundedSender<OutCmd> },
}

pub(crate) struct PeerEntry {
    pub sink: PeerSink,
    pub status: PeerStatus,
}

#[derive(Default)]
pub(crate) struct Registry {
    next_id: u64,
    pub peers: HashMap<u64, PeerEntry>,
}

pub(crate) struct Hub {
    pub device: DeviceId,
    pub docs: Mutex<ServerDocs>,
    pub peers: Mutex<Registry>,
    tokens: RwLock<Vec<String>>,
    /// Direct address hints learned out of band (mDNS), by peer.
    pub hints: Mutex<HashMap<EndpointId, BTreeSet<SocketAddr>>>,
    status: watch::Sender<Vec<PeerStatus>>,
    /// A peer unpaired from us: (its endpoint, its device id). The app
    /// listens and clears the pairing if it matches.
    unpaired: tokio::sync::broadcast::Sender<Unpaired>,
}

/// A peer told us it unpaired from us.
#[derive(Debug, Clone, Copy)]
pub struct Unpaired {
    pub endpoint: Option<EndpointId>,
    pub device: DeviceId,
}

impl Hub {
    pub fn new(device: DeviceId, docs: ServerDocs, tokens: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            device,
            docs: Mutex::new(docs),
            peers: Mutex::new(Registry::default()),
            tokens: RwLock::new(tokens),
            hints: Mutex::new(HashMap::new()),
            status: watch::Sender::new(Vec::new()),
            unpaired: tokio::sync::broadcast::channel(16).0,
        })
    }

    pub fn tokens(&self) -> Vec<String> {
        self.tokens.read().expect("token list poisoned").clone()
    }

    /// Accept `token` in `Hello` from now on (no-op if known or empty).
    pub fn add_token(&self, token: String) {
        let mut tokens = self.tokens.write().expect("token list poisoned");
        if !token.is_empty() && !tokens.contains(&token) {
            tokens.push(token);
        }
    }

    /// Stop accepting `token` (after leaving a workspace). Existing
    /// connections are the caller's to drop.
    pub fn remove_token(&self, token: &str) {
        self.tokens
            .write()
            .expect("token list poisoned")
            .retain(|t| t != token);
    }

    pub fn watch_status(&self) -> watch::Receiver<Vec<PeerStatus>> {
        self.status.subscribe()
    }

    pub fn statuses(&self) -> Vec<PeerStatus> {
        self.status.borrow().clone()
    }

    /// Stream of peers that unpaired from us.
    pub fn watch_unpaired(&self) -> tokio::sync::broadcast::Receiver<Unpaired> {
        self.unpaired.subscribe()
    }

    /// A peer unpaired from us (told over the wire): tell the app so it can
    /// forget the pairing.
    pub fn notify_unpaired(&self, endpoint: Option<EndpointId>, device: DeviceId) {
        let _ = self.unpaired.send(Unpaired { endpoint, device });
    }

    /// Look up which registered peers claim `device` (inbound and outbound).
    /// Returns `(peer_id, endpoint, is_inbound)`.
    pub fn peers_for_device(&self, device: DeviceId) -> Vec<(u64, Option<EndpointId>, bool)> {
        let peers = self.peers.lock().expect("peer registry poisoned");
        peers
            .peers
            .iter()
            .filter(|(_, p)| p.status.device == Some(device))
            .map(|(id, p)| {
                let inbound = matches!(p.sink, PeerSink::Inbound { .. });
                (*id, p.status.id, inbound)
            })
            .collect()
    }

    /// Send an unpair notice to a peer: an inbound peer gets a raw
    /// `ServerMsg::Unpair` on its docs lane; an outbound dial loop is told
    /// to send `ClientMsg::Unpair` and stop. Returns the peer's endpoint.
    pub fn send_unpair(&self, peer_id: u64) -> Option<EndpointId> {
        let peers = self.peers.lock().expect("peer registry poisoned");
        let peer = peers.peers.get(&peer_id)?;
        let endpoint = peer.status.id;
        match &peer.sink {
            PeerSink::Inbound { docs, .. } => {
                if let Ok(frame) = (ServerMsg::Unpair {
                    device: self.device,
                })
                .encode()
                {
                    let _ = docs.send(frame);
                }
            }
            PeerSink::Outbound { cmd } => {
                let _ = cmd.send(OutCmd::Unpair);
            }
        }
        endpoint
    }

    fn publish(&self, peers: &Registry) {
        let mut list: Vec<(u64, PeerStatus)> = peers
            .peers
            .iter()
            .map(|(id, p)| (*id, p.status.clone()))
            .collect();
        list.sort_by_key(|(id, _)| *id);
        self.status
            .send_replace(list.into_iter().map(|(_, s)| s).collect());
    }

    pub fn register(&self, sink: PeerSink, status: PeerStatus) -> u64 {
        let mut peers = self.peers.lock().expect("peer registry poisoned");
        let id = peers.next_id;
        peers.next_id += 1;
        peers.peers.insert(id, PeerEntry { sink, status });
        self.publish(&peers);
        id
    }

    pub fn deregister(&self, id: u64) {
        let mut peers = self.peers.lock().expect("peer registry poisoned");
        if peers.peers.remove(&id).is_some() {
            self.publish(&peers);
        }
    }

    pub fn update_status(&self, id: u64, f: impl FnOnce(&mut PeerStatus)) {
        let mut peers = self.peers.lock().expect("peer registry poisoned");
        if let Some(peer) = peers.peers.get_mut(&id) {
            f(&mut peer.status);
            self.publish(&peers);
        }
    }

    pub fn set_state(&self, id: u64, state: PeerState) {
        self.update_status(id, |s| s.state = state);
    }

    pub fn set_route(&self, id: u64, route: Option<Route>) {
        self.update_status(id, |s| {
            if let PeerState::Connected { route: current } = &mut s.state {
                *current = route;
            }
        });
    }

    /// Mirror an inbound session's subscriptions so fan-out can route.
    pub fn set_subscriptions(
        &self,
        id: u64,
        subscribed: HashSet<DocKey>,
        device: Option<DeviceId>,
    ) {
        let mut peers = self.peers.lock().expect("peer registry poisoned");
        if let Some(peer) = peers.peers.get_mut(&id) {
            if let PeerSink::Inbound {
                subscribed: subs, ..
            } = &mut peer.sink
            {
                *subs = subscribed;
            }
            if peer.status.device != device {
                peer.status.device = device;
                self.publish(&peers);
            }
        }
    }

    /// Is there a live inbound connection from `remote`? (dial dedupe)
    pub fn has_inbound_from(&self, remote: EndpointId) -> bool {
        let peers = self.peers.lock().expect("peer registry poisoned");
        peers.peers.values().any(|p| {
            p.status.inbound
                && p.status.id == Some(remote)
                && matches!(p.status.state, PeerState::Connected { .. })
        })
    }

    /// Every doc this node holds (disk + open) — what an outbound session
    /// subscribes to on connect.
    pub fn known_docs(&self) -> Vec<DocKey> {
        self.docs.lock().expect("doc registry poisoned").list_docs()
    }

    pub fn doc_version(&self, doc: DocKey) -> Vec<u8> {
        let mut docs = self.docs.lock().expect("doc registry poisoned");
        docs.catch_up(doc, &[])
            .map(|c| c.server_have)
            .unwrap_or_default()
    }

    /// A doc a peer or the app asked for: make every outbound session
    /// subscribe to it as well, so the mesh mirrors it everywhere.
    pub fn want_doc(&self, doc: DocKey) {
        let peers = self.peers.lock().expect("peer registry poisoned");
        for peer in peers.peers.values() {
            if let PeerSink::Outbound { cmd } = &peer.sink {
                let _ = cmd.send(OutCmd::Subscribe(doc));
            }
        }
    }

    /// An update that changed our doc: every other peer hears it. Inbound
    /// peers get a raw `ServerMsg::Update`; outbound sessions forward it as
    /// their own local update (they drop it if not subscribed).
    pub fn fanout_update(&self, origin: u64, doc: DocKey, payload: Vec<u8>, from: DeviceId) {
        let frame = match (ServerMsg::Update {
            doc,
            payload: payload.clone(),
            from,
        })
        .encode()
        {
            Ok(frame) => frame,
            Err(err) => {
                tracing::error!(%err, "encoding update for fan-out");
                return;
            }
        };
        let peers = self.peers.lock().expect("peer registry poisoned");
        for (&id, peer) in &peers.peers {
            if id == origin {
                continue;
            }
            match &peer.sink {
                PeerSink::Inbound {
                    docs, subscribed, ..
                } => {
                    if subscribed.contains(&doc) {
                        let _ = docs.send(frame.clone());
                    }
                }
                PeerSink::Outbound { cmd } => {
                    let _ = cmd.send(OutCmd::Update {
                        doc,
                        payload: payload.clone(),
                    });
                }
            }
        }
    }

    /// Transient payload: relayed to every other peer, never stored.
    pub fn fanout_ephemeral(&self, origin: u64, doc: DocKey, payload: Vec<u8>, from: DeviceId) {
        let frame = match (ServerMsg::Ephemeral {
            doc,
            payload: payload.clone(),
            from,
        })
        .encode()
        {
            Ok(frame) => frame,
            Err(err) => {
                tracing::error!(%err, "encoding ephemeral for fan-out");
                return;
            }
        };
        let peers = self.peers.lock().expect("peer registry poisoned");
        for (&id, peer) in &peers.peers {
            if id == origin {
                continue;
            }
            match &peer.sink {
                PeerSink::Inbound {
                    eph, subscribed, ..
                } => {
                    if subscribed.contains(&doc) {
                        let _ = eph.send(frame.clone());
                    }
                }
                PeerSink::Outbound { cmd } => {
                    let _ = cmd.send(OutCmd::Ephemeral {
                        doc,
                        payload: payload.clone(),
                    });
                }
            }
        }
    }

    pub fn add_hint(&self, id: EndpointId, addr: SocketAddr) -> bool {
        self.hints
            .lock()
            .expect("hints poisoned")
            .entry(id)
            .or_default()
            .insert(addr)
    }

    pub fn hints_for(&self, id: EndpointId) -> Vec<SocketAddr> {
        self.hints
            .lock()
            .expect("hints poisoned")
            .get(&id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }
}
