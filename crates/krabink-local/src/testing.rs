//! Integration-test harness: nodes in a temp dir and an "app" driving a
//! `ClientSession` over the local link, the way desktop and FFI code do.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use krabink_core::{
    ClientDocs, ClientEffect, ClientSession, DeviceId, DocKey, NoteDoc, NoteId, ServerMsg,
};

use crate::{
    LocalLink, Node, NodeConfig, PeerKind, PeerState, PeerTarget, RelayTarget, Role, Route,
};

pub const TOKEN: &str = "test-token";

/// A device node in `dir`; `relay` optional (loopback tests run without).
pub async fn node(
    dir: &tempfile::TempDir,
    name: &str,
    tokens: &[&str],
    relay: Option<RelayTarget>,
) -> Node {
    node_with_role(dir, name, tokens, relay, Role::Device).await
}

pub async fn node_with_role(
    dir: &tempfile::TempDir,
    name: &str,
    tokens: &[&str],
    relay: Option<RelayTarget>,
    role: Role,
) -> Node {
    Node::start(NodeConfig {
        key_path: dir.path().join(format!("{name}.key")),
        store_path: dir.path().join(format!("{name}.redb")),
        device: DeviceId::new(),
        tokens: tokens.iter().map(|t| t.to_string()).collect(),
        relay,
        bind_port: None,
        role,
    })
    .await
    .expect("node starts")
}

/// Dial `node` directly over loopback.
pub async fn loopback_target(node: &Node, token: &str) -> PeerTarget {
    let port = node.bound_port().await.expect("bound");
    PeerTarget {
        id: node.id(),
        relay: None,
        addrs: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))],
        token: token.into(),
        kind: PeerKind::Desktop,
    }
}

/// Dial `node` through its relay only (no direct address hints).
pub fn relay_target(node: &Node, token: &str, kind: PeerKind) -> PeerTarget {
    PeerTarget {
        id: node.id(),
        relay: node.relay().map(|r| r.url),
        addrs: Vec::new(),
        token: token.into(),
        kind,
    }
}

/// Wait until `node`'s outbound peer `peer` is connected; its route.
pub async fn wait_connected(node: &Node, peer: &Node) -> Option<Route> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = node
            .peers()
            .into_iter()
            .find(|p| p.id == Some(peer.id()) && !p.inbound);
        if let Some(status) = status
            && let PeerState::Connected { route } = status.state
        {
            return route;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "peer never connected: {:?}",
            node.peers()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub struct TestDocs(pub HashMap<DocKey, NoteDoc>);

impl ClientDocs for TestDocs {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> krabink_core::Result<bool> {
        self.0[&doc].import_update(payload)
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> krabink_core::Result<Vec<u8>> {
        self.0[&doc].export_updates_since(have)
    }
}

/// The app on a device: a `ClientSession` over the local link plus one
/// note doc.
pub struct App {
    pub link: LocalLink,
    pub session: ClientSession,
    pub docs: TestDocs,
    /// Every `ServerMsg::Update` frame received, for echo accounting.
    pub updates: usize,
}

impl App {
    /// Open the link, handshake, subscribe to `note`.
    pub async fn open(node: &Node, note: NoteId) -> Self {
        let key = DocKey::from(note);
        let mut app = Self {
            link: node.local_link(),
            session: ClientSession::new(node.device(), TOKEN.into()),
            docs: TestDocs(HashMap::from([(key, NoteDoc::new(note))])),
            updates: 0,
        };
        let effects = app.session.connect();
        app.apply(effects);
        app.pump_until(|a| a.session.is_ready()).await;
        let have = app.docs.0[&key].version();
        let effects = app.session.subscribe(key, have);
        app.apply(effects);
        app.pump_until(|a| a.session.is_subscribed(key)).await;
        app
    }

    pub fn apply(&mut self, effects: Vec<ClientEffect>) {
        for effect in effects {
            match effect {
                ClientEffect::Send(msg) => self.link.send(msg.encode().unwrap()),
                ClientEffect::Fatal(err) => panic!("fatal: {err}"),
                _ => {}
            }
        }
    }

    /// Handle one frame if one arrives within `wait`.
    pub async fn pump_one(&mut self, wait: Duration) -> bool {
        match tokio::time::timeout(wait, self.link.from_node.recv()).await {
            Ok(Some(frame)) => {
                let msg = ServerMsg::decode(&frame).unwrap();
                if matches!(msg, ServerMsg::Update { .. }) {
                    self.updates += 1;
                }
                let effects = self.session.handle(msg, &mut self.docs);
                self.apply(effects);
                true
            }
            _ => false,
        }
    }

    /// Pump frames until `done` holds; panics after 10s.
    pub async fn pump_until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for condition"
            );
            self.pump_one(Duration::from_millis(200)).await;
        }
    }

    /// Drain whatever is queued; returns once the link is quiet for a bit.
    pub async fn settle(&mut self) {
        while self.pump_one(Duration::from_millis(300)).await {}
    }

    /// Prepend `text` to the note and push the commit to the node.
    pub fn edit(&mut self, note: NoteId, text: &str) {
        let key = DocKey::from(note);
        let doc = &self.docs.0[&key];
        let before = doc.version();
        doc.splice_text(0, 0, text).unwrap();
        let payload = doc.export_updates_since(&before).unwrap();
        let effects = self.session.local_update(key, payload);
        self.apply(effects);
    }

    pub fn text(&self, note: NoteId) -> String {
        self.docs.0[&DocKey::from(note)].text()
    }
}
