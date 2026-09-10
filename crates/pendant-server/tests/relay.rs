//! Integration test: real server on an ephemeral port, three headless clients
//! over real WebSockets. Verifies convergence and relay latency.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use pendant_core::{
    ClientDocs, ClientEffect, ClientSession, DeviceId, DocKey, NoteDoc, NoteId, Result, ServerMsg,
    Store,
};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

const TOKEN: &str = "integration-token";

struct Docs(HashMap<DocKey, NoteDoc>);

impl ClientDocs for Docs {
    fn import(&mut self, doc: DocKey, payload: &[u8]) -> Result<()> {
        self.0[&doc].import_update(payload)
    }

    fn updates_since(&mut self, doc: DocKey, have: &[u8]) -> Result<Vec<u8>> {
        self.0[&doc].export_updates_since(have)
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct TestClient {
    session: ClientSession,
    docs: Docs,
    socket: Socket,
}

impl TestClient {
    async fn connect(port: u16, note: NoteId) -> Self {
        let mut request = format!("ws://127.0.0.1:{port}/ws")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();

        let mut client = Self {
            session: ClientSession::new(DeviceId::new(), TOKEN.into()),
            docs: Docs(HashMap::from([(DocKey::from(note), NoteDoc::new(note))])),
            socket,
        };
        let effects = client.session.connect();
        client.run_effects(effects).await;
        client.recv_until(|c| c.session.is_ready()).await;

        let key = DocKey::from(note);
        let have = client.docs.0[&key].version();
        let effects = client.session.subscribe(key, have);
        client.run_effects(effects).await;
        client.recv_until(|c| c.session.is_subscribed(key)).await;
        client
    }

    fn note(&self, key: DocKey) -> &NoteDoc {
        &self.docs.0[&key]
    }

    async fn edit(&mut self, key: DocKey, edit: impl FnOnce(&NoteDoc)) {
        let before = self.note(key).version();
        edit(self.note(key));
        let payload = self.note(key).export_updates_since(&before).unwrap();
        let effects = self.session.local_update(key, payload);
        self.run_effects(effects).await;
    }

    /// Receive and apply frames until `done` holds. Panics after 5s.
    async fn recv_until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(self) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("recv_until timed out");
            let frame = tokio::time::timeout(remaining, self.socket.next())
                .await
                .expect("recv_until timed out")
                .expect("socket closed")
                .expect("socket error");
            let Message::Binary(bytes) = frame else {
                continue;
            };
            let msg = ServerMsg::decode(&bytes).unwrap();
            let effects = self.session.handle(msg, &mut self.docs);
            self.run_effects(effects).await;
        }
    }

    async fn run_effects(&mut self, effects: Vec<ClientEffect>) {
        for effect in effects {
            match effect {
                ClientEffect::Send(msg) => {
                    self.socket
                        .send(Message::Binary(msg.encode().unwrap().into()))
                        .await
                        .unwrap();
                }
                ClientEffect::Fatal(err) => panic!("client fatal: {err}"),
                ClientEffect::Connected
                | ClientEffect::DocSynced(_)
                | ClientEffect::Ephemeral { .. } => {}
            }
        }
    }
}

async fn spawn_server() -> u16 {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("server.redb")).unwrap();
    let state = pendant_server::AppState::new(store, vec![TOKEN.into()]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _dir = dir; // keep the store's tempdir alive for the whole test
        axum::serve(listener, state.router()).await.unwrap();
    });
    port
}

#[tokio::test]
async fn three_clients_converge_over_real_sockets() {
    let port = spawn_server().await;
    let note = NoteId::new();
    let key = DocKey::from(note);

    let mut a = TestClient::connect(port, note).await;
    let mut b = TestClient::connect(port, note).await;
    let mut c = TestClient::connect(port, note).await;

    a.edit(key, |doc| doc.splice_text(0, 0, "alpha ").unwrap())
        .await;
    b.recv_until(|cl| cl.note(key).text().contains("alpha"))
        .await;
    b.edit(key, |doc| {
        let len = doc.text_len();
        doc.splice_text(len, 0, "beta ").unwrap();
    })
    .await;

    let done = |cl: &TestClient| {
        let text = cl.note(key).text();
        text.contains("alpha") && text.contains("beta")
    };
    a.recv_until(done).await;
    c.recv_until(done).await;

    assert_eq!(a.note(key).text(), b.note(key).text());
    assert_eq!(a.note(key).text(), c.note(key).text());
}

#[tokio::test]
async fn relay_latency_p95_under_10ms() {
    let port = spawn_server().await;
    let note = NoteId::new();
    let key = DocKey::from(note);

    let mut a = TestClient::connect(port, note).await;
    let mut b = TestClient::connect(port, note).await;

    let mut samples = Vec::with_capacity(50);
    for i in 0..50u32 {
        let marker = format!("m{i};");
        let started = Instant::now();
        a.edit(key, |doc| {
            let len = doc.text_len();
            doc.splice_text(len, 0, &marker).unwrap();
        })
        .await;
        b.recv_until(|cl| cl.note(key).text().contains(&marker))
            .await;
        samples.push(started.elapsed());
    }

    samples.sort();
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    assert!(
        p95 < Duration::from_millis(10),
        "p95 relay latency {p95:?} exceeds 10ms (samples: {samples:?})"
    );
}

#[tokio::test]
async fn unauthorized_upgrade_rejected() {
    let port = spawn_server().await;
    let request = format!("ws://127.0.0.1:{port}/ws")
        .into_client_request()
        .unwrap();
    // No Authorization header at all.
    let err = tokio_tungstenite::connect_async(request).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("401"), "expected 401 rejection, got: {text}");
}
