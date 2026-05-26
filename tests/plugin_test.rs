use haproxy_spoa_hub_plugin_api::{ConfigValue, PluginContext, RHashMap, RString};

use mirror_plugin::config::PluginConfig;

fn make_context(params: &[(&str, ConfigValue)]) -> PluginContext {
    let mut config = RHashMap::new();
    for (key, value) in params {
        config.insert(RString::from(*key), value.clone());
    }
    PluginContext {
        name: RString::from("mirror"),
        config,
    }
}

#[test]
fn default_config() {
    let context = make_context(&[]);
    let config = PluginConfig::from_context(&context);
    // Default is intentionally generous (60s). Tight defaults silently
    // drop mirror dispatches against in-cluster Services that are still
    // cold-starting (DNS / kube-proxy propagation); per-dispatch
    // overrides via `arg_timeout_ms` (chart-driven from the primary
    // backend's `timeout server`) tighten this when needed.
    assert_eq!(config.timeout_ms, 60_000);
    assert_eq!(config.retries, 0);
    assert_eq!(config.max_body_bytes, 1024 * 1024);
    assert!(config.request_concurrency.is_none());
    assert!(!config.verify_tls);
}

#[test]
fn custom_retries() {
    // Operators can set a plugin-wide retry budget that applies when
    // SPOE doesn't pass arg_retries (i.e., when the chart doesn't
    // emit per-dispatch overrides — e.g., for an operator-emitted
    // SPOE message that doesn't come from the chart's mirror filter).
    let context = make_context(&[("retries", ConfigValue::Integer(3))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.retries, 3);
}

#[test]
fn custom_timeout() {
    let context = make_context(&[("timeout_ms", ConfigValue::Integer(500))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.timeout_ms, 500);
}

#[test]
fn custom_body_cap() {
    let context = make_context(&[("max_body_bytes", ConfigValue::Integer(2048))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.max_body_bytes, 2048);
}

#[test]
fn custom_concurrency() {
    let context = make_context(&[("request_concurrency", ConfigValue::Integer(8192))]);
    let config = PluginConfig::from_context(&context);
    // Explicit override bypasses the auto-derive ceiling — operators
    // tuning for a specific workload shape get the value they ask for.
    assert_eq!(config.request_concurrency, Some(8192));
    assert_eq!(config.resolved_request_concurrency(), 8192);
}

#[test]
fn request_concurrency_default_is_auto_derived() {
    let context = make_context(&[]);
    let config = PluginConfig::from_context(&context);
    assert!(config.request_concurrency.is_none());
    // Auto-derive always falls in [1024, 4096] regardless of host CPU
    // count: floor matches v0.2.0's static default; ceiling keeps the
    // in-flight buffer-memory budget bounded.
    let resolved = config.resolved_request_concurrency();
    assert!(
        (1024..=4096).contains(&resolved),
        "resolved {} outside clamp [1024, 4096]",
        resolved
    );
}

#[test]
fn request_concurrency_scales_with_workers() {
    // Pinning 4 workers should pull the auto-derived concurrency to 4 *
    // 512 = 2048 — inside the [1024, 4096] band.
    let context = make_context(&[("worker_threads", ConfigValue::Integer(4))]);
    let config = PluginConfig::from_context(&context);
    assert!(config.request_concurrency.is_none());
    assert_eq!(config.resolved_request_concurrency(), 2048);
}

#[test]
fn request_concurrency_floor_holds_for_tiny_workers() {
    // worker_threads=1 keeps a 1-CPU sidecar running with the same
    // 1024-permit headroom v0.2.0 hardcoded.
    let context = make_context(&[("worker_threads", ConfigValue::Integer(1))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.resolved_request_concurrency(), 1024);
}

#[test]
fn request_concurrency_ceiling_holds_for_giant_workers() {
    // 32 workers × 512 = 16384, clamped to the 4096 ceiling. Operators
    // who genuinely need more must pin the value explicitly.
    let context = make_context(&[("worker_threads", ConfigValue::Integer(32))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.resolved_request_concurrency(), 4096);
}

#[test]
fn verify_tls_toggle() {
    let context = make_context(&[("verify_tls", ConfigValue::Bool(true))]);
    let config = PluginConfig::from_context(&context);
    assert!(config.verify_tls);
}

#[test]
fn worker_threads_default_is_auto_detect() {
    let context = make_context(&[]);
    let config = PluginConfig::from_context(&context);
    assert!(config.worker_threads.is_none());
    // resolved_worker_threads must always return ≥ 2 (the floor) — even
    // on a single-CPU sidecar.
    assert!(config.resolved_worker_threads() >= 2);
}

#[test]
fn worker_threads_override_respected() {
    let context = make_context(&[("worker_threads", ConfigValue::Integer(8))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.worker_threads, Some(8));
    assert_eq!(config.resolved_worker_threads(), 8);
}

#[test]
fn worker_threads_explicit_one_kept() {
    // Operators may deliberately set worker_threads=1 even though it's
    // below our default floor — respect the override.
    let context = make_context(&[("worker_threads", ConfigValue::Integer(1))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.resolved_worker_threads(), 1);
}

#[test]
fn worker_threads_zero_or_negative_falls_back() {
    let context = make_context(&[("worker_threads", ConfigValue::Integer(-3))]);
    let config = PluginConfig::from_context(&context);
    assert!(config.worker_threads.is_none());
    // Falls back to the auto-derived floor.
    assert!(config.resolved_worker_threads() >= 2);
}

#[test]
fn pool_max_idle_per_host_default() {
    let context = make_context(&[]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.pool_max_idle_per_host, 32);
}

#[test]
fn pool_max_idle_per_host_zero_disables_pool() {
    // 0 is a legal value — disables idle keep-alive entirely. Useful
    // for setups where pool reuse would mask staleness in a churning
    // target set.
    let context = make_context(&[("pool_max_idle_per_host", ConfigValue::Integer(0))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.pool_max_idle_per_host, 0);
}

#[test]
fn pool_max_idle_per_host_custom() {
    let context = make_context(&[("pool_max_idle_per_host", ConfigValue::Integer(128))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.pool_max_idle_per_host, 128);
}

#[test]
fn zero_and_negative_integers_fall_back_to_default() {
    // A zero/negative timeout is nonsense and would block forever or
    // overflow on cast. We fall back to the default rather than panic
    // or produce a degenerate value.
    let context = make_context(&[
        ("timeout_ms", ConfigValue::Integer(0)),
        ("max_body_bytes", ConfigValue::Integer(-1)),
        ("request_concurrency", ConfigValue::Integer(0)),
    ]);
    let config = PluginConfig::from_context(&context);
    let defaults = PluginConfig::default();
    assert_eq!(config.timeout_ms, defaults.timeout_ms);
    assert_eq!(config.max_body_bytes, defaults.max_body_bytes);
    // request_concurrency=0 is also nonsense; falls back to None
    // (auto-derive) rather than producing a zero-permit semaphore.
    assert_eq!(config.request_concurrency, defaults.request_concurrency);
    assert!(config.request_concurrency.is_none());
}

#[test]
fn config_schema_is_valid_json() {
    let schema = PluginConfig::config_schema();
    let parsed: serde_json::Value =
        serde_json::from_str(schema).expect("schema must be valid JSON");
    assert_eq!(parsed["type"], "object");
    assert_eq!(parsed["properties"]["timeout_ms"]["type"], "integer");
    assert_eq!(parsed["properties"]["max_body_bytes"]["type"], "integer");
    assert_eq!(
        parsed["properties"]["request_concurrency"]["type"],
        "integer"
    );
    assert_eq!(parsed["properties"]["verify_tls"]["type"], "boolean");
    assert_eq!(parsed["additionalProperties"], false);
}
