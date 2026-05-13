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
    pub request_concurrency: usize,
    /// Whether to validate TLS certificates when the mirror target uses
    /// `arg_scheme=https`. Defaults to `false` so the conformance suite
    /// (which uses self-signed in-cluster certs) works out of the box.
    /// Flip to `true` once `BackendTLSPolicy`-style trust material is
    /// threaded through from the chart.
    pub verify_tls: bool,
}

impl Default for PluginConfig {
    fn default() -> Self {
        PluginConfig {
            timeout_ms: 200,
            max_body_bytes: 1024 * 1024,
            request_concurrency: 1024,
            verify_tls: false,
        }
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
            .map(|n| n as usize)
            .unwrap_or(defaults.request_concurrency);

        let verify_tls = context
            .get_config("verify_tls")
            .and_then(|v| v.as_bool())
            .unwrap_or(defaults.verify_tls);

        PluginConfig {
            timeout_ms,
            max_body_bytes,
            request_concurrency,
            verify_tls,
        }
    }

    pub fn config_schema() -> &'static str {
        r#"{
            "type": "object",
            "properties": {
                "timeout_ms": { "type": "integer", "minimum": 1, "default": 200 },
                "max_body_bytes": { "type": "integer", "minimum": 1, "default": 1048576 },
                "request_concurrency": { "type": "integer", "minimum": 1, "default": 1024 },
                "verify_tls": { "type": "boolean", "default": false }
            },
            "additionalProperties": false
        }"#
    }
}
