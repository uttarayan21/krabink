//! QUIC side of an accepted connection: attach the peer's lanes to the
//! protocol loop and report which path the connection runs over.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, TransportAddr};
use tokio::sync::mpsc;

use crate::framing::{Lane, read_frame, read_lane, write_frame};
use crate::hub::{Hub, PeerSink};
use crate::peers::{PeerKind, PeerState, PeerStatus, Route};
use crate::serve::{PeerIo, serve_peer};

/// Accept connections until the endpoint closes.
pub(crate) async fn accept_loop(endpoint: Endpoint, hub: Arc<Hub>) {
    while let Some(incoming) = endpoint.accept().await {
        let hub = hub.clone();
        tokio::spawn(async move {
            let accepting = match incoming.accept() {
                Ok(accepting) => accepting,
                Err(err) => {
                    tracing::debug!(%err, "incoming connection dropped");
                    return;
                }
            };
            match accepting.await {
                Ok(conn) => serve_connection(conn, hub).await,
                Err(err) => tracing::debug!(%err, "incoming handshake failed"),
            }
        });
    }
}

/// Serve one accepted connection until it closes.
pub(crate) async fn serve_connection(conn: Connection, hub: Arc<Hub>) {
    let remote = conn.remote_id();
    tracing::info!(remote = %remote.fmt_short(), "peer connected");

    let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (docs_tx, docs_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (eph_tx, eph_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let peer_id = hub.register(
        PeerSink::Inbound {
            docs: docs_tx.clone(),
            eph: eph_tx.clone(),
            subscribed: HashSet::new(),
        },
        PeerStatus {
            id: Some(remote),
            kind: PeerKind::Unknown,
            state: PeerState::Connected {
                route: current_route(&conn),
            },
            inbound: true,
            device: None,
        },
    );
    let mut serve = tokio::spawn(serve_peer(
        hub.clone(),
        peer_id,
        PeerIo {
            incoming: in_rx,
            docs: docs_tx,
            eph: eph_tx,
        },
    ));
    let routes = tokio::spawn(watch_routes(conn.clone(), hub.clone(), peer_id));

    let mut docs_rx = Some(docs_rx);
    let mut eph_rx = Some(eph_rx);
    let mut readers = Vec::new();
    let mut writers = Vec::new();
    loop {
        tokio::select! {
            accepted = conn.accept_bi() => {
                let (send, mut recv) = match accepted {
                    Ok(streams) => streams,
                    Err(err) => {
                        tracing::debug!(%err, "connection closed");
                        break;
                    }
                };
                let lane = match read_lane(&mut recv).await {
                    Ok(Some(lane)) => lane,
                    Ok(None) => {
                        tracing::warn!(remote = %remote.fmt_short(), "unknown lane tag; closing");
                        break;
                    }
                    Err(err) => {
                        tracing::debug!(%err, "lane open failed");
                        break;
                    }
                };
                let out = match lane {
                    Lane::Docs => docs_rx.take(),
                    Lane::Ephemeral => eph_rx.take(),
                };
                let Some(out) = out else {
                    tracing::warn!(remote = %remote.fmt_short(), ?lane, "lane opened twice; closing");
                    break;
                };
                readers.push(tokio::spawn(pump_in(recv, in_tx.clone())));
                writers.push(tokio::spawn(pump_out(send, out)));
            }
            _ = &mut serve => break, // session asked to disconnect
            _ = conn.closed() => break,
        }
    }

    // Let queued frames (e.g. the Error before a disconnect) flush: once
    // the session and registry rows are gone the lane channels close and
    // the writers finish on their own.
    serve.abort();
    routes.abort();
    hub.deregister(peer_id);
    for writer in writers {
        let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    }
    for reader in readers {
        reader.abort();
    }
    conn.close(0u32.into(), b"bye");
    tracing::info!(remote = %remote.fmt_short(), "peer disconnected");
}

/// Read frames off a lane into the merged inbound channel.
pub(crate) async fn pump_in(mut recv: RecvStream, tx: mpsc::UnboundedSender<Vec<u8>>) {
    loop {
        match read_frame(&mut recv).await {
            Ok(Some(frame)) => {
                if tx.send(frame).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(err) => {
                tracing::debug!(%err, "lane read ended");
                return;
            }
        }
    }
}

/// Write queued frames onto a lane.
pub(crate) async fn pump_out(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if let Err(err) = write_frame(&mut send, &frame).await {
            tracing::debug!(%err, "lane write ended");
            return;
        }
    }
    // Finish and wait for the peer to read everything, so a final Error
    // frame survives the connection close that follows.
    let _ = send.finish();
    let _ = send.stopped().await;
}

pub(crate) fn current_route(conn: &Connection) -> Option<Route> {
    conn.paths()
        .iter()
        .find(|p| p.is_selected())
        .and_then(|p| route_of(p.remote_addr()))
}

fn route_of(addr: &TransportAddr) -> Option<Route> {
    match addr {
        TransportAddr::Ip(addr) => Some(Route::Direct(*addr)),
        TransportAddr::Relay(url) => Some(Route::Relay(url.clone())),
        _ => None,
    }
}

/// Follow path changes and publish the selected one as the peer's route.
pub(crate) async fn watch_routes(conn: Connection, hub: Arc<Hub>, peer_id: u64) {
    let mut paths = conn.paths_stream();
    while let Some(list) = paths.next().await {
        let route = list
            .iter()
            .find(|p| p.is_selected())
            .and_then(|p| route_of(p.remote_addr()));
        hub.set_route(peer_id, route);
    }
}
