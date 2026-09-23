//! The iroh relay with workspace-token access control.

use std::sync::Arc;

use iroh_relay::server::{
    Access, AccessControl, AcmeConfig, CertConfig, ClientRequest, QuicConfig,
    RelayConfig as RelayServerConfig, Server, ServerConfig, TlsConfig,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::{RelayOpts, TlsOpts};
use crate::errors::{Error, Result, ResultExt};

/// Admit a relay client only if it presents one of the workspace tokens.
#[derive(Debug)]
pub struct TokenAccess(pub Vec<String>);

impl AccessControl for TokenAccess {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        match request.auth_token() {
            Some(token) if self.0.contains(&token) => Access::Allow,
            _ => Access::Deny {
                reason: Some("not authorized".into()),
            },
        }
    }
}

/// Spawn the relay. Drop or `shutdown()` the returned server to stop it.
pub async fn spawn(opts: RelayOpts, tokens: Vec<String>) -> Result<Server> {
    // The relay's TLS stack wants a process-wide provider; harmless if the
    // embedding process already installed one.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut relay = RelayServerConfig::new(opts.http_listen);
    relay.key_cache_capacity = Some(1024);
    relay.access = Arc::new(TokenAccess(tokens));
    relay.tls = match &opts.tls {
        Some(tls) => Some(tls_config(tls).await?),
        None => None,
    };

    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    config.quic = opts.quic_listen.map(QuicConfig::new);
    Server::spawn(config)
        .await
        .change_context(Error)
        .attach("spawning relay")
}

async fn tls_config(tls: &TlsOpts) -> Result<TlsConfig> {
    let builder = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .change_context(Error)
    .attach("tls protocol versions")?
    .with_no_client_auth();
    match tls {
        TlsOpts::Manual {
            https_listen,
            cert,
            key,
        } => {
            let certs = CertificateDer::pem_file_iter(cert)
                .change_context(Error)
                .attach_with(|| format!("reading {}", cert.display()))?
                .collect::<Result<Vec<_>, _>>()
                .change_context(Error)
                .attach_with(|| format!("parsing {}", cert.display()))?;
            let key = PrivateKeyDer::from_pem_file(key)
                .change_context(Error)
                .attach_with(|| format!("reading {}", key.display()))?;
            let server_config = builder
                .with_single_cert(certs, key)
                .change_context(Error)
                .attach("tls certificate")?;
            Ok(TlsConfig::new(
                *https_listen,
                CertConfig::Manual { server_config },
            ))
        }
        TlsOpts::LetsEncrypt {
            https_listen,
            hostname,
            contact,
            prod,
            cache_dir,
        } => {
            let acme_config = AcmeConfig::letsencrypt(*prod)
                .domains(vec![hostname.clone()])
                .contact(vec![format!("mailto:{contact}")])
                .cache_path(cache_dir.clone());
            Ok(TlsConfig::new(
                *https_listen,
                CertConfig::LetsEncrypt {
                    acme_config,
                    server_config_builder: builder,
                },
            ))
        }
    }
}
