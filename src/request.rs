//! Reassemble one or more HTTP mirror requests from SPOE message arguments.
//!
//! The chart-side spoe-message contract is a **single static** message
//! that fans out to every target for the request:
//!
//! ```text
//! spoe-message mirror
//!   args arg_targets=str(<entry>;<entry>;...)
//!        arg_method=method
//!        arg_path=path
//!        arg_query=query
//!        arg_ver=req.ver
//!        arg_hdrs=req.hdrs_bin
//!        arg_body=req.body
//! ```
//!
//! `arg_targets` is a `;`-delimited list — the chart appends one entry
//! per matching mirror source (`http-request set-var(txn.gw_mirror_targets)
//! str(<entry>;),concat(,txn.gw_mirror_targets,) if <cond>`), so adding a
//! mirror target only changes the frontend, never this plugin's config or
//! `spoe.conf`. Each entry is pipe-delimited:
//!
//! ```text
//!   <scheme>|<host:port>|<timeout_ms>|<retries>
//! ```
//!
//! `scheme` is `http`/`https` (anything else → `http`). `timeout_ms` and
//! `retries` are per-target overrides; an empty field means "absent"
//! (fall back to [`PluginConfig`] defaults). Empty entries (a trailing
//! `;`, or an entry with no target) are skipped. The request line and body
//! fields (`arg_method`/`arg_path`/`arg_query`/`arg_ver`/`arg_hdrs`/
//! `arg_body`) are shared by every target and captured once.
//!
//! For backward compatibility with a chart that still emits the legacy
//! single-target message (`arg_target`/`arg_scheme`/`arg_timeout_ms`/
//! `arg_retries`), a message without `arg_targets` falls back to
//! extracting exactly one request.
//!
//! `arg_hdrs` is HAProxy's `req.hdrs_bin` encoding: alternating
//! length-prefixed name/value pairs terminated by a zero-length name.
//! `arg_body` is the raw request body; multi-frame SPOE bodies are
//! reassembled by the hub before reaching this plugin.
//!
//! The chart populates the per-target `timeout_ms`/`retries` from the
//! primary backend's `timeout server` and `retries` so a mirror dispatch
//! waits as long as a normal backend request would — a too-tight default
//! silently drops mirrors during in-cluster Service cold-start (DNS not
//! yet propagated, kube-proxy iptables not yet installed) and the failure
//! is invisible because dispatch is fire-and-forget.
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

/// Request-line and body fields shared by every mirror target of a single
/// SPOE message. Captured once, then cloned into each per-target
/// [`MirrorRequest`].
struct SharedRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

fn extract_shared(message: &SpoeMessage, max_body_bytes: usize) -> SharedRequest {
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

    SharedRequest {
        method,
        path,
        query,
        headers,
        body,
    }
}

/// Extract every mirror request carried by an SPOE message.
///
/// When `arg_targets` is present it is parsed as a `;`-delimited list of
/// `scheme|host:port|timeout_ms|retries` entries, each producing one
/// [`MirrorRequest`] sharing the message's request-line/body fields. Empty
/// entries and entries with no target are skipped. A message without
/// `arg_targets` falls back to the legacy single-target contract
/// ([`extract_mirror_request`]), yielding at most one request. Body length
/// is capped at `max_body_bytes`.
pub fn extract_mirror_requests(message: &SpoeMessage, max_body_bytes: usize) -> Vec<MirrorRequest> {
    let Some(list) = message.get_string("arg_targets").filter(|s| !s.is_empty()) else {
        // Legacy single-target message.
        return extract_mirror_request(message, max_body_bytes)
            .into_iter()
            .collect();
    };

    let shared = extract_shared(message, max_body_bytes);
    let mut out = Vec::new();
    for entry in list.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut fields = entry.split('|');
        // Field 0: scheme. Anything but "https" is http (see the legacy
        // path's rationale) — a missing field also defaults to http.
        let scheme = match fields.next() {
            Some("https") => Scheme::Https,
            _ => Scheme::Http,
        };
        // Field 1: host:port. An entry with no (or empty) target is
        // meaningless — skip it rather than dispatch to nowhere.
        let target = match fields.next() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => continue,
        };
        // Fields 2/3: optional per-target overrides. Empty or unparseable
        // → None (fall back to PluginConfig defaults). A malformed number
        // never drops the whole entry — the target still mirrors with
        // default timeout/retries. `timeout_ms=0` is treated as absent
        // (guards the "0ms timeout" footgun); `retries=0` is a legitimate
        // explicit "no retries" override.
        let timeout_ms = fields
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0);
        let retries = fields.next().and_then(|s| s.parse::<u32>().ok());

        out.push(MirrorRequest {
            target,
            scheme,
            method: shared.method.clone(),
            path: shared.path.clone(),
            query: shared.query.clone(),
            headers: shared.headers.clone(),
            body: shared.body.clone(),
            timeout_ms,
            retries,
        });
    }
    out
}

