//! Embedded sync relay: every desktop instance serves `pendant-server`'s
//! router in-process so an iPad can pair straight to the desktop with no
//! dedicated relay. The relay is reachable on every interface (LAN,
//! Tailscale, …) and advertised over mDNS as `_pendant._tcp` with the
//! desktop's device id in TXT, so clients on the same network find it even
//! after its address changed. A dedicated relay (if configured) is the
//! fallback path; the desktop bridges between the two in [`crate::sync`].

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use bevy::prelude::*;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use pendant_core::{DeviceId, Store};
use pendant_server::relay::AppState;
use tokio::net::TcpListener;

use crate::errors::{Error, Result, ResultExt};

/// Default relay port, shared with `pendant-server`.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8722";

/// DNS-SD service type clients browse for.
pub const SERVICE_TYPE: &str = "_pendant._tcp.local.";

/// The in-process relay. Dropping it takes a final checkpoint and
/// withdraws the mDNS advertisement.
#[derive(Resource)]
pub struct EmbeddedRelay {
    state: AppState,
    /// `ws://<lan-ip>:<port>/ws` — the preferred direct path in the QR.
    pub advertised: String,
    /// Direct paths on the other interfaces (VPN/overlay, public…).
    pub alt: Vec<String>,
    /// `ws://127.0.0.1:<port>/ws` — what this process connects to itself.
    pub local: String,
    /// mDNS instance name, `None` when advertising failed.
    pub mdns_name: Option<String>,
    mdns: Option<(ServiceDaemon, String)>,
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
        device: DeviceId,
    ) -> Result<Self> {
        let store = Store::open(store_path)
            .change_context(Error)
            .attach_with(|| format!("opening relay store {}", store_path.display()))?;
        let state = pendant_server::app_state(store, tokens);

        let listener = runtime.block_on(bind(listen))?;
        let bound = listener.local_addr().change_context(Error)?;
        let port = bound.port();

        let ips = candidate_ips();
        let url = |ip: &Ipv4Addr| format!("ws://{ip}:{port}/ws");
        let advertised = ips.first().map(url).unwrap_or_else(|| {
            tracing::warn!("no usable interface; advertising loopback only");
            url(&Ipv4Addr::LOCALHOST)
        });
        let alt: Vec<String> = ips.iter().skip(1).map(url).collect();

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

        let mdns = advertise(&ips, port, device)
            .map_err(|err| tracing::warn!(%err, "mDNS advertising failed; direct pairing by address only"))
            .ok();

        Ok(Self {
            state,
            advertised,
            alt,
            local: format!("ws://127.0.0.1:{port}/ws"),
            mdns_name: mdns.as_ref().map(|(_, name)| name.clone()),
            mdns,
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
        if let Some((daemon, fullname)) = self.mdns.take() {
            let _ = daemon.unregister(&fullname);
            let _ = daemon.shutdown();
        }
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

/// Register `_pendant._tcp` on every candidate address. Returns the
/// daemon (keeps answering queries) and the service's full name.
fn advertise(
    ips: &[Ipv4Addr],
    port: u16,
    device: DeviceId,
) -> std::result::Result<(ServiceDaemon, String), mdns_sd::Error> {
    let host = crate::sync::local_device_name();
    let instance = format!("pendant-{host}");
    let props = HashMap::from([
        ("id".to_string(), device.to_string()),
        ("path".to_string(), "/ws".to_string()),
    ]);
    let addrs: Vec<IpAddr> = ips.iter().copied().map(IpAddr::V4).collect();
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{host}.local."),
        addrs.as_slice(),
        port,
        props,
    )?;
    let fullname = info.get_fullname().to_string();
    let daemon = ServiceDaemon::new()?;
    daemon.register(info)?;
    tracing::info!(name = %fullname, "mDNS advertising");
    Ok((daemon, fullname))
}

/// Every IPv4 address peers could reach us at, best first: real LAN
/// ranges (same network, also where mDNS works), then carrier-grade NAT
/// space that overlays like Tailscale use, then anything public.
fn candidate_ips() -> Vec<Ipv4Addr> {
    let mut ips: Vec<Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|iface| !iface.is_loopback() && !is_virtual(&iface.name))
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => Some(v4.ip),
            if_addrs::IfAddr::V6(_) => None,
        })
        .filter(|ip| !ip.is_link_local() && !ip.is_unspecified())
        .collect();
    ips.sort_by_key(|ip| (rank(ip), ip.octets()));
    ips.dedup();
    ips
}

/// Container/VM bridges nobody pairs through; they only add dead
/// candidates to the QR.
fn is_virtual(name: &str) -> bool {
    ["docker", "virbr", "br-", "veth", "lxc", "vmnet", "bridge"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn rank(ip: &Ipv4Addr) -> u8 {
    let [a, b, ..] = ip.octets();
    if ip.is_private() {
        0
    } else if a == 100 && (64..128).contains(&b) {
        1 // 100.64/10 (Tailscale and friends)
    } else {
        2
    }
}
