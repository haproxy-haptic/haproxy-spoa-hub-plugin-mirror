use std::num::NonZeroUsize;
use std::thread::available_parallelism;

use haproxy_spoa_hub_plugin_api::PluginContext;

/// Plugin configuration parsed from the hub's `[plugins.params]` TOML table.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Per-request HTTP timeout in milliseconds, end-to-end (connect + send + receive).
    pub timeout_ms: u64,
    /// Hard cap on the reassembled request body size in bytes. Bodies
    /// exceeding this are truncated; the mirror request is still sent.
    pub max_body_bytes: usize,
    /// Bounded in-flight mirror requests across all targets. Excess
    /// dispatches are dropped (with a metric increment) rather than
    /// queued — keeps the SPOA worker thread responsive under burst.
    /// `None` selects an auto-derived default at runtime
    /// ([`PluginConfig::resolved_request_concurrency`]) that scales
    /// with `resolved_worker_threads()`; operators can pin a specific
    /// count via the TOML knob when the worker-count × 512 heuristic
    /// doesn't match their workload shape.
    pub request_concurrency: Option<usize>,
    /// Whether to validate TLS certificates when the mirror target uses
    /// `arg_scheme=https`. Defaults to `false` so the conformance suite
    /// (which uses self-signed in-cluster certs) works out of the box.
    /// Flip to `true` once `BackendTLSPolicy`-style trust material is
    /// threaded through from the chart.
    pub verify_tls: bool,
    /// Number of tokio worker threads driving async mirror dispatches.
    /// `None` selects an auto-derived default at runtime
    /// ([`PluginConfig::resolved_worker_threads`]). Operators can pin a
    /// specific count via the TOML knob for predictable resource
    /// accounting in cgroup-bounded deployments.
    pub worker_threads: Option<usize>,
    /// Per-host idle-connection cap for the reqwest connection pool.
    /// `0` disables idle reuse entirely; any positive value bounds the
    /// per-target pool. Default 32 prevents an idle-conn explosion when
    /// mirroring to many distinct backends; raise for high-fan-out
    /// workloads with stable target sets where reuse pays off most.
    pub pool_max_idle_per_host: usize,
}

impl Default for PluginConfig {
    fn default() -> Self {
        PluginConfig {
            timeout_ms: 200,
            max_body_bytes: 1024 * 1024,
            request_concurrency: None,
            verify_tls: false,
            worker_threads: None,
            pool_max_idle_per_host: 32,
        }
    }
}

impl PluginConfig {
    /// Resolve `worker_threads` to a concrete count. Returns the
    /// operator's explicit override when set; otherwise queries
    /// [`std::thread::available_parallelism`] (which is cgroup-aware on
    /// Linux ≥ 5.16 / glibc ≥ 2.36, so a CPU-limited Pod sees its
    /// limit rather than the host's CPU count) and floors at 2.
    ///
    /// The floor matters: on a sidecar with `cpu: 100m` (~0.1 CPU),
    /// `available_parallelism()` can return 1, which would leave the
    /// runtime with a single worker shared between the reqwest
    /// connection loop and the per-request dispatch tasks — a recipe
    /// for self-deadlock under load. Two workers is the minimum that
    /// keeps reqwest's connection driver and a dispatch task running
    /// concurrently.
    pub fn resolved_worker_threads(&self) -> usize {
        if let Some(n) = self.worker_threads {
            return n.max(1);
        }
        let detected = available_parallelism().map(NonZeroUsize::get).unwrap_or(2);
        detected.max(2)
    }

    /// Resolve `request_concurrency` to a concrete in-flight cap.
    /// Returns the operator's explicit override when set; otherwise
    /// derives `resolved_worker_threads() × 512`, clamped to
    /// `[1024, 4096]`.
    ///
    /// Per-worker math: each tokio worker can comfortably drive ~512
    /// concurrent reqwest dispatches before scheduling overhead and
    /// per-task memory dominate. The floor of 1024 matches plugin
    /// v0.2.0's static default — a small-CPU sidecar (1–2 workers)
    /// keeps the same headroom it had before auto-sizing. The ceiling
    /// of 4096 keeps the in-flight set bounded on a 16-thread host:
    /// beyond ~4k concurrent mirrors per plugin, the memory footprint
    /// of buffered request bodies and the latency tail of the tokio
    /// scheduler outweigh the benefit of accepting more rather than
    /// dropping into the saturation metric. Workloads that genuinely
    /// need more should pin `request_concurrency` and tune the host's
    /// memory budget alongside it.
    ///
    /// Examples (worker count → resolved concurrency):
    /// ```text
    ///   2 →  1024   (floor; tiny sidecar, unchanged from v0.2.0)
    ///   4 →  2048
    ///   6 →  3072
    ///   8 →  4096   (ceiling)
    ///  16 →  4096   (clamped)
    /// ```
    pub fn resolved_request_concurrency(&self) -> usize {
        const PER_WORKER: usize = 512;
        const MIN: usize = 1024;
        const MAX: usize = 4096;
        if let Some(n) = self.request_concurrency {
            return n.max(1);
        }
        let derived = self.resolved_worker_threads().saturating_mul(PER_WORKER);
        derived.clamp(MIN, MAX)
    }
}

impl PluginConfig {
    pub fn from_context(context: &PluginContext) -> Self {
        let defaults = PluginConfig::default();

        let timeout_ms = context
            .get_config("timeout_ms")
            .and_then(|v| v.as_integer())
            .filter(|n| *n > 0)
            .map(|n| n as u64)
            .unwrap_or(defaults.timeout_ms);

        let max_body_bytes = context
            .get_config("max_body_bytes")
            .and_then(|v| v.as_integer())
            .filter(|n| *n > 0)
            .map(|n| n as usize)
            .unwrap_or(defaults.max_body_bytes);

        let request_concurrency = context
            .get_config("request_concurrency")
            .and_then(|v| v.as_integer())
            .filter(|n| *n > 0)
            .map(|n| n as usize);

        let verify_tls = context
            .get_config("verify_tls")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.verify_tls);

        let worker_threads = context
            .get_config("worker_threads")
            .and_then(|v| v.as_integer())
            .filter(|n| *n > 0)
            .map(|n| n as usize);

        let pool_max_idle_per_host = context
            .get_config("pool_max_idle_per_host")
            .and_then(|v| v.as_integer())
            .filter(|n| *n >= 0)
            .map(|n| n as usize)
            .unwrap_or(defaults.pool_max_idle_per_host);

        PluginConfig {
            timeout_ms,
            max_body_bytes,
            request_concurrency,
            verify_tls,
            worker_threads,
            pool_max_idle_per_host,
        }
    }

    pub fn config_schema() -> &'static str {
        r#"{
            "type": "object",
            "properties": {
                "timeout_ms": { "type": "integer", "minimum": 1, "default": 200 },
                "max_body_bytes": { "type": "integer", "minimum": 1, "default": 1048576 },
                "request_concurrency": { "type": "integer", "minimum": 1 },
                "verify_tls": { "type": "boolean", "default": false },
                "worker_threads": { "type": "integer", "minimum": 1 },
                "pool_max_idle_per_host": { "type": "integer", "minimum": 0, "default": 32 }
            },
            "additionalProperties": false
        }"#
    }
}
