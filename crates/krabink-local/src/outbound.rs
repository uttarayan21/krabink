//! The dial loop: the single reconnect/backoff state machine. Connects to
//! one peer, drives a [`ClientSession`] over the two lanes, mirrors every
//! doc the peer has into the hub and fans what it learns out to the other
//! peers.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, TransportAddr};
use krabink_core::{
    ClientDocs, ClientEffect, ClientMsg, ClientSession, DeviceId, DocKey, DocProvider, ServerMsg,
};
use tokio::sync::mpsc;

use crate::ALPN;
use crate::framing::{Lane, write_lane};
use crate::hub::{Hub, OutCmd};
use crate::inbound::{current_route, pump_in, pump_out, watch_routes};
use crate::peers::{PeerState, PeerTarget};

const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// iroh races every path itself; this only bounds a peer that is simply
/// not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

enum End {
    /// Told to stop (peer removed, node suspended).
    Closed,
    /// Transport dropped; reconnect with backoff.
    Lost,
    /// Peer rejected us; wait a long while before trying again.
    Fatal(String),
}

pub(crate) async fn dial_loop(
    endpoint: Endpoint,
    hub: Arc<Hub>,
    peer_id: u64,
    target: PeerTarget,
    cmd: mpsc::UnboundedReceiver<OutCmd>,
) {
    dial_until_closed(endpoint, &hub, peer_id, target, cmd).await;
    hub.deregister(peer_id);
}

async fn dial_until_closed(
    endpoint: Endpoint,
    hub: &Arc<Hub>,
    peer_id: u64,
    target: PeerTarget,
    mut cmd: mpsc::UnboundedReceiver<OutCmd>,
) {
    let mut backoff = BACKOFF_START;
    loop {
        hub.set_state(peer_id, PeerState::Connecting);

        // Somebody who already dialled us needs no second connection.
        if hub.has_inbound_from(target.id) {
            if pause(&mut cmd, BACKOFF_MAX).await {
                return;
            }
            continue;
        }

        let mut addrs: Vec<TransportAddr> = target
            .addrs
            .iter()
            .chain(hub.hints_for(target.id).iter())
            .map(|a| TransportAddr::Ip(*a))
            .collect();
        addrs.extend(target.relay.clone().map(TransportAddr::Relay));
        let addr = EndpointAddr::from_parts(target.id, addrs);

        let attempt = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN));
        let conn = tokio::select! {
            result = attempt => match result {
                Ok(Ok(conn)) => Some(conn),
                Ok(Err(err)) => {
                    tracing::info!(peer = %target.id.fmt_short(), %err, "dial failed");
                    None
                }
                Err(_) => {
                    tracing::info!(peer = %target.id.fmt_short(), "dial timed out");
                    None
                }
            },
            next = cmd.recv() => match next {
                None | Some(OutCmd::Close) => return,
                Some(_) => continue, // not connected: dropped, re-derived on catch-up
            },
        };

        let Some(conn) = conn else {
            if pause(&mut cmd, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        };
        backoff = BACKOFF_START;

        let end = connection(conn, hub, peer_id, &target, &mut cmd).await;
        match end {
            End::Closed => return,
            End::Lost => {
                hub.set_state(peer_id, PeerState::Connecting);
                if pause(&mut cmd, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            End::Fatal(message) => {
                tracing::warn!(peer = %target.id.fmt_short(), message, "peer rejected session");
                hub.set_state(peer_id, PeerState::Fatal { message });
                if pause(&mut cmd, BACKOFF_MAX).await {
                    return;
                }
            }
        }
    }
}

/// Sleep unless told to close first. Returns true when closing. A new
/// address hint cuts the wait short: the next dial sees it.
async fn pause(cmd: &mut mpsc::UnboundedReceiver<OutCmd>, wait: Duration) -> bool {
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return false,
            next = cmd.recv() => match next {
                None | Some(OutCmd::Close) => return true,
                Some(OutCmd::Hint) => return false,
                Some(_) => {} // not connected; drop
            },
        }
    }
}

struct Lanes {
    docs: mpsc::UnboundedSender<Vec<u8>>,
    eph: mpsc::UnboundedSender<Vec<u8>>,
}

impl Lanes {
    fn send(&self, msg: &ClientMsg) -> Result<(), End> {
        let frame = msg
            .encode()
            .map_err(|err| End::Fatal(format!("encoding frame: {err}")))?;
        let sent = match Lane::of_client(msg) {
            Lane::Docs => self.docs.send(frame),
            Lane::Ephemeral => self.eph.send(frame),
        };
        sent.map_err(|_| End::Lost)
    }
}

async fn connection(
    conn: Connection,
    hub: &Arc<Hub>,
    peer_id: u64,
    target: &PeerTarget,
    cmd: &mut mpsc::UnboundedReceiver<OutCmd>,
) -> End {
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut pumps = Vec::new();
    let mut lanes = Vec::new();
    for lane in [Lane::Docs, Lane::Ephemeral] {
        let (mut send, recv) = match conn.open_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                tracing::info!(%err, "opening lane failed");
                return End::Lost;
            }
        };
        if let Err(err) = write_lane(&mut send, lane).await {
            tracing::info!(%err, "lane tag write failed");
            return End::Lost;
        }
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        pumps.push(tokio::spawn(pump_in(recv, in_tx.clone())));
        pumps.push(tokio::spawn(pump_out(send, rx)));
        lanes.push(tx);
    }
    drop(in_tx);
    let eph = lanes.pop().expect("two lanes");
    let docs = lanes.pop().expect("two lanes");
    let lanes = Lanes { docs, eph };
    let routes = tokio::spawn(watch_routes(conn.clone(), hub.clone(), peer_id));

    let end = session_loop(&conn, hub, peer_id, target, cmd, &lanes, &mut in_rx).await;

    routes.abort();
    for pump in pumps {
        pump.abort();
    }
    conn.close(0u32.into(), b"bye");
    end
}

