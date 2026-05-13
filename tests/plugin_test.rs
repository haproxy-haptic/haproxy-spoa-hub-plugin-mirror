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
