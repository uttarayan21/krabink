//! The desktop's sync node as a Bevy resource, plus LAN discovery: mDNS
//! advertises the node and feeds found peers to it as dial hints, and the
//! QR's direct addresses follow the endpoint's.

use std::time::Duration;

use bevy::prelude::*;
use pendant_core::{DeviceId, DocKey};
use pendant_local::mdns::Mdns;
use pendant_local::{Node, Unpaired, direct_addrs};
use tokio::sync::broadcast;

use crate::sync::LocalCommit;

use crate::docs::Docs;
use crate::settings::Settings;

#[derive(Resource)]
pub struct SyncNode {
    pub node: Node,
    mdns: Option<Mdns>,
    refresh: Timer,
    /// Peers that unpaired from us over the wire; drained into row removals.
    unpaired: broadcast::Receiver<Unpaired>,
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
        let unpaired = node.watch_unpaired();
        Self {
            node,
            mdns,
            refresh: Timer::new(Duration::from_secs(5), TimerMode::Repeating),
            unpaired,
        }
    }

    pub fn mdns_name(&self) -> Option<&str> {
        self.mdns.as_ref().map(Mdns::name)
    }

    /// Unpair from `device`: tell it over the wire and stop dialling it.
    /// The peer forgets this desktop in turn.
    pub fn unpair(&self, runtime: &tokio::runtime::Runtime, device: DeviceId) {
        runtime.block_on(self.node.unpair(device));
    }

    /// Re-advertise under a new device name (drops the old registration
    /// first; the node itself is untouched).
    pub fn readvertise(&mut self, runtime: &tokio::runtime::Runtime, name: &str) {
        self.mdns = None;
        let Some(port) = runtime.block_on(self.node.bound_port()) else {
            return;
        };
        self.mdns = Mdns::start(self.node.id(), port, name)
            .map_err(|err| tracing::warn!(%err, "mDNS re-advertise failed"))
            .ok();
    }
}

pub struct NodePlugin;

impl Plugin for NodePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (poll_discovery, poll_unpaired, refresh_addrs));
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

/// A peer told us it unpaired: forget its row so it leaves our device
/// list. The node already dropped the connection; nothing to redial.
fn poll_unpaired(
    mut sync: ResMut<SyncNode>,
    mut docs: ResMut<Docs>,
    mut commits: MessageWriter<LocalCommit>,
) {
    loop {
        match sync.unpaired.try_recv() {
            Ok(event) => match docs.remove_device(event.device) {
                Ok(payload) if !payload.is_empty() => {
                    commits.write(LocalCommit {
                        doc: DocKey::WORKSPACE,
                        payload,
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "removing unpaired device failed"),
            },
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
        }
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