async fn session_loop(
    conn: &Connection,
    hub: &Arc<Hub>,
    peer_id: u64,
    target: &PeerTarget,
    cmd: &mut mpsc::UnboundedReceiver<OutCmd>,
    lanes: &Lanes,
    in_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> End {
    let mut session = ClientSession::new(hub.device, target.token.clone());
    let effects = session.connect();
    if let Err(end) = apply(&mut session, hub, peer_id, conn, lanes, effects) {
        return end;
    }

    loop {
        tokio::select! {
            // Frames first: a server Error must be read before the close
            // that follows it, so a rejection reads as Fatal, not Lost.
            biased;
            frame = in_rx.recv() => {
                let Some(frame) = frame else { return End::Lost };
                let msg = match ServerMsg::decode(&frame) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!(%err, "dropping malformed frame");
                        continue;
                    }
                };
                let effects = match msg {
                    // The peer's doc list: mirror everything it has.
                    ServerMsg::DocList { docs } => {
                        let mut effects = Vec::new();
                        for doc in docs {
                            if !session.is_subscribed(doc) {
                                effects.extend(session.subscribe(doc, hub.doc_version(doc)));
                            }
                        }
                        effects
                    }
                    msg => {
                        let from = match &msg {
                            ServerMsg::Update { from, .. } => *from,
                            _ => hub.device,
                        };
                        let mut docs = HubDocs { hub, peer_id, from };
                        session.handle(msg, &mut docs)
                    }
                };
                if let Err(end) = apply(&mut session, hub, peer_id, conn, lanes, effects) {
                    return end;
                }
            }
            next = cmd.recv() => {
                let effects = match next {
                    None | Some(OutCmd::Close) => return End::Closed,
                    Some(OutCmd::Unpair) => {
                        let _ = lanes.send(&session.unpair());
                        return End::Closed;
                    }
                    Some(OutCmd::Update { doc, payload }) => session.local_update(doc, payload),
                    Some(OutCmd::Ephemeral { doc, payload }) => session.ephemeral(doc, payload),
                    Some(OutCmd::Subscribe(doc)) => {
                        if session.is_ready() && !session.is_subscribed(doc) {
                            session.subscribe(doc, hub.doc_version(doc))
                        } else {
                            Vec::new()
                        }
                    }
                    // Already connected; iroh probes new paths itself.
                    Some(OutCmd::Hint) => Vec::new(),
                };
                if let Err(end) = apply(&mut session, hub, peer_id, conn, lanes, effects) {
                    return end;
                }
            }
            _ = conn.closed() => return End::Lost,
        }
    }
}

fn apply(
    session: &mut ClientSession,
    hub: &Arc<Hub>,
    peer_id: u64,
    conn: &Connection,
    lanes: &Lanes,
    effects: Vec<ClientEffect>,
) -> Result<(), End> {
    let mut queue: VecDeque<ClientEffect> = effects.into();
    while let Some(effect) = queue.pop_front() {
        match effect {
            ClientEffect::Send(msg) => lanes.send(&msg)?,
            ClientEffect::Connected => {
                let device = session.server_device();
                hub.update_status(peer_id, |s| {
                    s.state = PeerState::Connected {
                        route: current_route(conn),
                    };
                    s.device = device;
                });
                for doc in hub.known_docs() {
                    queue.extend(session.subscribe(doc, hub.doc_version(doc)));
                }
                // …and whatever the peer has that we don't.
                queue.push_back(ClientEffect::Send(ClientMsg::ListDocs));
            }
            ClientEffect::DocSynced(doc) => tracing::debug!(?doc, "synced"),
            ClientEffect::Ephemeral { doc, payload, from } => {
                hub.fanout_ephemeral(peer_id, doc, payload, from);
            }
            ClientEffect::Fatal(message) => return Err(End::Fatal(message)),
            ClientEffect::Unpaired(device) => {
                let endpoint = hub
                    .peers
                    .lock()
                    .expect("peer registry poisoned")
                    .peers
                    .get(&peer_id)
                    .and_then(|p| p.status.id);
                hub.notify_unpaired(endpoint, device);
                return Err(End::Closed);
            }
        }
    }
    Ok(())
}

/// [`ClientDocs`] over the hub store: imports persist and, when they
/// changed the doc, fan out to every other peer.
struct HubDocs<'a> {
    hub: &'a Arc<Hub>,
    peer_id: u64,
    from: DeviceId,
}

impl ClientDocs for HubDocs<'_> {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> krabink_core::Result<bool> {
        if payload.is_empty() {
            return Ok(false);
        }
        let changed = {
            let mut docs = self.hub.docs.lock().expect("doc registry poisoned");
            docs.import_update(doc, payload)?
        };
        if changed {
            self.hub
                .fanout_update(self.peer_id, doc, payload.to_vec(), self.from);
        }
        Ok(changed)
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> krabink_core::Result<Vec<u8>> {
        let mut docs = self.hub.docs.lock().expect("doc registry poisoned");
        Ok(docs.catch_up(doc, have)?.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hint_cuts_the_backoff_short() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(OutCmd::Hint).unwrap();
        let started = tokio::time::Instant::now();
        assert!(!pause(&mut rx, Duration::from_secs(30)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn close_ends_the_pause_and_updates_do_not() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(OutCmd::Subscribe(krabink_core::DocKey::WORKSPACE))
            .unwrap();
        tx.send(OutCmd::Close).unwrap();
        assert!(pause(&mut rx, Duration::from_secs(30)).await);
    }
}
