//! Device pairing via a shareable URI:
//! `pendant://pair?server=…&token=…[&fallback=…]`.
//! One device renders the URI as a QR code; the other opens it and adopts
//! the sync server + token. The URI carries no identity — possession of the
//! token is the whole credential, same trust model as the config file.
//!
//! `server` is the direct path (the sharing desktop's embedded relay on the
//! LAN); `fallback` is an optional dedicated relay to route through when the
//! direct path is unreachable. Both accept the same token.

/// Sync coordinates carried by a pairing URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairInfo {
    /// Relay endpoint, e.g. `ws://192.168.0.162:8722/ws`.
    pub server: String,
    /// Bearer token the relay accepts.
    pub token: String,
    /// Dedicated relay to use when `server` cannot be reached.
    pub fallback: Option<String>,
}

const SCHEME_AND_PATH: &str = "pendant://pair?";

impl PairInfo {
    pub fn to_uri(&self) -> String {
        let mut uri = format!(
            "{SCHEME_AND_PATH}server={}&token={}",
            percent_encode(&self.server),
            percent_encode(&self.token)
        );
        if let Some(fallback) = &self.fallback {
            uri.push_str("&fallback=");
            uri.push_str(&percent_encode(fallback));
        }
        uri
    }

    /// Every endpoint to try, direct path first.
    pub fn endpoints(&self) -> Vec<&str> {
        std::iter::once(self.server.as_str())
            .chain(self.fallback.as_deref())
            .collect()
    }

    /// Parse a pairing URI. Returns `None` for anything that is not a
    /// well-formed `pendant://pair` URI with both parameters present.
    pub fn parse(uri: &str) -> Option<Self> {
        let query = uri.strip_prefix(SCHEME_AND_PATH)?;
        let mut server = None;
        let mut token = None;
        let mut fallback = None;
        for kv in query.split('&') {
            let (key, value) = kv.split_once('=')?;
            match key {
                "server" => server = Some(percent_decode(value)?),
                "token" => token = Some(percent_decode(value)?),
                "fallback" => fallback = Some(percent_decode(value)?),
                _ => {} // ignore unknown params so the format can grow
            }
        }
        Some(Self {
            server: server.filter(|s| !s.is_empty())?,
            token: token?,
            fallback: fallback.filter(|s| !s.is_empty()),
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

    #[test]
    fn roundtrip() {
        let info = PairInfo {
            server: "ws://192.168.0.162:8722/ws".into(),
            token: "demo".into(),
            fallback: None,
        };
        let uri = info.to_uri();
        assert_eq!(
            uri,
            "pendant://pair?server=ws%3A%2F%2F192.168.0.162%3A8722%2Fws&token=demo"
        );
        assert_eq!(PairInfo::parse(&uri), Some(info));
    }

    #[test]
    fn roundtrip_awkward_token() {
        let info = PairInfo {
            server: "wss://relay.example.com/ws".into(),
            token: "a&b=c %/ü".into(),
            fallback: None,
        };
        assert_eq!(PairInfo::parse(&info.to_uri()), Some(info));
    }

    #[test]
    fn roundtrip_with_fallback() {
        let info = PairInfo {
            server: "ws://192.168.0.162:8722/ws".into(),
            token: "demo".into(),
            fallback: Some("wss://relay.example.com/ws".into()),
        };
        let uri = info.to_uri();
        assert!(uri.ends_with("&fallback=wss%3A%2F%2Frelay.example.com%2Fws"));
        assert_eq!(PairInfo::parse(&uri), Some(info.clone()));
        assert_eq!(
            info.endpoints(),
            vec!["ws://192.168.0.162:8722/ws", "wss://relay.example.com/ws"]
        );
        // Empty fallback reads as absent.
        let parsed = PairInfo::parse("pendant://pair?server=ws%3A%2F%2Fh%2Fws&token=t&fallback=");
        assert_eq!(parsed.and_then(|p| p.fallback), None);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(PairInfo::parse("https://example.com"), None);
        assert_eq!(PairInfo::parse("pendant://pair?token=x"), None); // no server
        assert_eq!(PairInfo::parse("pendant://pair?server=&token=x"), None);
        assert_eq!(PairInfo::parse("pendant://pair?server=ws%GG&token=x"), None);
    }

    #[test]
    fn ignores_unknown_params() {
        let parsed = PairInfo::parse("pendant://pair?server=ws%3A%2F%2Fh%2Fws&token=t&v=2");
        assert_eq!(
            parsed,
            Some(PairInfo {
                server: "ws://h/ws".into(),
                token: "t".into(),
                fallback: None,
            })
        );
    }
}
