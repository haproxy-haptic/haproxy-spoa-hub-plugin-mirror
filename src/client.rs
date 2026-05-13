//! HTTP dispatcher for mirrored requests.
//!
//! Spawned from the synchronous SPOA `process()` thunk onto the plugin's
//! tokio runtime as a fire-and-forget task. Bounded by a semaphore so
//! a burst of mirror traffic can't exhaust the connection pool — excess
//! requests increment the drop counter rather than queueing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use reqwest::Client;
use tokio::sync::Semaphore;

use crate::config::PluginConfig;
use crate::request::MirrorRequest;

#[derive(Clone)]
pub struct MirrorClient {
    inner: Client,
    permits: Arc<Semaphore>,
    dropped: Arc<AtomicU64>,
}

impl MirrorClient {
    pub fn new(config: &PluginConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let inner = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .danger_accept_invalid_certs(!config.verify_tls)
            // Don't reuse the host's `http_proxy` env — mirror traffic
            // is in-cluster pod→pod by design.
            .no_proxy()
            .build()?;
        Ok(MirrorClient {
            inner,
            permits: Arc::new(Semaphore::new(config.request_concurrency)),
            dropped: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Total number of mirror dispatches that were dropped because the
    /// concurrency semaphore was saturated. Exposed for tests + metrics.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Dispatch one mirror request. Returns immediately when no permit
    /// is available (incrementing the drop counter). Otherwise spawns
    /// the HTTP call and returns after the call completes (or errors).
    pub async fn dispatch(&self, req: MirrorRequest) {
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!(
                    target: "mirror",
                    "dropping mirror request to {} — concurrency saturated",
                    req.target
                );
                return;
            }
        };

        let url = build_url(&req);
        let method = match reqwest::Method::from_bytes(req.method.as_bytes()) {
            Ok(m) => m,
            Err(_) => {
                log::warn!(target: "mirror", "invalid method {:?}", req.method);
                drop(permit);
                return;
            }
        };

        let mut builder = self.inner.request(method, &url);
        for (name, value) in &req.headers {
            if is_hop_by_hop(name) {
                continue;
            }
            builder = builder.header(name, value.clone());
        }
        if !req.body.is_empty() {
            builder = builder.body(Bytes::from(req.body));
        }

        match builder.send().await {
            Ok(resp) => log::debug!(
                target: "mirror",
                "mirror {} → {}: status {}",
                req.target, url, resp.status()
            ),
            Err(err) => log::debug!(
                target: "mirror",
                "mirror {} → {} failed: {}",
                req.target, url, err
            ),
        }
        drop(permit);
    }
}

fn build_url(req: &MirrorRequest) -> String {
    let mut url = String::with_capacity(
        req.scheme.as_str().len()
            + 3
            + req.target.len()
            + req.path.len()
            + req.query.as_ref().map_or(0, |q| q.len() + 1),
    );
    url.push_str(req.scheme.as_str());
    url.push_str("://");
    url.push_str(&req.target);
    if !req.path.starts_with('/') {
        url.push('/');
    }
    url.push_str(&req.path);
    if let Some(q) = &req.query {
        url.push('?');
        url.push_str(q);
    }
    url
}

/// Headers that must not be forwarded across the proxy hop per RFC 7230
/// section 6.1. We strip them rather than relying on reqwest to do so
/// because the mirror's response is never read by HAProxy and there's
/// no value in propagating connection-scoped state.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Scheme;

    fn mk_req(target: &str, path: &str, scheme: Scheme) -> MirrorRequest {
        MirrorRequest {
            target: target.to_string(),
            scheme,
            method: "GET".to_string(),
            path: path.to_string(),
            query: None,
            headers: vec![],
            body: vec![],
        }
    }

    #[test]
    fn builds_url_http() {
        let req = mk_req("svc:80", "/foo", Scheme::Http);
        assert_eq!(build_url(&req), "http://svc:80/foo");
    }

    #[test]
    fn builds_url_https() {
        let req = mk_req("svc:443", "/foo", Scheme::Https);
        assert_eq!(build_url(&req), "https://svc:443/foo");
    }

    #[test]
    fn builds_url_prepends_slash() {
        let req = mk_req("svc:80", "foo", Scheme::Http);
        assert_eq!(build_url(&req), "http://svc:80/foo");
    }

    #[test]
    fn builds_url_with_query() {
        let mut req = mk_req("svc:80", "/foo", Scheme::Http);
        req.query = Some("a=1&b=2".to_string());
        assert_eq!(build_url(&req), "http://svc:80/foo?a=1&b=2");
    }

    #[test]
    fn hop_by_hop_matches_case_insensitive() {
        assert!(is_hop_by_hop("connection"));
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("CONNECTION"));
        assert!(is_hop_by_hop("Transfer-Encoding"));
        assert!(is_hop_by_hop("Host"));
        assert!(!is_hop_by_hop("x-foo"));
        assert!(!is_hop_by_hop("user-agent"));
    }
}
