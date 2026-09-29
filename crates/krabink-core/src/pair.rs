//! Device pairing via a shareable URI:
//! `krabink://pair?node=…&token=…[&relay=…][&addr=…]*[&replica=…]`.
//! One device renders the URI as a QR code; the other opens it and dials
//! the node. The URI carries no proof of identity beyond the node's public
//! key — possession of the token is the whole credential, same trust model
//! as the config file.
//!
//! `node` is the sharing device's endpoint id (its public key); `relay` is
//! the home relay the handshake goes through and the fallback path when
//! hole punching fails; every `addr` is a direct `ip:port` hint (LAN,
//! Tailscale…) that lets peers connect without the relay; `replica` is the
//! always-on cloud node of the same workspace, if there is one. All accept
//! the same token.

/// The relay every node homes on when none is configured. The same origin
/// serves the web build, so a browser reaches it without CORS or mixed
/// content. It admits anyone; the workspace token is checked by the peers.
pub const DEFAULT_RELAY: &str = "https://krabink.darksailor.dev";

/// Sync coordinates carried by a pairing URI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PairInfo {
    /// Endpoint id (public key) of the sharing device's node.
    pub node: String,
    /// Workspace token every node and the relay accept.
    pub token: String,
    /// Home relay URL, e.g. `https://relay.example.org`.
    pub relay: Option<String>,
    /// Direct `ip:port` hints for the node, best first.
    pub addrs: Vec<String>,
    /// Endpoint id of the workspace's cloud replica.
    pub replica: Option<String>,
}

const SCHEME_AND_PATH: &str = "krabink://pair?";
/// Fragment that carries a pairing URI to the web build. A fragment never
/// reaches the server, so the token stays out of its logs.
const WEB_FRAGMENT: &str = "#pair=";

impl PairInfo {
    pub fn to_uri(&self) -> String {
        let mut uri = format!(
            "{SCHEME_AND_PATH}node={}&token={}",
            percent_encode(&self.node),
            percent_encode(&self.token)
        );
        if let Some(relay) = &self.relay {
            uri.push_str("&relay=");
            uri.push_str(&percent_encode(relay));
        }
        for addr in &self.addrs {
            uri.push_str("&addr=");
            uri.push_str(&percent_encode(addr));
        }
        if let Some(replica) = &self.replica {
            uri.push_str("&replica=");
            uri.push_str(&percent_encode(replica));
        }
        uri
    }

    /// A link that opens the web build at `base` (e.g. [`DEFAULT_RELAY`])
    /// already paired with this node.
    pub fn web_link(&self, base: &str) -> String {
        format!(
            "{}/{WEB_FRAGMENT}{}",
            base.trim_end_matches('/'),
            percent_encode(&self.to_uri())
        )
    }

    /// The pairing a web link's fragment (`location.hash`) carries.
    pub fn from_web_fragment(fragment: &str) -> Option<Self> {
        Self::parse(&percent_decode(fragment.strip_prefix(WEB_FRAGMENT)?)?)
    }

    /// Parse a pairing URI. Returns `None` for anything that is not a
    /// well-formed `krabink://pair` URI with `node` and `token` present.
    pub fn parse(uri: &str) -> Option<Self> {
        let query = uri.strip_prefix(SCHEME_AND_PATH)?;
        let mut node = None;
        let mut token = None;
        let mut relay = None;
        let mut replica = None;
        let mut addrs = Vec::new();
        for kv in query.split('&') {
            let (key, value) = kv.split_once('=')?;
            match key {
                "node" => node = Some(percent_decode(value)?),
                "token" => token = Some(percent_decode(value)?),
                "relay" => relay = Some(percent_decode(value)?),
                "replica" => replica = Some(percent_decode(value)?),
                "addr" => {
                    let value = percent_decode(value)?;
                    if !value.is_empty() {
                        addrs.push(value);
                    }
                }
                _ => {} // ignore unknown params so the format can grow
            }
        }
        Some(Self {
            node: node.filter(|s| !s.is_empty())?,
            token: token?,
            relay: relay.filter(|s| !s.is_empty()),
            addrs,
            replica: replica.filter(|s| !s.is_empty()),
        })
    }
}

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(raw: &str) -> Option<String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => {
                let hi = bytes.next()?;
                let lo = bytes.next()?;
                let hex = [hi, lo];
                let hex = core::str::from_utf8(&hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
            }
            _ => out.push(byte),
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "7671c8b3d3d241b0abd418429c2257f91037b76e08af827b7cf10b95bddfa31e";

    #[test]
    fn roundtrip() {
        let info = PairInfo {
            node: NODE.into(),
            token: "demo".into(),
            ..Default::default()
        };
        let uri = info.to_uri();
        assert_eq!(uri, format!("krabink://pair?node={NODE}&token=demo"));
        assert_eq!(PairInfo::parse(&uri), Some(info));
    }

    #[test]
    fn roundtrip_awkward_token() {
        let info = PairInfo {
            node: NODE.into(),
            token: "a&b=c %/ü".into(),
            ..Default::default()
        };
        assert_eq!(PairInfo::parse(&info.to_uri()), Some(info));
    }

    #[test]
    fn roundtrip_full() {
        let info = PairInfo {
            node: NODE.into(),
            token: "demo".into(),
            relay: Some("https://relay.example.org".into()),
            addrs: vec!["192.168.0.188:7842".into(), "100.78.171.80:7842".into()],
            replica: Some("aa".repeat(32)),
        };
        let parsed = PairInfo::parse(&info.to_uri()).unwrap();
        assert_eq!(parsed, info);
        assert!(
            info.to_uri()
                .contains("&relay=https%3A%2F%2Frelay.example.org")
        );
        // Empty entries read as absent.
        let parsed = PairInfo::parse(&format!(
            "krabink://pair?node={NODE}&token=t&addr=&relay=&replica="
        ))
        .unwrap();
        assert!(parsed.addrs.is_empty());
        assert_eq!(parsed.relay, None);
        assert_eq!(parsed.replica, None);
    }

    #[test]
    fn web_link_roundtrip() {
        let info = PairInfo {
            node: NODE.into(),
            token: "a&b=c %/ü".into(),
            relay: Some(DEFAULT_RELAY.into()),
            ..Default::default()
        };
        let link = info.web_link("https://krabink.darksailor.dev/");
        let (base, fragment) = link.split_once('#').unwrap();
        assert_eq!(base, "https://krabink.darksailor.dev/");
        assert!(!fragment.contains('&'), "{fragment}");
        assert_eq!(
            PairInfo::from_web_fragment(&format!("#{fragment}")),
            Some(info)
        );
        assert_eq!(PairInfo::from_web_fragment("#other"), None);
        assert_eq!(PairInfo::from_web_fragment(""), None);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(PairInfo::parse("https://example.com"), None);
        assert_eq!(PairInfo::parse("krabink://pair?token=x"), None); // no node
        assert_eq!(PairInfo::parse("krabink://pair?node=&token=x"), None);
        assert_eq!(PairInfo::parse("krabink://pair?node=ab%GG&token=x"), None);
    }

    #[test]
    fn ignores_unknown_params() {
        let parsed = PairInfo::parse(&format!("krabink://pair?node={NODE}&token=t&v=2"));
        assert_eq!(
            parsed,
            Some(PairInfo {
                node: NODE.into(),
                token: "t".into(),
                ..Default::default()
            })
        );
    }
}
