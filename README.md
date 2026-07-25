# HAProxy SPOA Hub — Mirror Plugin

[![Build Status](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub-plugin-mirror/badges/master/pipeline.svg)](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub-plugin-mirror/-/pipelines)

A [haproxy-spoa-hub](https://gitlab.com/haproxy-haptic/haproxy-spoa-hub) plugin that mirrors HTTP requests to a configurable upstream target while the original request continues unaffected. Built for the Gateway API `HTTPRouteFilter` of type `RequestMirror`, but useful anywhere fire-and-forget request duplication is needed (shadow traffic, integration testing against a canary, audit pipelines).

## Overview

Request mirroring sends a copy of an incoming HTTP request to a second backend without affecting the response the original client receives. HAProxy emits an SPOE NOTIFY frame containing the request method, path, headers and body plus a per-request list of mirror targets; this plugin reassembles the request and dispatches it asynchronously over HTTP(S) to every target in the list. The plugin acks back to HAProxy immediately — the upstream calls live on a dedicated tokio runtime — so the SPOA worker thread is never blocked.

Unlike [upstream `haproxytech/spoa-mirror`](https://github.com/haproxytech/spoa-mirror), which pins the mirror destination at agent startup, this plugin accepts the targets as a per-request SPOE argument. One plugin instance can therefore serve every HTTPRoute in a cluster from a single static SPOE message + group: the caller appends `scheme|host:port|timeout_ms|retries` entries to one request-scoped variable and passes the joined list as `arg_targets`. A request that mirrors to several destinations at once (a route rule with multiple `RequestMirror` filters, or several `mirror-target` Ingresses on one host) rides one NOTIFY, and the plugin fans out to each entry — so adding a mirror target never changes the SPOE configuration.

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
verify_tls = false           # validate TLS certs for https targets
# request_concurrency omitted: auto-derived as worker_threads × 512,
# clamped to [1024, 4096]. Set explicitly to bypass the clamp:
# request_concurrency = 8192
# worker_threads omitted: auto-derived from available_parallelism()
# floored at 2. Set explicitly for predictable resource accounting:
# worker_threads = 4
pool_max_idle_per_host = 32  # per-target idle conn pool cap; 0 disables reuse
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
    args arg_targets=var(txn.gw_mirror_targets) \
         arg_method=method \
         arg_path=path \
         arg_query=query \
         arg_ver=req.ver \
         arg_hdrs=req.hdrs_bin \
         arg_body=req.body
    event on-frontend-http-request
```

`arg_targets` is a `;`-delimited list, each entry `scheme|host:port|timeout_ms|retries` (empty timeout/retries → plugin defaults). In a Gateway-API controller (like [haptic](https://gitlab.com/haproxy-haptic/haptic)), each mirror source appends its own entry to the request-scoped variable — `http-request set-var(txn.gw_mirror_targets) str(<entry>;),concat(,txn.gw_mirror_targets,) if <cond>` — and the `spoe-message` block itself is static.

#### HAProxy frontend (`haproxy.cfg`)

```haproxy
frontend http-in
    bind *:80
    # Required for PUT/POST body mirroring — without this, only the
    # request line and headers reach the plugin.
    option http-buffer-request

    filter spoe engine mirror config /etc/haproxy/spoe-mirror.conf

    # Append a target (sampled at 20% via rand), then fire the group once.
    http-request set-var(txn.gw_mirror_targets) str(http|backend-2.example.svc.cluster.local:80||;),concat(,txn.gw_mirror_targets,) if { rand(100) lt 20 }
    http-request send-spoe-group mirror mirror if { var(txn.gw_mirror_targets) -m found }

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

| Parameter                | Type    | Default                                 | Description                                                                 |
|--------------------------|---------|-----------------------------------------|-----------------------------------------------------------------------------|
| `timeout_ms`             | integer | `200`                                   | Per-request HTTP timeout (connect + send + receive) in milliseconds.        |
| `max_body_bytes`         | integer | `1048576`                               | Hard cap on reassembled body size. Larger bodies are truncated.             |
| `request_concurrency`    | integer | _auto_ (`worker_threads × 512`, clamped to `[1024, 4096]`) | Max in-flight mirrors across all targets. Excess dispatches are dropped. Auto-scales with the worker count so 8 workers don't share a 2-worker headroom; clamped on both ends because (a) 1024 matches v0.2.0's static default exactly for tiny sidecars and (b) beyond ~4k concurrent mirrors per plugin the body-buffer memory + scheduler tail outweigh accepting more rather than dropping. Explicit overrides bypass the clamp. |
| `verify_tls`             | boolean | `false`                                 | Validate TLS certs for `https` targets. Off by default for self-signed.     |
| `worker_threads`         | integer | _auto_ (`available_parallelism()`, ≥ 2) | Tokio worker thread count. Auto-derived per cgroup CPU limit when omitted; floored at 2 to prevent single-worker self-deadlock between reqwest's connection driver and dispatch tasks. |
| `pool_max_idle_per_host` | integer | `32`                                    | Per-target idle-connection pool cap. `0` disables idle reuse. Raise for high-fan-out workloads with a stable target set; lower for churning target sets where stale connections would mask backend changes. |

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

1. HAProxy emits an SPOE NOTIFY frame with the request method, path, headers (as `req.hdrs_bin`), body (as `req.body`), and the target list (`arg_targets`).
2. The hub passes the frame to this plugin, which parses the target list, builds a `reqwest::Request` per target, and spawns each on the plugin's tokio runtime. The plugin acks the SPOE frame immediately so HAProxy is never waiting on the mirror calls.
3. The mirror requests fire asynchronously. The responses are discarded.
4. HAProxy continues processing the original request — its response is what reaches the client.

If the concurrency semaphore is saturated, the dispatch returns immediately and the drop counter increments. This bounds memory and connection-pool usage at the cost of skipping some mirror traffic under burst.

## Performance Considerations

- **`option http-buffer-request`** is required for PUT/POST body mirroring. Without it, HAProxy doesn't buffer the request body before the SPOE notification, and `arg_body` arrives empty. The trade-off is increased per-request latency under buffering (the entire body is read before the SPOE frame fires).
- **Body cap (`max_body_bytes`)** truncates rather than skipping. Set lower if you're mirroring high-volume APIs where partial body coverage is acceptable.
- **Concurrency cap (`request_concurrency`)** prevents the plugin from holding open thousands of in-flight reqwest connections under burst. The auto-derived default scales with the tokio worker count (`worker_threads × 512`, clamped to `[1024, 4096]`); pin explicitly for workloads that need more than 4k concurrent mirrors per plugin instance, accepting the proportional memory cost from buffered request bodies.
- **Sampling (`rand(100) lt N`)** lives in HAProxy, not the plugin. Keep it there — HAProxy's `rand()` is uniform, well-tested, and avoids per-request plugin overhead for skipped traffic.

## Observability

The plugin emits Prometheus metrics through the hub's `/metrics` endpoint (requires `haproxy-spoa-hub` v0.6.0+ for the v2 vtable's metric recorder; older hubs load the plugin fine but emissions are silent no-ops). All names are prefixed with `plugin_mirror_` by the hub.

| metric                          | kind      | labels                                |
|---------------------------------|-----------|---------------------------------------|
| `dispatches_total`              | counter   | `target`, `scheme`                    |
| `dispatch_dropped_total`        | counter   | `target`, `reason`                    |
| `dispatch_outcomes_total`       | counter   | `target`, `outcome`, `status_class`   |
| `dispatch_duration_seconds`     | histogram | `target`, `scheme`                    |
| `in_flight`                     | gauge     | _(no labels)_                         |

- `reason` ∈ `{saturation, invalid_method}` — `saturation` means `request_concurrency` was hit; `invalid_method` means the SPOE arg carried a method the HTTP layer refused.
- `outcome` ∈ `{ok, error}` — `ok` means the request reached the mirror backend and returned a response (any status); `error` means the underlying reqwest call failed (timeout, connection refused, TLS handshake, etc.).
- `status_class` is the response's HTTP status bucketed to its hundreds digit (`2xx`, `4xx`, …), `none` when `outcome=error`. Bounded cardinality keeps the labelset small for fan-out deployments.

Useful alerts and questions to answer from this set:

- `rate(plugin_mirror_dispatch_dropped_total{reason="saturation"}[5m]) > 0` — `request_concurrency` is too low for the workload.
- `histogram_quantile(0.99, rate(plugin_mirror_dispatch_duration_seconds_bucket[5m])) > 0.2` — tail latency creeping above `timeout_ms`; tune up or split out the slow target.
- `sum by (target) (rate(plugin_mirror_dispatch_outcomes_total{outcome="error"}[5m])) > 0` — a backend is failing; check from there.

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
