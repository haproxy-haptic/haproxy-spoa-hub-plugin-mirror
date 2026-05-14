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
    assert_eq!(config.timeout_ms, 200);
    assert_eq!(config.max_body_bytes, 1024 * 1024);
    assert_eq!(config.request_concurrency, 1024);
    assert!(!config.verify_tls);
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
    let context = make_context(&[("request_concurrency", ConfigValue::Integer(4096))]);
    let config = PluginConfig::from_context(&context);
    assert_eq!(config.request_concurrency, 4096);
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
    assert_eq!(config.request_concurrency, defaults.request_concurrency);
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
