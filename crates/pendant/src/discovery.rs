//! Desktop-side relay discovery, the counterpart of the iPad's
//! `RelayDiscovery.swift`: when this desktop joined another desktop's
//! workspace, browse `_pendant._tcp` for that desktop's device id (TXT
//! `id=`) and repoint the direct link whenever it turns up at a new
//! address or port. Needed because embedded relays take a fresh ephemeral
//! port per launch, so a persisted `ws://ip:port/ws` goes stale on the
//! paired desktop's next restart. Only works where multicast reaches (same
//! LAN); elsewhere the stored address or the dedicated relay carries on.

use std::net::Ipv4Addr;

use bevy::prelude::*;
use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};
use pendant_core::{DeviceId, PairInfo};

use crate::relay::SERVICE_TYPE;
use crate::settings::Settings;
use crate::sync::SyncTransport;

/// The desktop whose workspace this one joined: its device id (what mDNS
/// matches on) and the direct-path coordinates currently in use. Absent
/// when this desktop only serves its own relay or syncs through a
/// dedicated one without a paired desktop.
#[derive(Resource, Clone, Debug)]
pub struct PairedDesktop {
    pub relay_id: String,
    /// Direct url of the paired desktop's embedded relay.
    pub server: String,
    pub token: String,
    /// Dedicated relay shared by the workspace, if any.
    pub fallback: Option<String>,
}

impl PairedDesktop {
    /// From adopted pairing coordinates; `None` when the URI carried no
    /// relay id (a dedicated relay's URI rather than a desktop's).
    pub fn from_pair(info: &PairInfo) -> Option<Self> {
        Some(Self {
            relay_id: info.relay_id.clone()?,
            server: info.server.clone(),
            token: info.token.clone(),
            fallback: info.fallback.clone(),
        })
    }

    fn as_pair(&self) -> PairInfo {
        PairInfo {
            server: self.server.clone(),
            token: self.token.clone(),
            fallback: self.fallback.clone(),
            alt: Vec::new(),
            relay_id: Some(self.relay_id.clone()),
        }
    }
}

/// Browser state; lives whether or not a desktop is paired so pairing
/// later (settings window) just starts it.
#[derive(Resource)]
pub struct RelayFinder {
    own: String,
    daemon: Option<ServiceDaemon>,
    events: Option<Receiver<ServiceEvent>>,
    /// Relay id the running browse is for.
    browsing_for: Option<String>,
    /// Set after the daemon failed to start; we do not retry every frame.
    disabled: bool,
    /// Last direct url mDNS resolved for the paired desktop.
    pub last_found: Option<String>,
}

impl RelayFinder {
    pub fn new(own: DeviceId) -> Self {
        Self {
            own: own.to_string(),
            daemon: None,
            events: None,
            browsing_for: None,
            disabled: false,
            last_found: None,
        }
    }

    fn ensure_browsing(&mut self, relay_id: &str) {
        if self.disabled || self.browsing_for.as_deref() == Some(relay_id) {
            return;
        }
        self.stop();
        let daemon = match self
            .daemon
            .take()
            .map(Ok)
            .unwrap_or_else(ServiceDaemon::new)
        {
            Ok(daemon) => daemon,
            Err(err) => {
                tracing::warn!(%err, "mDNS browsing unavailable; paired desktop found by stored address only");
                self.disabled = true;
                return;
            }
        };
        match daemon.browse(SERVICE_TYPE) {
            Ok(events) => {
                tracing::info!(relay = relay_id, "browsing mDNS for the paired desktop");
                self.events = Some(events);
                self.browsing_for = Some(relay_id.to_string());
            }
            Err(err) => {
                tracing::warn!(%err, "mDNS browse failed; paired desktop found by stored address only");
                self.disabled = true;
            }
        }
        self.daemon = Some(daemon);
    }

    fn stop(&mut self) {
        if self.browsing_for.take().is_some()
            && let Some(daemon) = &self.daemon
            && let Err(err) = daemon.stop_browse(SERVICE_TYPE)
        {
            tracing::debug!(%err, "stopping mDNS browse");
        }
        self.events = None;
        self.last_found = None;
    }

    /// Drain pending events; the newest direct url for `relay_id`, if any
    /// resolved this frame.
    fn poll(&self, relay_id: &str) -> Option<String> {
        let events = self.events.as_ref()?;
        let mut found = None;
        while let Ok(event) = events.try_recv() {
            if let ServiceEvent::ServiceResolved(info) = event
                && info.get_properties().get_property_val_str("id") == Some(relay_id)
                && let Some(url) = direct_url(&info)
            {
                found = Some(url);
            }
        }
        found
    }
}

impl Drop for RelayFinder {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

/// `ws://<best v4 address>:<port><path>` for a resolved service, ranking
/// addresses like our own advertisement (LAN first, then overlays).
fn direct_url(info: &mdns_sd::ResolvedService) -> Option<String> {
    let ip: Ipv4Addr = info
        .get_addresses_v4()
        .into_iter()
        .filter(|ip| !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified())
        .min_by_key(|ip| (crate::relay::rank(ip), ip.octets()))?;
    let path = info
        .get_properties()
        .get_property_val_str("path")
        .filter(|p| p.starts_with('/'))
        .unwrap_or("/ws");
    Some(format!("ws://{ip}:{}{path}", info.get_port()))
}

pub struct DiscoveryPlugin;

impl Plugin for DiscoveryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, find_paired_desktop);
    }
}

/// Keep the browse aligned with the paired desktop (start/stop/re-target)
/// and swap the direct link when mDNS reports a different address.
fn find_paired_desktop(
    mut finder: ResMut<RelayFinder>,
    paired: Option<ResMut<PairedDesktop>>,
    runtime: Res<crate::Runtime>,
    mut transport: ResMut<SyncTransport>,
    mut settings: ResMut<Settings>,
) {
    let Some(mut paired) = paired else {
        finder.stop();
        return;
    };
    // Pasting our own QR pairs us with ourselves; nothing to find.
    if paired.relay_id == finder.own {
        return;
    }
    finder.ensure_browsing(&paired.relay_id);
    let Some(url) = finder.poll(&paired.relay_id) else {
        return;
    };
    finder.last_found = Some(url.clone());
    if url == paired.server {
        return;
    }
    tracing::info!(
        relay = %paired.relay_id,
        from = %paired.server,
        to = %url,
        "paired desktop re-found over mDNS; repointing direct link"
    );
    let previous = std::mem::replace(&mut paired.server, url.clone());
    transport.replace_remotes(
        runtime.0.handle(),
        std::iter::once(url.clone()).chain(paired.fallback.clone()),
        &paired.token,
    );
    settings.direct_repointed(&previous, &url);
    // Persist so the next launch starts from the fresh address; browsing
    // would fix a stale one anyway, this just saves the round trip.
    if let Err(err) = crate::config::persist_pair(&paired.as_pair()) {
        tracing::error!(%err, "persisting re-found pairing failed");
    }
}
