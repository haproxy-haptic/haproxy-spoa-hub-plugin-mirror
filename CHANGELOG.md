# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-05-15

### Changed

- `request_concurrency` is now auto-derived by default. Previously the
  in-flight cap was a static 1024 regardless of how many tokio workers
  the plugin had — a sidecar with 8 workers got the same headroom as a
  sidecar with 2, and the workers above 2 sat idle whenever the cap
  saturated. The new default is `resolved_worker_threads() × 512`,
  clamped to `[1024, 4096]`. The floor of 1024 matches v0.2.0's
  hardcoded default exactly, so a tiny-CPU sidecar (1–2 workers) keeps
  the same permit count; the ceiling of 4096 keeps the in-flight
  buffer memory bounded on big hosts (beyond ~4k concurrent mirrors
  per plugin, body buffers + scheduler tail dominate the dispatch
  benefit — the saturation metric is preferable to unbounded
  allocations). Operators with workloads that genuinely need more can
  still pin `request_concurrency` to a specific value in
  `[plugins.params]`; explicit overrides bypass the clamp.

### Plugin contract update

`request_concurrency` retains its TOML shape (positive integer); only
the default behavior changed. Schema and SPOE message contract are
unchanged. No ABI changes.

## [0.2.0] - 2026-05-14

### Added

- `worker_threads` config option. The plugin's tokio runtime previously
  hardcoded 2 worker threads; on hosts with real cores to spare this
  left the dispatch loop under-provisioned for high-fan-out workloads.
  The new default is auto-derived from
  `std::thread::available_parallelism()` (cgroup-aware on Linux ≥ 5.16
  / glibc ≥ 2.36, so CPU-limited Pods see their limit rather than the
  host's count) with a floor of 2. Operators can pin a specific count
  via the TOML knob for predictable resource accounting. The floor of
  2 matters: on a sidecar with `cpu: 100m` (~0.1 CPU),
  `available_parallelism()` can return 1, which would leave the runtime
  with a single worker shared between reqwest's connection loop and
  per-request dispatch tasks — a recipe for self-deadlock under load.
- `pool_max_idle_per_host` config option (default 32). Bounds the
  per-target idle-connection pool. reqwest's default is `usize::MAX`,
  which is fine for a single hot mirror target but explodes for
  fan-out mirroring across many distinct backends. Setting 0 disables
  idle reuse entirely — useful for setups where pool reuse would mask
  staleness in a churning target set.

### Plugin contract update

`[plugins.params]` grows two new optional fields:

```toml
[plugins.params]
worker_threads = 4            # optional; auto-derived when omitted
pool_max_idle_per_host = 32   # default
```

No breaking changes to the SPOE message contract or the plugin ABI.

## [0.1.0] - 2026-05-13

### Added

- HTTP request mirroring plugin for `haproxy-spoa-hub`, designed for
  Gateway API `HTTPRouteFilter` of type `RequestMirror`.
- Dynamic per-request mirror target via the `arg_target` SPOE arg —
  unlike upstream `haproxytech/spoa-mirror`, the destination is not
  hardcoded at agent startup. This lets one plugin instance serve every
  HTTPRoute in a cluster with its own per-route mirror backend.
- Plain HTTP and HTTPS scheme support via reqwest with `rustls-tls`
  (no OpenSSL dependency). Selected per-request via `arg_scheme`.
- Body mirroring up to a configurable cap (`max_body_bytes`, default
  1 MiB). Bodies larger than the cap are truncated; the mirror request
  is still sent.
- Bounded in-flight concurrency (`request_concurrency`, default 1024).
  When saturated, excess dispatches are dropped (rather than queued) to
  keep the SPOA worker thread responsive under burst load.
- Hop-by-hop headers (RFC 7230 §6.1) stripped before dispatch:
  `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`,
  `TE`, `Trailers`, `Transfer-Encoding`, `Upgrade`, `Host`.
- TLS verification toggle (`verify_tls`, default `false`) — defaults to
  off so the conformance suite's in-cluster self-signed mirror backends
  work out of the box. Flip to `true` once `BackendTLSPolicy`-style
  trust material is threaded through from the chart.
- Configurable per-request HTTP timeout (`timeout_ms`, default 200 ms).
- Session-aware drop counter accessible via `MirrorClient::dropped()` for
  conformance-test resilience (the percentage sub-cases run 500
  concurrent requests; we need to detect overflow rather than chase
  statistical flakes).
- GitLab CI pipeline (fmt, clippy, test, bench, build, smoke, sign,
  upload, release) matching the sibling fingerprinting plugin's shape.
- Release builds use LTO and single codegen unit; `cargo auditable`
  embeds Cargo.lock for SBOM enumeration; cosign keyless signs every
  artifact via GitLab OIDC.

### Plugin contract

```toml
[[plugins]]
name = "mirror"
library = "libmirror_plugin.so"
messages = ["mirror"]

[plugins.params]
timeout_ms = 200
max_body_bytes = 1048576
request_concurrency = 1024
verify_tls = false
```

### SPOE message contract

```
spoe-message mirror
  args arg_target=str(<host:port>) \
       arg_scheme=str(http|https) \
       arg_method=method \
       arg_path=path \
       arg_query=query \
       arg_ver=req.ver \
       arg_hdrs=req.hdrs_bin \
       arg_body=req.body
  event on-frontend-http-request
```
