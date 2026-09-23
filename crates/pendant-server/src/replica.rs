//! The always-on replica: a headless node behind our own relay that
//! accepts every device with a valid token and mirrors every doc.

use std::path::Path;

use iroh::RelayUrl;
use pendant_core::DeviceId;
use pendant_local::{Node, NodeConfig, RelayTarget, Role};

use crate::config::ReplicaConfig;
use crate::errors::{Error, Result, ResultExt};

/// Start the replica node. `relay` is our public relay URL; the replica
/// authenticates to it with the first workspace token.
pub async fn start(cfg: &ReplicaConfig, relay: RelayUrl, tokens: Vec<String>) -> Result<Node> {
    let token = tokens
        .first()
        .cloned()
        .ok_or_else(|| error_stack::Report::new(Error).attach("replica needs a token"))?;
    let device = load_or_create_device_id(&cfg.key.with_extension("device"))?;
    Node::start(NodeConfig {
        key_path: cfg.key.clone(),
        store_path: cfg.db.clone(),
        device,
        tokens,
        relay: Some(RelayTarget { url: relay, token }),
        bind_port: cfg.udp_port,
        role: Role::Replica,
    })
    .await
    .change_context(Error)
    .attach("starting replica node")
}

fn load_or_create_device_id(path: &Path) -> Result<DeviceId> {
    if let Ok(text) = std::fs::read_to_string(path) {
        return text
            .trim()
            .parse()
            .map_err(|_| error_stack::Report::new(Error))
            .attach_with(|| format!("malformed device id in {}", path.display()));
    }
    let id = DeviceId::new();
    std::fs::write(path, id.to_string())
        .change_context(Error)
        .attach_with(|| format!("writing {}", path.display()))?;
    Ok(id)
}
