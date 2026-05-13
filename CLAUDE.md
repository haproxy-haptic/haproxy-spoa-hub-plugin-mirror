# haproxy-spoa-hub-plugin-mirror Development Guidelines

## Active Technologies

- Rust (edition 2024, minimum 1.86) + `haproxy-spoa-hub-plugin-api` 0.5
  (hand-rolled `#[repr(C)]` vtable), `reqwest` with `rustls-tls`, `tokio`
  multi-thread runtime
- Async dispatch lives on a dedicated tokio runtime stored in `OnceLock`;
  `process()` is synchronous (per the SPOA plugin contract) and
  spawn-and-forgets the HTTP call

## Project Structure

```
src/
  lib.rs           — module declarations (pub mod client/config/request, mod plugin)
  plugin.rs        — define_plugin! invocation, holds PluginState (config + runtime + client)
  config.rs        — PluginConfig (timeout_ms, max_body_bytes, request_concurrency, verify_tls)
  client.rs        — MirrorClient: reqwest dispatcher + Semaphore + drop counter
  request.rs       — extract_mirror_request, parse_hdrs_bin (HAProxy's req.hdrs_bin format)
tests/
  plugin_test.rs   — config parsing + schema validation
  mirror_test.rs   — end-to-end against a real hyper backend
  smoke/run.sh     — CI smoke test
benches/
  mirror_throughput.rs — Criterion microbench on extract_mirror_request
```

## Commands

```bash
cargo build --release            # Build plugin .so
cargo test --all-features        # Unit + integration tests
cargo clippy -- -D warnings      # Lint (CI enforces -D warnings)
cargo fmt --check                # Format check
cargo bench                      # Criterion benchmarks
```

## Code Style

Rust (edition 2024, minimum 1.86): standard conventions; `max_width = 100`
via `rustfmt.toml`. Clippy is run with `-D warnings` in CI — no
`#[allow]` suppressions without a comment explaining why.

## Plugin Contract

This plugin diverges from upstream `haproxytech/spoa-mirror` in one key
way: **the mirror target is a per-request SPOE arg, not a CLI flag**.
This is what makes Gateway-API HTTPRoute `RequestMirror` (with its
per-route `backendRef`) possible.

The full SPOE message arg set is:
- `arg_target` (str): `<host:port>` — required
- `arg_scheme` (str): `http`|`https` — defaults to `http` if missing
- `arg_method` (str): HTTP method — defaults to `GET`
- `arg_path` (str): URI path — defaults to `/`
- `arg_query` (str): URI query (without `?`) — optional
- `arg_ver` (str): HTTP version — informational, not currently used
- `arg_hdrs` (bin): `req.hdrs_bin` format (HAProxy's varint-prefixed name/value pairs)
- `arg_body` (bin): raw request body, capped at `max_body_bytes`

Sampling (`SupportHTTPRouteRequestPercentageMirror`) is done HAProxy-side
via `rand(100) lt <percent>` on the `send-spoe-group` call, not in the
plugin. Keep it that way.
