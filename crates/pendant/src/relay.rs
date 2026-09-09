//! Embedded sync relay: every desktop instance serves `pendant-server`'s
//! router in-process so an iPad on the same network can pair straight to
//! the desktop with no dedicated relay. A dedicated relay (if configured)
//! is the fallback path; the desktop bridges between the two in
//! [`crate::sync`] so peers on either side converge.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use bevy::prelude::*;
use pendant_core::Store;
use pendant_server::relay::AppState;
use tokio::net::TcpListener;

use crate::errors::{Error, Result, ResultExt};

/// Default relay port, shared with `pendant-server`.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8722";

/// The in-process relay. Dropping it takes a final checkpoint.
#[derive(Resource)]
pub struct EmbeddedRelay {
    state: AppState,
    /// `ws://<lan-ip>:<port>/ws` — what the pairing QR advertises.
    pub advertised: String,
    /// `ws://127.0.0.1:<port>/ws` — what this process connects to itself.
    pub local: String,
    _serve: tokio::task::JoinHandle<()>,
    _maintenance: tokio::task::JoinHandle<()>,
}

impl EmbeddedRelay {
    /// Bind and serve. Falls back to an ephemeral port when `listen` is
    /// taken so a second instance (or a local `pendant-server`) never
    /// blocks startup.
    pub fn start(
        runtime: &tokio::runtime::Handle,
        listen: SocketAddr,
        store_path: &Path,
        tokens: Vec<String>,
    ) -> Result<Self> {
        let store = Store::open(store_path)
            .change_context(Error)
            .attach_with(|| format!("opening relay store {}", store_path.display()))?;
        let state = pendant_server::app_state(store, tokens);

        let listener = runtime.block_on(bind(listen))?;
        let bound = listener.local_addr().change_context(Error)?;
        let port = bound.port();
        let ip = lan_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        tracing::info!(listen = %bound, advertised = %ip, "embedded relay up");

        let serve = runtime.spawn({
            let router = pendant_server::router(state.clone());
            async move {
                if let Err(err) = axum::serve(listener, router).await {
                    tracing::error!(%err, "embedded relay stopped");
                }
            }
        });
        let maintenance = runtime.spawn(pendant_server::maintenance(std::sync::Arc::clone(
            &state.docs,
        )));

        Ok(Self {
            state,
            advertised: format!("ws://{ip}:{port}/ws"),
            local: format!("ws://127.0.0.1:{port}/ws"),
            _serve: serve,
            _maintenance: maintenance,
        })
    }

    /// Accept `token` from now on (after joining another workspace live).
    pub fn add_token(&self, token: &str) {
        self.state.add_token(token.to_string());
    }
}

impl Drop for EmbeddedRelay {
    fn drop(&mut self) {
        if let Err(err) = pendant_server::checkpoint(&self.state) {
            tracing::error!(%err, "embedded relay final checkpoint failed");
        }
    }
}

async fn bind(listen: SocketAddr) -> Result<TcpListener> {
    match TcpListener::bind(listen).await {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && listen.port() != 0 => {
            tracing::warn!(%listen, "relay port busy; using an ephemeral one");
            let ephemeral = SocketAddr::new(listen.ip(), 0);
            TcpListener::bind(ephemeral)
                .await
                .change_context(Error)
                .attach_with(|| format!("binding {ephemeral}"))
        }
        Err(err) => Err(err)
            .change_context(Error)
            .attach_with(|| format!("binding {listen}")),
    }
}

/// The address peers on the LAN reach us at: the source IP the kernel
/// would pick for an outbound packet. `connect` on UDP sends nothing.
fn lan_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("10.254.254.254:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}
