//! Library surface of the krabink cloud server, split from the binary so
//! integration tests can spawn the real relay on an ephemeral port.
//!
//! Two halves: the **relay** (an iroh relay server admitting only
//! connections that present a workspace token) and the optional
//! **replica** (a headless [`krabink_local::Node`] that mirrors every doc
//! so devices sync while the others are offline).

pub mod config;
pub mod errors;
pub mod relay;
pub mod replica;

pub use config::{Config, RelayOpts, ReplicaConfig, TlsOpts};
