//! Nodes talking QUIC over loopback, no relay: direct path, three-node
//! bridging without echo storms, the in-process link, token rejection.

use std::time::Duration;

use pendant_core::{DocKey, NoteId};
use pendant_local::testing::{App, TOKEN, loopback_target, node, wait_connected};
use pendant_local::{PeerKind, PeerState, Route};

#[tokio::test]
async fn local_link_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], None).await;
    let note = NoteId::new();
    let app = App::open(&a, note).await;
    assert!(app.session.is_ready());
    assert!(app.session.is_subscribed(DocKey::from(note)));
    let local = a
        .peers()
        .into_iter()
        .find(|p| p.kind == PeerKind::Local)
        .unwrap();
    assert_eq!(local.device, Some(a.device()));
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_nodes_direct_on_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], None).await;
    let b = node(&dir, "b", &[TOKEN], None).await;
    let note = NoteId::new();
    let mut app_a = App::open(&a, note).await;
    let mut app_b = App::open(&b, note).await;

    b.set_peers(vec![loopback_target(&a, TOKEN).await]).await;
    let route = match wait_connected(&b, &a).await {
        Some(route) => route,
        None => {
            // The first path report can trail the handshake by a moment.
            tokio::time::sleep(Duration::from_millis(500)).await;
            wait_connected(&b, &a).await.expect("route reported")
        }
    };
    assert!(matches!(route, Route::Direct(_)), "{route:?}");

    app_a.edit(note, "hello from a");
    app_b
        .pump_until(|app| app.text(note).contains("hello from a"))
        .await;
    app_b.edit(note, "and b: ");
    app_a
        .pump_until(|app| app.text(note).starts_with("and b: "))
        .await;
    assert_eq!(app_a.text(note), app_b.text(note));

    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn three_nodes_converge_through_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], None).await;
    let b = node(&dir, "b", &[TOKEN], None).await;
    let c = node(&dir, "c", &[TOKEN], None).await;
    let note = NoteId::new();
    let mut app_a = App::open(&a, note).await;
    let mut app_b = App::open(&b, note).await;
    let mut app_c = App::open(&c, note).await;

    // B and C both dial A; C also dials B, closing a cycle.
    b.set_peers(vec![loopback_target(&a, TOKEN).await]).await;
    c.set_peers(vec![
        loopback_target(&a, TOKEN).await,
        loopback_target(&b, TOKEN).await,
    ])
    .await;
    wait_connected(&b, &a).await;
    wait_connected(&c, &a).await;
    wait_connected(&c, &b).await;

    app_b.edit(note, "from b");
    app_c.pump_until(|app| app.text(note) == "from b").await;
    app_a.pump_until(|app| app.text(note) == "from b").await;

    // Let any echoes arrive, then check every app saw the update once.
    app_a.settle().await;
    app_b.settle().await;
    app_c.settle().await;
    assert_eq!(app_a.updates, 1, "a saw the edit once");
    assert_eq!(app_c.updates, 1, "c saw the edit once");
    assert_eq!(app_b.updates, 0, "b never hears its own edit back");

    c.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_hello_token_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let a = node(&dir, "a", &[TOKEN], None).await;
    let b = node(&dir, "b", &["other"], None).await;
    b.set_peers(vec![loopback_target(&a, "wrong").await]).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let fatal = b
            .peers()
            .iter()
            .any(|p| matches!(p.state, PeerState::Fatal { .. }));
        if fatal {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{:?}", b.peers());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}
