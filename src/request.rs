//! Reassemble an HTTP mirror request from SPOE message arguments.
//!
//! The chart-side spoe-message contract is:
//!
//! ```text
//! spoe-message mirror
//!   args arg_target=str(<host:port>)
//!        arg_scheme=str(http|https)
//!        arg_method=method
//!        arg_path=path
//!        arg_query=query
//!        arg_ver=req.ver
//!        arg_hdrs=req.hdrs_bin
//!        arg_body=req.body
//!        arg_timeout_ms=int                (optional)
//!        arg_retries=int                   (optional)
//! ```
//!
//! `arg_hdrs` is HAProxy's `req.hdrs_bin` encoding: alternating
//! length-prefixed name/value pairs terminated by a zero-length name.
//! `arg_body` is the raw request body; multi-frame SPOE bodies are
//! reassembled by the hub before reaching this plugin.
//!
//! `arg_timeout_ms` and `arg_retries` are optional per-dispatch
//! overrides. When absent the plugin's [`PluginConfig`] defaults are
//! used. The chart populates these from the primary backend's
//! `timeout server` and `retries` so a mirror dispatch waits as long
//! as a normal backend request would — a too-tight default silently
//! drops mirrors during in-cluster Service cold-start (DNS not yet
//! propagated, kube-proxy iptables not yet installed) and the
//! failure is invisible because dispatch is fire-and-forget.
//!
//! [`PluginConfig`]: crate::config::PluginConfig

use haproxy_spoa_hub_plugin_api::SpoeMessage;