/// Extract a single mirror request from the legacy `arg_target` contract,
/// returning None if the target is missing/empty. Retained for backward
/// compatibility with a chart that predates the `arg_targets` list; new
/// configs go through [`extract_mirror_requests`].
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

    let shared = extract_shared(message, max_body_bytes);

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
        method: shared.method,
        path: shared.path,
        query: shared.query,
        headers: shared.headers,
        body: shared.body,
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

    #[test]
    fn targets_list_fans_out_sharing_request_fields() {
        // Two targets in one message, chart wire format
        // `scheme|host:port|timeout_ms|retries;` per entry (trailing `;`).
        let msg = message(vec![
            (
                "arg_targets",
                SpoeValue::String("http|10.0.0.1:8080|1500|3;https|10.0.0.2:443||;".into()),
            ),
            ("arg_method", SpoeValue::String("POST".into())),
            ("arg_path", SpoeValue::String("/api".into())),
        ]);
        let reqs = extract_mirror_requests(&msg, 1024);
        assert_eq!(reqs.len(), 2);

        assert_eq!(reqs[0].target, "10.0.0.1:8080");
        assert_eq!(reqs[0].scheme, Scheme::Http);
        assert_eq!(reqs[0].timeout_ms, Some(1500));
        assert_eq!(reqs[0].retries, Some(3));

        assert_eq!(reqs[1].target, "10.0.0.2:443");
        assert_eq!(reqs[1].scheme, Scheme::Https);
        // Empty override fields → None (fall back to plugin defaults).
        assert_eq!(reqs[1].timeout_ms, None);
        assert_eq!(reqs[1].retries, None);

        // Request-line fields are shared across every target.
        for r in &reqs {
            assert_eq!(r.method, "POST");
            assert_eq!(r.path, "/api");
        }
    }

    #[test]
    fn targets_list_skips_empty_and_targetless_entries() {
        // Trailing `;` yields an empty entry; a scheme-only entry has no
        // target. Both are skipped rather than dispatched to nowhere.
        let msg = message(vec![(
            "arg_targets",
            SpoeValue::String("http|good:80||;;https||500|1;".into()),
        )]);
        let reqs = extract_mirror_requests(&msg, 1024);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].target, "good:80");
    }

    #[test]
    fn targets_list_zero_timeout_is_absent_retries_zero_is_explicit() {
        let msg = message(vec![(
            "arg_targets",
            SpoeValue::String("http|host:80|0|0;".into()),
        )]);
        let reqs = extract_mirror_requests(&msg, 1024);
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].timeout_ms, None,
            "timeout_ms=0 must be treated as absent (fallback to default)"
        );
        assert_eq!(
            reqs[0].retries,
            Some(0),
            "retries=0 must round-trip as an explicit no-retries override"
        );
    }

    #[test]
    fn absent_targets_falls_back_to_single_target_contract() {
        // A message that predates the list contract (legacy chart) still
        // mirrors via the single arg_target path.
        let msg = message(vec![
            ("arg_target", SpoeValue::String("legacy:80".into())),
            ("arg_scheme", SpoeValue::String("https".into())),
        ]);
        let reqs = extract_mirror_requests(&msg, 1024);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].target, "legacy:80");
        assert_eq!(reqs[0].scheme, Scheme::Https);
    }

    #[test]
    fn empty_targets_list_yields_no_requests() {
        let msg = message(vec![("arg_targets", SpoeValue::String("".into()))]);
        // Empty arg_targets falls through to the legacy path, which finds
        // no arg_target either → zero requests.
        assert!(extract_mirror_requests(&msg, 1024).is_empty());
    }
}
