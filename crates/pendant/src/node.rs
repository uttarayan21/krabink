//! The desktop's sync node as a Bevy resource, plus LAN discovery: mDNS
//! advertises the node and feeds found peers to it as dial hints, and the
//! QR's direct addresses follow the endpoint's.

use std::time::Duration;

use bevy::prelude::*;
use pendant_local::mdns::Mdns;
use pendant_local::{Node, direct_addrs};

use crate::settings::Settings;

#[derive(Resource)]
pub struct SyncNode {
    pub node: Node,
    mdns: Option<Mdns>,
    refresh: Timer,
}

impl SyncNode {
    /// Wrap a started node and begin advertising it on the LAN.
    pub fn new(node: Node, runtime: &tokio::runtime::Runtime) -> Self {
        let port = runtime.block_on(node.bound_port());
        let mdns = port.and_then(|port| {
            Mdns::start(node.id(), port, &crate::sync::local_device_name())
                .map_err(|err| tracing::warn!(%err, "mDNS unavailable; LAN peers need the relay"))
                .ok()
        });
        Self {
            node,
            mdns,
            refresh: Timer::new(Duration::from_secs(5), TimerMode::Repeating),
        }
    }

    pub fn mdns_name(&self) -> Option<&str> {
        self.mdns.as_ref().map(Mdns::name)
    }
}

pub struct NodePlugin;

impl Plugin for NodePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (poll_discovery, refresh_addrs));
    }
}

/// Every node mDNS resolved becomes a dial hint; the node only uses hints
/// for peers it is dialling.
fn poll_discovery(sync: Res<SyncNode>) {
    let Some(mdns) = &sync.mdns else {
        return;
    };
    for (id, addr) in mdns.poll() {
        sync.node.add_addr_hint(id, addr);
    }
}

/// Keep the QR's direct addresses in step with the endpoint's (interfaces
/// come and go; the port never changes while running).
fn refresh_addrs(
    time: Res<Time>,
    runtime: Res<crate::Runtime>,
    mut sync: ResMut<SyncNode>,
    mut settings: ResMut<Settings>,
) {
    if !sync.refresh.tick(time.delta()).just_finished() {
        return;
    }
    let addrs: Vec<String> = direct_addrs(&runtime.0.block_on(sync.node.addr()))
        .into_iter()
        .map(|a| a.to_string())
        .collect();
    settings.set_addrs(addrs);
}
