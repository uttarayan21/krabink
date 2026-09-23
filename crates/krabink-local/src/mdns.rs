//! LAN discovery for the relay-less case (desktop only): advertise this
//! node as `_krabink._udp` with TXT `id=<EndpointId>`, browse for other
//! nodes and hand their addresses to the node as dial hints. Only matters
//! where multicast reaches and the relay does not; with a relay up, iroh
//! finds the LAN path by itself.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use iroh::EndpointId;
use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::peers::rank_ip;

pub const SERVICE_TYPE: &str = "_krabink._udp.local.";

pub struct Mdns {
    own: EndpointId,
    daemon: ServiceDaemon,
    fullname: String,
    events: Receiver<ServiceEvent>,
}

impl Mdns {
    /// Advertise `node` on `port` at every candidate interface and start
    /// browsing for other nodes.
    pub fn start(node: EndpointId, port: u16, host: &str) -> Result<Self, mdns_sd::Error> {
        let instance = format!("krabink-{host}");
        let props = HashMap::from([("id".to_string(), node.to_string())]);
        let addrs: Vec<IpAddr> = candidate_ips().into_iter().map(IpAddr::V4).collect();
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
        let events = daemon.browse(SERVICE_TYPE)?;
        tracing::info!(name = %fullname, port, "mDNS advertising");
        Ok(Self {
            own: node,
            daemon,
            fullname,
            events,
        })
    }

    pub fn name(&self) -> &str {
        &self.fullname
    }

    /// Drain pending events: every `(node, ip:port)` resolved this poll,
    /// best address first, excluding ourselves.
    pub fn poll(&self) -> Vec<(EndpointId, SocketAddr)> {
        let mut found = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            let ServiceEvent::ServiceResolved(info) = event else {
                continue;
            };
            let Some(id) = info
                .get_properties()
                .get_property_val_str("id")
                .and_then(|id| id.parse::<EndpointId>().ok())
            else {
                continue;
            };
            if id == self.own {
                continue;
            }
            let port = info.get_port();
            let mut ips: Vec<Ipv4Addr> = info
                .get_addresses_v4()
                .into_iter()
                .filter(|ip| !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified())
                .collect();
            ips.sort_by_key(|ip| (rank_ip(IpAddr::V4(*ip)), ip.octets()));
            found.extend(ips.into_iter().map(|ip| (id, SocketAddr::from((ip, port)))));
        }
        found
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Every IPv4 address peers could reach us at, best first.
pub fn candidate_ips() -> Vec<Ipv4Addr> {
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
    ips.sort_by_key(|ip| (rank_ip(IpAddr::V4(*ip)), ip.octets()));
    ips.dedup();
    ips
}

/// Container/VM bridges nobody pairs through.
fn is_virtual(name: &str) -> bool {
    ["docker", "virbr", "br-", "veth", "lxc", "vmnet", "bridge"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}
