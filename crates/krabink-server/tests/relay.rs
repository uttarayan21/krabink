//! Real relay on an ephemeral port: nodes find each other through it,
//! tokens gate it, the replica bridges devices that are never online
//! together.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use iroh::RelayUrl;
use iroh_relay::server::Server;
use krabink_core::NoteId;
use krabink_local::testing::{App, TOKEN, node, node_with_role, relay_target, wait_connected};
use krabink_local::{PeerKind, RelayTarget, Role};
use krabink_server::RelayOpts;

async fn dev_relay() -> (Server, RelayUrl) {
    let server = krabink_server::relay::spawn(
        RelayOpts::dev(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        vec![TOKEN.into()],
    )
    .await
    .expect("relay spawns");
    let url: RelayUrl = format!("http://{}", server.http_addr().expect("http listener"))
        .parse()
        .unwrap();
    (server, url)
}

fn relay(url: &RelayUrl, token: &str) -> Option<RelayTarget> {
    Some(RelayTarget {
        url: url.clone(),
        token: token.into(),
    })
}

#[tokio::test]
async fn three_nodes_converge_over_relay() {
    let (_server, url) = dev_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], relay(&url, TOKEN)).await;
    let b = node(&dir, "b", &[TOKEN], relay(&url, TOKEN)).await;
    let c = node(&dir, "c", &[TOKEN], relay(&url, TOKEN)).await;
    let note = NoteId::new();
    let mut app_a = App::open(&a, note).await;
    let mut app_b = App::open(&b, note).await;
    let mut app_c = App::open(&c, note).await;

    // Only the relay is known: the handshake has to go through it.
    b.set_peers(vec![relay_target(&a, TOKEN, PeerKind::Desktop)])
        .await;
    c.set_peers(vec![relay_target(&a, TOKEN, PeerKind::Desktop)])
        .await;
    wait_connected(&b, &a).await;
    wait_connected(&c, &a).await;

    app_b.edit(note, "via relay");
    app_a.pump_until(|app| app.text(note) == "via relay").await;
    app_c.pump_until(|app| app.text(note) == "via relay").await;

    c.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn relay_latency_p95_under_budget() {
    let (_server, url) = dev_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], relay(&url, TOKEN)).await;
    let b = node(&dir, "b", &[TOKEN], relay(&url, TOKEN)).await;
    let note = NoteId::new();
    let mut app_a = App::open(&a, note).await;
    let mut app_b = App::open(&b, note).await;
    b.set_peers(vec![relay_target(&a, TOKEN, PeerKind::Desktop)])
        .await;
    wait_connected(&b, &a).await;

    let mut samples = Vec::new();
    for i in 0..50 {
        let marker = format!("e{i};");
        let start = Instant::now();
        app_a.edit(note, &marker);
        app_b
            .pump_until(|app| app.text(note).starts_with(&marker))
            .await;
        samples.push(start.elapsed());
    }
    samples.sort();
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    let budget = Duration::from_millis(
        std::env::var("KRABINK_LATENCY_BUDGET_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
    );
    assert!(p95 < budget, "p95 {p95:?} over budget {budget:?}");

    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_relay_token_is_denied() {
    let (_server, url) = dev_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], relay(&url, "wrong")).await;
    let mut health = a.watch_relay();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let current = health.borrow_and_update().clone();
        if let Some(err) = &current.error {
            assert!(err.contains("not authorized"), "{err}");
            assert!(!current.connected);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no relay error reported: {current:?}"
        );
        let _ = tokio::time::timeout(Duration::from_millis(500), health.changed()).await;
    }
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_hello_token_disconnects_over_relay() {
    let (_server, url) = dev_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], relay(&url, TOKEN)).await;
    let b = node(&dir, "b", &[TOKEN], relay(&url, TOKEN)).await;
    b.set_peers(vec![relay_target(&a, "wrong-hello", PeerKind::Desktop)])
        .await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if b.peers()
            .iter()
            .any(|p| matches!(p.state, krabink_local::PeerState::Fatal { .. }))
        {
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", b.peers());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn replica_bridges_offline_edits() {
    let (_server, url) = dev_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let replica =
        node_with_role(&dir, "replica", &[TOKEN], relay(&url, TOKEN), Role::Replica).await;
    let a = node(&dir, "a", &[TOKEN], relay(&url, TOKEN)).await;
    let b = node(&dir, "b", &[TOKEN], relay(&url, TOKEN)).await;
    let note = NoteId::new();
    let mut app_a = App::open(&a, note).await;
    let mut app_b = App::open(&b, note).await;
    a.set_peers(vec![relay_target(&replica, TOKEN, PeerKind::Replica)])
        .await;
    b.set_peers(vec![relay_target(&replica, TOKEN, PeerKind::Replica)])
        .await;
    wait_connected(&a, &replica).await;
    wait_connected(&b, &replica).await;

    // B goes away; A keeps editing; the replica holds it.
    b.suspend().await;
    app_a.edit(note, "while b was away");
    tokio::time::sleep(Duration::from_millis(500)).await;

    b.resume().await.unwrap();
    wait_connected(&b, &replica).await;
    app_b
        .pump_until(|app| app.text(note) == "while b was away")
        .await;

    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
    replica.shutdown().await.unwrap();
}
