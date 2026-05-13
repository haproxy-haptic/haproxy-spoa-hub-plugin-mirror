# HAProxy SPOA Hub — Mirror Plugin

[![Build Status](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub-plugin-mirror/badges/master/pipeline.svg)](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub-plugin-mirror/-/pipelines)

A [haproxy-spoa-hub](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub) plugin that mirrors HTTP requests to a configurable upstream target while the original request continues unaffected. Built for the Gateway API `HTTPRouteFilter` of type `RequestMirror`, but useful anywhere fire-and-forget request duplication is needed (shadow traffic, integration testing against a canary, audit pipelines).

## Overview

Request mirroring sends a copy of an incoming HTTP request to a second backend without affecting the response the original client receives. HAProxy emits an SPOE NOTIFY frame containing the request method, path, headers and body; this plugin reassembles the request and dispatches it asynchronously over HTTP(S) to the target named in the SPOE arg. The plugin acks back to HAProxy immediately — the upstream call lives on a dedicated tokio runtime — so the SPOA worker thread is never blocked.

Unlike [upstream `haproxytech/spoa-mirror`](https://github.com/haproxytech/spoa-mirror), which pins the mirror destination at agent startup, this plugin accepts the target as a per-request SPOE argument. One plugin instance can therefore serve every HTTPRoute in a cluster — each route picks its own mirror backend by emitting a different `arg_target`. Multi-mirror routes work by emitting multiple `send-spoe-group` calls, each with its own target.

## Quick Start

### Prerequisites

- [haproxy-spoa-hub](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub) v0.5.0 or newer running with this plugin baked in
- HAProxy >= 2.8 (3.1+ for native `mode spop` backend)
- Rust toolchain (>= 1.86) for building from source

### Build

```bash
cargo build --release
```

The output is `target/release/libmirror_plugin.so`. Copy it to the hub's plugin directory.

Pre-built binaries for all supported architectures are available on the [releases page](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub-plugin-mirror/-/releases).

### Supported Architectures

| Architecture | Rust Target                       | Compatible With                          |
|--------------|-----------------------------------|------------------------------------------|
| `amd64`      | `x86_64-unknown-linux-gnu`        | haproxytech/haproxy-debian (amd64)       |
| `arm64`      | `aarch64-unknown-linux-gnu`       | haproxytech/haproxy-debian (arm64)       |
| `armv7`      | `armv7-unknown-linux-gnueabihf`   | haproxytech/haproxy-debian (arm/v7)      |

All binaries are built on Debian bookworm and require **glibc >= 2.36** at runtime. The glibc version is included in the filename (e.g., `libmirror_plugin-amd64-glibc2.36.so`).

### Configure the Hub

```toml
[[plugins]]
name = "mirror"
library = "libmirror_plugin.so"
messages = ["mirror"]
```

By default the plugin mirrors with a 200 ms per-request timeout, 1 MiB body cap, and TLS verification off. To customise:

```toml
[plugins.params]
timeout_ms = 200             # per-request HTTP timeout in milliseconds
max_body_bytes = 1048576     # cap on body bytes (truncates if exceeded)
request_concurrency = 1024   # max in-flight mirrors; excess dropped
verify_tls = false           # validate TLS certs for https targets
```

### Configure HAProxy

#### SPOE agent definition (`spoe-mirror.conf`)

```
[mirror]
spoe-agent mirror-agent
    messages mirror
    option var-prefix spoe
    option continue-on-error
    timeout hello      2s
    timeout idle       5m
    timeout processing 200ms
    use-backend spoa-hub-backend

spoe-message mirror
    args arg_target=str(backend-2.example.svc.cluster.local:80) \
         arg_scheme=str(http) \
         arg_method=method \
         arg_path=path \
         arg_query=query \
         arg_ver=req.ver \
         arg_hdrs=req.hdrs_bin \
         arg_body=req.body
    event on-frontend-http-request
```

In a Gateway-API controller (like [haptic](https://gitlab.com/haproxy-haptic/haptic)), the `args` line is rendered per HTTPRoute, with `arg_target` bound to the resolved `backendRef` and other args left as plain fetch references.

#### HAProxy frontend (`haproxy.cfg`)

```haproxy
frontend http-in
    bind *:80
    # Required for PUT/POST body mirroring — without this, only the
    # request line and headers reach the plugin.
    option http-buffer-request

    filter spoe engine mirror config /etc/haproxy/spoe-mirror.conf

    # Fire the mirror; sampled at 20% of traffic via rand().
    http-request send-spoe-group mirror mirror if { rand(100) lt 20 }

    default_backend primary

backend primary
    server primary primary.example.svc.cluster.local:80

backend spoa-hub-backend
    mode spop
    server hub /var/run/spoe/hub.sock
```

The `rand(100) lt 20` ACL gates `send-spoe-group` at 20% sampling — no native plugin support for sampling is needed because HAProxy already does this cleanly. For full traffic mirroring, drop the `if` clause.

### Verify

Watch the mirror backend's access log: requests to `primary` should show up there too (with `User-Agent`, body, and method preserved), at the configured sample rate.

## Configuration Reference

| Parameter            | Type    | Default     | Description                                                                 |
|----------------------|---------|-------------|-----------------------------------------------------------------------------|
| `timeout_ms`         | integer | `200`       | Per-request HTTP timeout (connect + send + receive) in milliseconds.        |
| `max_body_bytes`     | integer | `1048576`   | Hard cap on reassembled body size. Larger bodies are truncated.             |
| `request_concurrency`| integer | `1024`      | Max in-flight mirrors across all targets. Excess dispatches are dropped.    |
| `verify_tls`         | boolean | `false`     | Validate TLS certs for `https` targets. Off by default for self-signed.     |

## How It Works

```
   ┌────────────┐   1. NOTIFY (SPOE)        ┌──────────────┐
   │            ├──────────────────────────▶│              │
   │  HAProxy   │   2. ACK (immediately)    │ spoa-hub +   │
   │            │◀──────────────────────────│ mirror plug. │
   └──────┬─────┘                           └──────┬───────┘
          │                                        │ 3. async dispatch
          │ 4. response                            │    on tokio runtime
          ▼                                        ▼
   ┌────────────┐                           ┌──────────────┐
   │  primary   │                           │   mirror     │
   │  backend   │                           │   backend    │
   └────────────┘                           └──────────────┘
```

1. HAProxy emits an SPOE NOTIFY frame with the request method, path, headers (as `req.hdrs_bin`), and body (as `req.body`).
2. The hub passes the frame to this plugin, which extracts the mirror target from `arg_target`, builds a `reqwest::Request`, and spawns it on the plugin's tokio runtime. The plugin acks the SPOE frame immediately so HAProxy is never waiting on the mirror call.
3. The mirror request fires asynchronously. The response is discarded.
4. HAProxy continues processing the original request — its response is what reaches the client.

If the concurrency semaphore is saturated, the dispatch returns immediately and the drop counter increments. This bounds memory and connection-pool usage at the cost of skipping some mirror traffic under burst.

## Performance Considerations

- **`option http-buffer-request`** is required for PUT/POST body mirroring. Without it, HAProxy doesn't buffer the request body before the SPOE notification, and `arg_body` arrives empty. The trade-off is increased per-request latency under buffering (the entire body is read before the SPOE frame fires).
- **Body cap (`max_body_bytes`)** truncates rather than skipping. Set lower if you're mirroring high-volume APIs where partial body coverage is acceptable.
- **Concurrency cap (`request_concurrency`)** prevents the plugin from holding open thousands of in-flight reqwest connections under burst. The default 1024 fits typical clusters; raise for high-fan-out workloads.
- **Sampling (`rand(100) lt N`)** lives in HAProxy, not the plugin. Keep it there — HAProxy's `rand()` is uniform, well-tested, and avoids per-request plugin overhead for skipped traffic.

## Development

### Commands

```bash
cargo build --release            # Build plugin .so
cargo test                       # Unit + integration tests
cargo bench                      # Criterion microbenchmark on dispatch hot path
cargo clippy -- -D warnings      # Lint
cargo fmt --check                # Format check
```

### Project Structure

```
src/
  lib.rs              Module declarations
  plugin.rs           Plugin entry (define_plugin! macro, tokio runtime)
  config.rs           PluginConfig (timeout, body cap, concurrency, TLS verify)
  client.rs           MirrorClient: reqwest dispatcher, semaphore, drop counter
  request.rs          SPOE → MirrorRequest reassembly, req.hdrs_bin parser
tests/
  plugin_test.rs      Config parsing + schema validation
  mirror_test.rs      End-to-end: dispatch through a real hyper backend
                      (verifies method, path, headers, body, hop-by-hop
                       stripping, concurrency dropping)
  smoke/              CI smoke test: load .so into the hub, verify init
benches/
  mirror_throughput.rs  Criterion microbenchmark on extract_mirror_request
```

## License

Licensed under the [Apache License 2.0](LICENSE).
