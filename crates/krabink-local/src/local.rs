//! The in-process peer: the app on this device talks to its own node over
//! channels with the very same `ClientSession` it would use over a socket.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::hub::{Hub, PeerSink};
use crate::peers::{PeerKind, PeerState, PeerStatus};
use crate::serve::{PeerIo, serve_peer};

/// App side of the in-process link. Frames are `ClientMsg` bytes going in
/// and `ServerMsg` bytes coming out; lanes are merged (in-process ordering
/// is already perfect).
pub struct LocalLink {
    pub to_node: mpsc::UnboundedSender<Vec<u8>>,
    pub from_node: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl LocalLink {
    pub fn send(&self, frame: Vec<u8>) {
        let _ = self.to_node.send(frame);
    }
}

/// Register the app as an inbound peer and start serving it. Must run on
/// the node's runtime.
pub(crate) fn spawn_local(hub: Arc<Hub>) -> LocalLink {
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let peer_id = hub.register(
        PeerSink::Inbound {
            docs: out_tx.clone(),
            eph: out_tx.clone(),
            subscribed: HashSet::new(),
        },
        PeerStatus {
            id: None,
            kind: PeerKind::Local,
            state: PeerState::Connected { route: None },
            inbound: true,
            device: None,
        },
    );
    tokio::spawn(serve_peer(
        hub,
        peer_id,
        PeerIo {
            incoming: in_rx,
            docs: out_tx.clone(),
            eph: out_tx,
        },
    ));
    LocalLink {
        to_node: in_tx,
        from_node: out_rx,
    }
}
