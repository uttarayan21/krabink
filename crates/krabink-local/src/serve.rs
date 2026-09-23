//! The inbound protocol loop: one [`ServerSession`] per peer, fed decoded
//! frames from whatever transport (QUIC lanes or the in-process link) and
//! executing its effects against the hub. Transport-agnostic on purpose.

use std::sync::Arc;

use krabink_core::{ClientMsg, ServerEffect, ServerMsg, ServerSession};
use tokio::sync::mpsc;

use crate::framing::Lane;
use crate::hub::Hub;

/// Channels the transport adapter hands the protocol loop.
pub(crate) struct PeerIo {
    /// Frames from the peer, both lanes merged (each is self-describing).
    pub incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    /// Frames to the peer, per lane.
    pub docs: mpsc::UnboundedSender<Vec<u8>>,
    pub eph: mpsc::UnboundedSender<Vec<u8>>,
}

impl PeerIo {
    fn send(&self, msg: &ServerMsg) {
        match msg.encode() {
            Ok(frame) => {
                let _ = match Lane::of_server(msg) {
                    Lane::Docs => self.docs.send(frame),
                    Lane::Ephemeral => self.eph.send(frame),
                };
            }
            Err(err) => tracing::error!(%err, "encode failed"),
        }
    }
}

/// Runs until the peer's incoming channel closes or the session asks to
/// disconnect. Deregisters the peer from the hub on the way out.
pub(crate) async fn serve_peer(hub: Arc<Hub>, peer_id: u64, mut io: PeerIo) {
    let mut session = ServerSession::new(hub.tokens(), hub.device);

    while let Some(frame) = io.incoming.recv().await {
        let msg = match ClientMsg::decode(&frame) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(peer_id, %err, "undecodable frame; closing");
                break;
            }
        };
        let subscribed_doc = match &msg {
            ClientMsg::Subscribe { doc, .. } => Some(*doc),
            _ => None,
        };

        let effects = {
            let mut docs = hub.docs.lock().expect("doc registry poisoned");
            session.handle(msg, &mut *docs)
        };
        hub.set_subscriptions(peer_id, session.subscriptions(), session.device());

        let mut disconnect = false;
        for effect in effects {
            match effect {
                ServerEffect::Send(msg) => io.send(&msg),
                ServerEffect::Broadcast { doc, msg } => match msg {
                    ServerMsg::Update { payload, from, .. } => {
                        hub.fanout_update(peer_id, doc, payload, from);
                    }
                    ServerMsg::Ephemeral { payload, from, .. } => {
                        hub.fanout_ephemeral(peer_id, doc, payload, from);
                    }
                    other => tracing::debug!(?other, "unexpected broadcast"),
                },
                ServerEffect::Disconnect { code, message } => {
                    tracing::info!(peer_id, ?code, message, "disconnecting peer");
                    io.send(&ServerMsg::Error { code, message });
                    disconnect = true;
                }
                ServerEffect::Unpaired { device } => {
                    tracing::info!(peer_id, %device, "peer unpaired from us");
                    let endpoint = hub
                        .peers
                        .lock()
                        .expect("peer registry poisoned")
                        .peers
                        .get(&peer_id)
                        .and_then(|p| p.status.id);
                    hub.notify_unpaired(endpoint, device);
                    disconnect = true;
                }
            }
        }
        if disconnect {
            break;
        }
        // A doc somebody wants from us is a doc the whole mesh should carry.
        if let Some(doc) = subscribed_doc {
            hub.want_doc(doc);
        }
    }

    hub.deregister(peer_id);
}
