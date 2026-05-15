//! HTTP dispatcher for mirrored requests.
//!
//! Spawned from the synchronous SPOA `process()` thunk onto the plugin's
//! tokio runtime as a fire-and-forget task. Bounded by a semaphore so
//! a burst of mirror traffic can't exhaust the connection pool — excess
//! requests increment the drop counter rather than queueing.
//!
//! ## Metrics
//!
//! Every dispatch path emits through [`crate::plugin::METRICS`] (a
//! [`MetricRecorder`] installed by the hub at plugin load). All metric
//! names appear in Prometheus under the `plugin_mirror_` prefix the hub
//! adds. Label cardinality scales with the number of distinct mirror
//! targets — uncapped by chart-side convention, see the haptic
//! configuration docs.
//!
//! | metric                           | kind      | labels                       |
//! |----------------------------------|-----------|------------------------------|
//! | `dispatches_total`               | counter   | `target`, `scheme`           |
//! | `dispatch_dropped_total`         | counter   | `target`, `reason`           |
//! | `dispatch_outcomes_total`        | counter   | `target`, `outcome`, `status_class` |
//! | `dispatch_duration_seconds`      | histogram | `target`, `scheme`           |
//! | `in_flight`                      | gauge     | _(no labels)_                |
//!
//! `reason` ∈ `{saturation, invalid_method}`; `outcome` ∈ `{ok, error}`;
//! `status_class` ∈ `{2xx, 3xx, 4xx, 5xx, none}` (the `none` value is
//! used when the dispatch errored before getting a response).
//!
//! [`MetricRecorder`]: haproxy_spoa_hub_plugin_api::MetricRecorder

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::Client;
use tokio::sync::Semaphore;

use crate::config::PluginConfig;
use crate::plugin::METRICS;
use crate::request::MirrorRequest;

#[derive(Clone)]
pub struct MirrorClient {
    inner: Client,
    permits: Arc<Semaphore>,
    dropped: Arc<AtomicU64>,
    /// Current in-flight dispatches (after permit acquire, before
    /// permit drop). Bumped on every successful permit acquire and
    /// decremented at the end of `dispatch`. Surfaced through the
    /// `in_flight` gauge each time it changes, so operators can see
    /// burst pressure without scraping the underlying tokio runtime.
    in_flight: Arc<AtomicI64>,
}

impl MirrorClient {
    pub fn new(config: &PluginConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let inner = Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .danger_accept_invalid_certs(!config.verify_tls)
            // Don't reuse the host's `http_proxy` env — mirror traffic
            // is in-cluster pod→pod by design.
            .no_proxy()
            // Bound the per-host idle pool so a long-running plugin
            // sweeping many distinct mirror targets doesn't accumulate
            // unbounded keep-alive sockets. reqwest's default is
            // usize::MAX, which is fine for a single hot target but
            // explodes for fan-out mirroring.
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .build()?;
        Ok(MirrorClient {
            inner,
            permits: Arc::new(Semaphore::new(config.resolved_request_concurrency())),
            dropped: Arc::new(AtomicU64::new(0)),
            in_flight: Arc::new(AtomicI64::new(0)),
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
        let target_label: &str = &req.target;
        let scheme_label: &str = req.scheme.as_str();

        // Every dispatch attempt counts, even ones we'll drop. Lets
        // operators reason about the saturation rate (dropped / total)
        // without joining counters across labels.
        METRICS.counter(
            "dispatches_total",
            &[("target", target_label), ("scheme", scheme_label)],
            1,
        );

        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                METRICS.counter(
                    "dispatch_dropped_total",
                    &[("target", target_label), ("reason", "saturation")],
                    1,
                );
                log::debug!(
                    target: "mirror",
                    "dropping mirror request to {} — concurrency saturated",
                    req.target
                );
                return;
            }
        };

        // Permit acquired — bump the in-flight gauge.
        #[allow(clippy::cast_precision_loss)]
        let in_flight = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        METRICS.gauge("in_flight", &[], in_flight as f64);

        let url = build_url(&req);
        let method = match reqwest::Method::from_bytes(req.method.as_bytes()) {
            Ok(m) => m,
            Err(_) => {
                log::warn!(target: "mirror", "invalid method {:?}", req.method);
                METRICS.counter(
                    "dispatch_dropped_total",
                    &[("target", target_label), ("reason", "invalid_method")],
                    1,
                );
                self.release_permit(permit);
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

        // Measure from after-permit-acquire to after-send-complete.
        // This is the duration of the actual HTTP roundtrip plus any
        // body-streaming overhead — what an operator tuning
        // timeout_ms / pool_max_idle_per_host actually cares about.
        let started = Instant::now();
        let outcome_label: (&str, &str);
        let status_class_label: (&str, &str);
        match builder.send().await {
            Ok(resp) => {
                log::debug!(
                    target: "mirror",
                    "mirror {} → {}: status {}",
                    req.target, url, resp.status()
                );
                outcome_label = ("outcome", "ok");
                status_class_label = ("status_class", status_class(resp.status().as_u16()));
            }
            Err(err) => {
                log::debug!(
                    target: "mirror",
                    "mirror {} → {} failed: {}",
                    req.target, url, err
                );
                outcome_label = ("outcome", "error");
                status_class_label = ("status_class", "none");
            }
        }
        let elapsed = started.elapsed();

        METRICS.counter(
            "dispatch_outcomes_total",
            &[("target", target_label), outcome_label, status_class_label],
            1,
        );
        METRICS.histogram(
            "dispatch_duration_seconds",
            &[("target", target_label), ("scheme", scheme_label)],
            elapsed.as_secs_f64(),
        );

        self.release_permit(permit);
    }

    /// Drop a permit and decrement the in-flight gauge. Centralises the
    /// gauge bookkeeping so the dispatch paths can't forget to mirror
    /// the increment they did on permit-acquire.
    fn release_permit(&self, permit: tokio::sync::OwnedSemaphorePermit) {
        drop(permit);
        #[allow(clippy::cast_precision_loss)]
        let in_flight = self.in_flight.fetch_sub(1, Ordering::Relaxed) - 1;
        METRICS.gauge("in_flight", &[], in_flight as f64);
    }
}

/// Map an HTTP status code to its standard class string. Bounded
/// cardinality (5 values) — keeps `dispatch_outcomes_total` labelset
/// small even with hundreds of distinct targets.
fn status_class(code: u16) -> &'static str {
    match code {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
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

    /// Pins the `status_class` mapping that lands as a `status_class`
    /// label on `dispatch_outcomes_total`. Bounded cardinality matters
    /// here — if this slipped to per-status-code, the label set would
    /// blow up for a multi-target deployment.
    #[test]
    fn status_class_buckets_match_prometheus_convention() {
        assert_eq!(status_class(100), "1xx");
        assert_eq!(status_class(199), "1xx");
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(204), "2xx");
        assert_eq!(status_class(299), "2xx");
        assert_eq!(status_class(301), "3xx");
        assert_eq!(status_class(399), "3xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(418), "4xx");
        assert_eq!(status_class(499), "4xx");
        assert_eq!(status_class(500), "5xx");
        assert_eq!(status_class(599), "5xx");
        // Out-of-band values (e.g. extension codes) land in `other`
        // rather than panicking; keeps the dispatcher resilient to
        // any oddly-coded backend.
        assert_eq!(status_class(0), "other");
        assert_eq!(status_class(99), "other");
        assert_eq!(status_class(600), "other");
        assert_eq!(status_class(999), "other");
    }
}
