//! Peer descriptions and the status the node reports about them.

use std::net::SocketAddr;

use iroh::{EndpointId, RelayUrl};
use pendant_core::DeviceId;

/// What a peer is, for the settings screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerKind {
    /// The app on this very device (the in-process link).
    Local,
    /// Always-on cloud replica.
    Replica,
    Desktop,
    Tablet,
    Unknown,
}

/// Somebody this node dials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerTarget {
    pub id: EndpointId,
    /// Home relay of the peer (handshake + fallback path).
    pub relay: Option<RelayUrl>,
    /// Direct address hints (LAN, Tailscale…) from the pairing QR.
    pub addrs: Vec<SocketAddr>,
    /// Workspace token sent in `Hello`.
    pub token: String,
    pub kind: PeerKind,
}

/// The path a connection currently uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct(SocketAddr),
    Relay(RelayUrl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    /// Handshake done. `route` is `None` until the first path report.
    Connected {
        route: Option<Route>,
    },
    /// The peer rejected us (bad token, protocol error); retried slowly.
    Fatal {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    /// Absent for the local link.
    pub id: Option<EndpointId>,
    pub kind: PeerKind,
    pub state: PeerState,
    /// True when the peer dialled us.
    pub inbound: bool,
    /// The peer's CRDT device id once it said hello.
    pub device: Option<DeviceId>,
}

impl PeerTarget {
    /// The node a pairing URI points at.
    pub fn from_pair(info: &pendant_core::PairInfo, kind: PeerKind) -> Result<Self, String> {
        let id: EndpointId = info
            .node
            .parse()
            .map_err(|err| format!("node id {:?}: {err}", info.node))?;
        let relay = info
            .relay
            .as_deref()
            .map(|url| {
                url.parse::<RelayUrl>()
                    .map_err(|err| format!("relay {url:?}: {err}"))
            })
            .transpose()?;
        let addrs = info
            .addrs
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
            .collect();
        Ok(Self {
            id,
            relay,
            addrs,
            token: info.token.clone(),
            kind,
        })
    }

    /// The workspace replica named by a pairing URI, if any.
    pub fn replica_from_pair(info: &pendant_core::PairInfo) -> Result<Option<Self>, String> {
        let Some(replica) = &info.replica else {
            return Ok(None);
        };
        let id: EndpointId = replica
            .parse()
            .map_err(|err| format!("replica id {replica:?}: {err}"))?;
        let relay = info
            .relay
            .as_deref()
            .map(|url| {
                url.parse::<RelayUrl>()
                    .map_err(|err| format!("relay {url:?}: {err}"))
            })
            .transpose()?;
        Ok(Some(Self {
            id,
            relay,
            addrs: Vec::new(),
            token: info.token.clone(),
            kind: PeerKind::Replica,
        }))
    }
}

/// Lower is better: real LAN ranges first (where hole punching is
/// trivial), then carrier-grade NAT space overlays like Tailscale use,
/// then anything public.
pub fn rank_ip(ip: std::net::IpAddr) -> u8 {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            if ip.is_private() {
                0
            } else if a == 100 && (64..128).contains(&b) {
                1
            } else {
                2
            }
        }
        std::net::IpAddr::V6(ip) => {
            if ip.is_unique_local() {
                1
            } else {
                3
            }
        }
    }
}

/// The dialable direct addresses in an endpoint address, best first:
/// IPv4 only, no loopback/link-local, ranked by [`rank_ip`].
pub fn direct_addrs(addr: &iroh::EndpointAddr) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = addr
        .addrs
        .iter()
        .filter_map(|a| match a {
            iroh::TransportAddr::Ip(addr) => Some(*addr),
            _ => None,
        })
        .filter(|a| match a.ip() {
            std::net::IpAddr::V4(ip) => {
                !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified()
            }
            std::net::IpAddr::V6(_) => false,
        })
        .collect();
    addrs.sort_by_key(|a| (rank_ip(a.ip()), a.ip()));
    addrs.dedup();
    addrs
}
