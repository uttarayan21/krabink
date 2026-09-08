//! Loopback tests: client and server session state machines wired
//! back-to-back through in-memory frame queues (encode/decode exercised on
//! every hop), plus malformed-frame fuzzing.

use std::collections::{HashMap, VecDeque};

use pendant_core::{
    CatchUp, ClientDocs, ClientEffect, ClientMsg, ClientSession, DeviceId, DocKey, DocProvider,
    NoteDoc, NoteId, Result, ServerEffect, ServerMsg, ServerSession,
};
use proptest::prelude::*;

const TOKEN: &str = "test-token";

struct Docs(HashMap<DocKey, NoteDoc>);

impl Docs {
    fn with_note(id: NoteId) -> Self {
        Self(HashMap::from([(DocKey::from(id), NoteDoc::new(id))]))
    }

    fn note(&self, key: DocKey) -> &NoteDoc {
        &self.0[&key]
    }
}

impl ClientDocs for Docs {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> Result<()> {
        self.0[&doc].import_update(payload)
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> Result<Vec<u8>> {
        self.0[&doc].export_updates_since(have)
    }
}

impl DocProvider for Docs {
    fn catch_up(&mut self, doc: DocKey, have: &[u8]) -> Result<CatchUp> {
        let note = &self.0[&doc];
        Ok(CatchUp {
            server_have: note.version(),
            payload: note.export_updates_since(have)?,
        })
    }

    fn import_update(&mut self, doc: DocKey, payload: &[u8]) -> Result<()> {
        self.0[&doc].import_update(payload)
    }

    fn list_docs(&mut self) -> Vec<DocKey> {
        self.0.keys().copied().collect()
    }
}

/// One server + N clients over in-memory frame queues.
struct Net {
    server_docs: Docs,
    server_sessions: Vec<Option<ServerSession>>,
    clients: Vec<(ClientSession, Docs)>,
    /// (client index, direction) frame queue. Encoded on push, decoded on pop.
    to_server: VecDeque<(usize, Vec<u8>)>,
    to_client: VecDeque<(usize, Vec<u8>)>,
}

impl Net {
    fn new(server_docs: Docs) -> Self {
        Self {
            server_docs,
            server_sessions: Vec::new(),
            clients: Vec::new(),
            to_server: VecDeque::new(),
            to_client: VecDeque::new(),
        }
    }

    fn add_client(&mut self, docs: Docs) -> usize {
        let idx = self.clients.len();
        self.clients
            .push((ClientSession::new(DeviceId::new(), TOKEN.into()), docs));
        self.server_sessions
            .push(Some(ServerSession::new(vec![TOKEN.into()])));
        let effects = self.clients[idx].0.connect();
        self.run_client_effects(idx, effects);
        self.pump();
        idx
    }

    /// Sever one client's connection (server side forgets the session).
    fn disconnect(&mut self, idx: usize) {
        self.server_sessions[idx] = None;
    }

    /// Reconnect: fresh sessions both sides, then resubscribe to `doc`.
    fn reconnect(&mut self, idx: usize, doc: DocKey) {
        self.server_sessions[idx] = Some(ServerSession::new(vec![TOKEN.into()]));
        let (client, _) = &mut self.clients[idx];
        *client = ClientSession::new(client.device(), TOKEN.into());
        let effects = client.connect();
        self.run_client_effects(idx, effects);
        self.pump();
        let have = docs_version(&self.clients[idx].1, doc);
        let effects = self.clients[idx].0.subscribe(doc, have);
        self.run_client_effects(idx, effects);
        self.pump();
    }

    fn subscribe(&mut self, idx: usize, doc: DocKey) {
        let have = docs_version(&self.clients[idx].1, doc);
        let effects = self.clients[idx].0.subscribe(doc, have);
        self.run_client_effects(idx, effects);
        self.pump();
    }

    fn local_edit(&mut self, idx: usize, doc: DocKey, edit: impl FnOnce(&NoteDoc)) {
        let (client, docs) = &mut self.clients[idx];
        let before = docs.note(doc).version();
        edit(docs.note(doc));
        let payload = docs.note(doc).export_updates_since(&before).unwrap();
        let effects = client.local_update(doc, payload);
        self.run_client_effects(idx, effects);
        self.pump();
    }

    fn run_client_effects(&mut self, idx: usize, effects: Vec<ClientEffect>) {
        for effect in effects {
            match effect {
                ClientEffect::Send(msg) => {
                    self.to_server.push_back((idx, msg.encode().unwrap()));
                }
                ClientEffect::Fatal(err) => panic!("client {idx} fatal: {err}"),
                ClientEffect::Connected
                | ClientEffect::DocSynced(_)
                | ClientEffect::Ephemeral { .. } => {}
            }
        }
    }

