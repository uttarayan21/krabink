//! Pendant sync node: the one networking stack every device runs.
//!
//! A [`Node`] owns an iroh [`Endpoint`](iroh::Endpoint) (dialled by public
//! key, hole-punched, relayed only when a direct path fails), the local
//! document store ([`ServerDocs`] over redb) and a hub that fans updates
//! out between every connected peer. Inbound connections are served with
//! the sans-io [`pendant_core::ServerSession`]; outbound ones are driven
//! with [`pendant_core::ClientSession`]; the app on this device talks to its
//! own node through an in-process [`LocalLink`] with the same client
//! session, so app code never sees a socket.
//!
//! Wire: ALPN [`ALPN`], one QUIC connection per peer pair, two bidi
//! streams ("lanes"): docs and ephemeral wet ink (see [`framing`]).

pub mod docs;
pub mod framing;
mod hub;
mod identity;
mod inbound;
mod local;
#[cfg(feature = "mdns")]
pub mod mdns;
mod node;
mod outbound;
mod peers;
mod serve;
#[cfg(feature = "test-utils")]
pub mod testing;

pub use docs::{IdleDocs, ServerDocs};
pub use identity::load_or_create_secret_key;
pub use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey};
pub use local::LocalLink;
pub use node::{Node, NodeConfig, RelayHealth, RelayTarget, Role};
pub use peers::{PeerKind, PeerState, PeerStatus, PeerTarget, Route, direct_addrs, rank_ip};

/// Application-level protocol id every pendant node accepts.
pub const ALPN: &[u8] = b"pendant/sync/1";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Core(#[from] pendant_core::Error),
    #[error("node key: {0}")]
    Key(String),
    #[error("binding endpoint: {0}")]
    Bind(String),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;
