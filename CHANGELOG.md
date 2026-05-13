# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