    /// Drain both queues until quiescent.
    fn pump(&mut self) {
        loop {
            if let Some((idx, frame)) = self.to_server.pop_front() {
                let msg = ClientMsg::decode(&frame).unwrap();
                let Some(session) = self.server_sessions[idx].as_mut() else {
                    continue; // frame raced a disconnect; drop it
                };
                let effects = session.handle(msg, &mut self.server_docs);
                for effect in effects {
                    match effect {
                        ServerEffect::Send(msg) => {
                            self.to_client.push_back((idx, msg.encode().unwrap()));
                        }
                        ServerEffect::Broadcast { doc, msg } => {
                            let frame = msg.encode().unwrap();
                            for (other, session) in self.server_sessions.iter().enumerate() {
                                let subscribed = other != idx
                                    && session.as_ref().is_some_and(|s| s.is_subscribed(doc));
                                if subscribed {
                                    self.to_client.push_back((other, frame.clone()));
                                }
                            }
                        }
                        ServerEffect::Disconnect { code, message } => {
                            panic!("server disconnected client {idx}: {code:?} {message}")
                        }
                    }
                }
                continue;
            }
            if let Some((idx, frame)) = self.to_client.pop_front() {
                let msg = ServerMsg::decode(&frame).unwrap();
                let (client, docs) = &mut self.clients[idx];
                let effects = client.handle(msg, docs);
                self.run_client_effects(idx, effects);
                continue;
            }
            break;
        }
    }
}

fn docs_version(docs: &Docs, doc: DocKey) -> Vec<u8> {
    docs.note(doc).version()
}

fn three_replicas() -> (Net, DocKey, NoteId) {
    let id = NoteId::new();
    let key = DocKey::from(id);
    let mut net = Net::new(Docs::with_note(id));
    let a = net.add_client(Docs::with_note(id));
    let b = net.add_client(Docs::with_note(id));
    net.subscribe(a, key);
    net.subscribe(b, key);
    (net, key, id)
}

#[test]
fn live_updates_relay_between_clients() {
    let (mut net, key, _) = three_replicas();

    net.local_edit(0, key, |doc| doc.splice_text(0, 0, "from a: ").unwrap());
    net.local_edit(1, key, |doc| {
        let len = doc.text_len();
        doc.splice_text(len, 0, "from b").unwrap();
    });

    let expect = "from a: from b";
    assert_eq!(net.clients[0].1.note(key).text(), expect);
    assert_eq!(net.clients[1].1.note(key).text(), expect);
    assert_eq!(net.server_docs.note(key).text(), expect);
}

#[test]
fn offline_edits_converge_on_rejoin() {
    let (mut net, key, _) = three_replicas();

    net.local_edit(0, key, |doc| doc.splice_text(0, 0, "shared\n").unwrap());
    net.disconnect(1);

    // A keeps editing live; B edits offline (no session to carry the update).
    net.local_edit(0, key, |doc| doc.splice_text(0, 0, "a-online\n").unwrap());
    net.clients[1]
        .1
        .note(key)
        .splice_text(0, 0, "b-offline\n")
        .unwrap();

    net.reconnect(1, key);

    let a_text = net.clients[0].1.note(key).text();
    let b_text = net.clients[1].1.note(key).text();
    assert_eq!(a_text, b_text);
    assert_eq!(net.server_docs.note(key).text(), a_text);
    assert!(a_text.contains("a-online") && a_text.contains("b-offline"));
}

#[test]
fn ephemeral_relays_without_touching_docs() {
    let (mut net, key, _) = three_replicas();
    let before = net.server_docs.note(key).version();

    let effects = net.clients[0].0.ephemeral(key, vec![9, 9, 9]);
    net.run_client_effects(0, effects);

    // Capture what client B receives.
    let (idx, frame) = net.to_server.pop_front().unwrap();
    let msg = ClientMsg::decode(&frame).unwrap();
    let session = net.server_sessions[idx].as_mut().unwrap();
    let effects = session.handle(msg, &mut net.server_docs);
    let [ServerEffect::Broadcast { doc, msg }] = effects.as_slice() else {
        panic!("expected a single broadcast, got {effects:?}");
    };
    assert_eq!(*doc, key);
    assert!(matches!(msg, ServerMsg::Ephemeral { payload, .. } if payload == &[9, 9, 9]));
    assert_eq!(
        net.server_docs.note(key).version(),
        before,
        "ephemeral must not commit"
    );
}

#[test]
fn bad_token_and_pre_hello_messages_disconnect() {
    let id = NoteId::new();
    let mut docs = Docs::with_note(id);

    let mut session = ServerSession::new(vec![TOKEN.into()]);
    let effects = session.handle(
        ClientMsg::Hello {
            device: DeviceId::new(),
            token: "wrong".into(),
        },
        &mut docs,
    );
    assert!(matches!(
        effects.as_slice(),
        [ServerEffect::Disconnect { .. }]
    ));

    let mut session = ServerSession::new(vec![TOKEN.into()]);
    let effects = session.handle(ClientMsg::ListDocs, &mut docs);
    assert!(matches!(
        effects.as_slice(),
        [ServerEffect::Disconnect { .. }]
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Malformed frames must error, never panic.
    #[test]
    fn arbitrary_bytes_never_panic_decoder(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let _ = ClientMsg::decode(&bytes);
        let _ = ServerMsg::decode(&bytes);
    }
}