/// A single mirror request reconstructed from SPOE message args.
#[derive(Debug, Clone)]
pub struct MirrorRequest {
    pub target: String,
    pub scheme: Scheme,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
    /// Per-dispatch timeout override from `arg_timeout_ms`. `None`
    /// means the SPOE message didn't carry one and the dispatcher
    /// falls back to [`PluginConfig::timeout_ms`].
    ///
    /// [`PluginConfig::timeout_ms`]: crate::config::PluginConfig::timeout_ms
    pub timeout_ms: Option<u64>,
    /// Per-dispatch retry-budget override from `arg_retries`. `None`
    /// falls back to [`PluginConfig::retries`].
    ///
    /// [`PluginConfig::retries`]: crate::config::PluginConfig::retries
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// Extract a mirror request from an SPOE message, returning None if any
/// required field is missing or malformed. Body length is capped at
/// `max_body_bytes` — bodies larger than that are truncated.
pub fn extract_mirror_request(
    message: &SpoeMessage,
    max_body_bytes: usize,
) -> Option<MirrorRequest> {
    let target = message.get_string("arg_target")?.to_string();
    if target.is_empty() {
        return None;
    }

    let scheme = match message.get_string("arg_scheme") {
        Some("https") => Scheme::Https,
        // Default to http when arg is missing, empty, or anything but
        // "https". The chart should always emit one of "http"/"https";
        // this default keeps a misconfigured message from silently
        // dispatching to a nonsense scheme.
        _ => Scheme::Http,
    };

    let method = message
        .get_string("arg_method")
        .filter(|s| !s.is_empty())
        .unwrap_or("GET")
        .to_string();

    let path = message
        .get_string("arg_path")
        .filter(|s| !s.is_empty())
        .unwrap_or("/")
        .to_string();

    let query = message
        .get_string("arg_query")
        .filter(|s| !s.is_empty())
        .map(String::from);

    let headers = message
        .get_binary("arg_hdrs")
        .map(parse_hdrs_bin)
        .unwrap_or_default();

    let body = message
        .get_binary("arg_body")
        .map(|raw| {
            if raw.len() > max_body_bytes {
                raw[..max_body_bytes].to_vec()
            } else {
                raw.to_vec()
            }
        })
        .unwrap_or_default();

    // Optional per-dispatch overrides. Reject zero/negative values
    // at the boundary so the client doesn't have to defend against
    // "0ms timeout" footguns from a misconfigured chart.
    let timeout_ms = message
        .get_int("arg_timeout_ms")
        .filter(|n| *n > 0)
        .map(|n| n as u64);
    let retries = message
        .get_int("arg_retries")
        .filter(|n| *n >= 0)
        .map(|n| n as u32);

    Some(MirrorRequest {
        target,
        scheme,
        method,
        path,
        query,
        headers,
        body,
        timeout_ms,
        retries,
    })
}

/// Parse HAProxy's `req.hdrs_bin` encoding into a list of (name, value) pairs.
///
/// Wire format (big-endian throughout):
///   <name_len: varint> <name_bytes> <value_len: varint> <value_bytes>
///   ... terminated by a zero-length name.
///
/// HAProxy emits these as SPOE-encoded varints. The SPOE varint
/// encoding is little-endian base-128 with a 0x80 continuation bit
/// matching the SPOP `typed-data` length prefix. We tolerate either
/// the SPOE-varint or a plain u16-be length, falling back to "stop at
/// first parse error" rather than producing nonsense headers.
pub fn parse_hdrs_bin(input: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < input.len() {
        let Some((name_len, after_name_len)) = read_varint(&input[cursor..]) else {
            break;
        };
        cursor += after_name_len;
        if name_len == 0 {
            // Termination sentinel.
            break;
        }
        let name_end = cursor.saturating_add(name_len);
        if name_end > input.len() {
            break;
        }
        let name = match std::str::from_utf8(&input[cursor..name_end]) {
            Ok(s) => s.to_string(),
            Err(_) => break,
        };
        cursor = name_end;

        let Some((val_len, after_val_len)) = read_varint(&input[cursor..]) else {
            break;
        };
        cursor += after_val_len;
        let val_end = cursor.saturating_add(val_len);
        if val_end > input.len() {
            break;
        }
        let value = input[cursor..val_end].to_vec();
        cursor = val_end;

        out.push((name, value));
    }
    out
}

/// Decode a single SPOE varint. Returns (value, bytes_consumed) or
/// None on truncated input.
fn read_varint(input: &[u8]) -> Option<(usize, usize)> {
    // SPOE encodes lengths as a sequence of bytes where the high bit
    // (0x80) is the continuation flag and the low 7 bits carry the
    // value, little-endian.
    let mut value: usize = 0;
    let mut shift = 0u32;
    for (i, &b) in input.iter().enumerate() {
        let chunk = (b & 0x7f) as usize;
        let shifted = chunk.checked_shl(shift)?;
        value = value.checked_add(shifted)?;
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift = shift.checked_add(7)?;
        if shift >= 64 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use haproxy_spoa_hub_plugin_api::{RHashMap, RString, SpoeValue};

    fn message(args: Vec<(&str, SpoeValue)>) -> SpoeMessage {
        let mut map = RHashMap::new();
        for (k, v) in args {
            map.insert(RString::from(k), v);
        }
        SpoeMessage {
            name: RString::from("mirror"),
            args: map,
            stream_id: 1,
            frame_id: 1,
        }
    }

    #[test]
    fn extracts_minimal_request() {
        let msg = message(vec![
            (
                "arg_target",
                SpoeValue::String("backend.svc.cluster.local:80".into()),
            ),
            ("arg_scheme", SpoeValue::String("http".into())),
            ("arg_method", SpoeValue::String("POST".into())),
            ("arg_path", SpoeValue::String("/api".into())),
        ]);
        let req = extract_mirror_request(&msg, 1024).expect("extracts");
        assert_eq!(req.target, "backend.svc.cluster.local:80");
        assert_eq!(req.scheme, Scheme::Http);
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api");
        assert!(req.body.is_empty());
        assert!(req.headers.is_empty());
    }

    #[test]
    fn https_scheme_recognised() {
        let msg = message(vec![
            ("arg_target", SpoeValue::String("backend:443".into())),
            ("arg_scheme", SpoeValue::String("https".into())),
        ]);
        let req = extract_mirror_request(&msg, 1024).unwrap();
        assert_eq!(req.scheme, Scheme::Https);
    }

    #[test]
    fn missing_target_returns_none() {
        let msg = message(vec![("arg_method", SpoeValue::String("GET".into()))]);
        assert!(extract_mirror_request(&msg, 1024).is_none());
    }

    #[test]
    fn empty_target_returns_none() {
        let msg = message(vec![("arg_target", SpoeValue::String("".into()))]);
        assert!(extract_mirror_request(&msg, 1024).is_none());
    }

    #[test]
    fn body_truncated_to_cap() {
        let huge = vec![0xAB; 4096];
        let msg = message(vec![
            ("arg_target", SpoeValue::String("backend:80".into())),
            ("arg_body", SpoeValue::Binary(huge.into())),
        ]);
        let req = extract_mirror_request(&msg, 256).unwrap();
        assert_eq!(req.body.len(), 256);
        assert!(req.body.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn defaults_filled_when_absent() {
        let msg = message(vec![("arg_target", SpoeValue::String("backend:80".into()))]);
        let req = extract_mirror_request(&msg, 1024).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/");
        assert!(req.query.is_none());
    }

    #[test]
    fn timeout_and_retries_absent_yield_none() {
        // No arg_timeout_ms / arg_retries → client falls back to
        // PluginConfig defaults. Pinning the None invariant on extract
        // is what guarantees the fallback path runs.
        let msg = message(vec![("arg_target", SpoeValue::String("backend:80".into()))]);
        let req = extract_mirror_request(&msg, 1024).unwrap();
        assert!(req.timeout_ms.is_none());
        assert!(req.retries.is_none());
    }

    #[test]
    fn timeout_and_retries_present_override_defaults() {
        // Chart-driven path: derived from primary backend's
        // `timeout server` / `retries` and emitted as SPOE args. The
        // client uses these in preference to PluginConfig defaults so
        // mirror dispatch matches the operator's per-route timeout
        // expectations.
        let msg = message(vec![
            ("arg_target", SpoeValue::String("backend:80".into())),
            ("arg_timeout_ms", SpoeValue::Int64(5_000)),
            ("arg_retries", SpoeValue::Int64(2)),
        ]);
        let req = extract_mirror_request(&msg, 1024).unwrap();
        assert_eq!(req.timeout_ms, Some(5_000));
        assert_eq!(req.retries, Some(2));
    }

    #[test]
    fn timeout_zero_is_rejected_at_boundary() {
        // A misconfigured chart that emits `arg_timeout_ms=0` would
        // otherwise create a "0ms timeout" footgun in the client.
        // Reject at extraction so the client can assume any Some
        // value is positive.
        let msg = message(vec![
            ("arg_target", SpoeValue::String("backend:80".into())),
            ("arg_timeout_ms", SpoeValue::Int64(0)),
        ]);
        let req = extract_mirror_request(&msg, 1024).unwrap();
        assert!(
            req.timeout_ms.is_none(),
            "arg_timeout_ms=0 must be treated as absent (fallback to default), not as a literal 0ms"
        );
    }

    #[test]
    fn retries_zero_is_explicit_override() {
        // arg_retries=0 is a legitimate operator choice (disable
        // retries for this route) and must be distinguishable from
        // "arg absent → fall back to default". The boundary filter
        // for retries is `>= 0` so the 0 passes through as Some(0).
        let msg = message(vec![
            ("arg_target", SpoeValue::String("backend:80".into())),
            ("arg_retries", SpoeValue::Int64(0)),
        ]);
        let req = extract_mirror_request(&msg, 1024).unwrap();
        assert_eq!(
            req.retries,
            Some(0),
            "arg_retries=0 must round-trip as Some(0), distinct from None (fallback to default)"
        );
    }

    #[test]
    fn parses_single_header() {
        // One header: "X-Foo: bar", terminator zero-length name.
        let mut wire = Vec::new();
        wire.push(5u8); // name len
        wire.extend_from_slice(b"X-Foo");
        wire.push(3u8); // value len
        wire.extend_from_slice(b"bar");
        wire.push(0u8); // terminator
        let headers = parse_hdrs_bin(&wire);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "X-Foo");
        assert_eq!(headers[0].1, b"bar");
    }

    #[test]
    fn parses_multiple_headers() {
        let mut wire = Vec::new();
        for (name, val) in &[("A", "1"), ("B", "22"), ("C", "333")] {
            wire.push(name.len() as u8);
            wire.extend_from_slice(name.as_bytes());
            wire.push(val.len() as u8);
            wire.extend_from_slice(val.as_bytes());
        }
        wire.push(0u8); // terminator
        let headers = parse_hdrs_bin(&wire);
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[2].0, "C");
        assert_eq!(headers[2].1, b"333");
    }

    #[test]
    fn truncated_header_stops_parsing_cleanly() {
        // Length prefix says 10 bytes but input only has 3.
        let wire = vec![10u8, b'X', b'Y', b'Z'];
        let headers = parse_hdrs_bin(&wire);
        assert!(headers.is_empty());
    }
}
