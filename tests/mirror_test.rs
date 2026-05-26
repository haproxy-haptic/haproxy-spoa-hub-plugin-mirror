//! End-to-end test: dispatch a mirrored request through `MirrorClient`
//! and verify the in-process mock backend received the right method,
//! path, headers, and body.
//!
//! The mock backend uses hyper directly rather than reqwest so the
//! client and server don't share a code path.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use mirror_plugin::client::MirrorClient;
use mirror_plugin::config::PluginConfig;
use mirror_plugin::request::{MirrorRequest, Scheme};

#[derive(Debug, Clone, Default)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

/// Start a one-shot HTTP server on `127.0.0.1:0` that captures the
/// first incoming request, sends it through the oneshot channel, and
/// shuts down. Returns the bound address.
async fn start_mock_backend() -> (SocketAddr, oneshot::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel();
    let captured = Arc::new(Mutex::new(Some(tx)));

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let captured = captured.clone();
        let svc = service_fn(move |req: Request<Incoming>| {
            let captured = captured.clone();
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let headers: Vec<(String, Vec<u8>)> = req
                    .headers()
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
                    .collect();
                let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
                if let Some(tx) = captured.lock().unwrap().take() {
                    let _ = tx.send(CapturedRequest {
                        method,
                        path,
                        headers,
                        body,
                    });
                }
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b""))))
            }
        });
        let _ = http1::Builder::new().serve_connection(io, svc).await;
    });

    (addr, rx)
}

#[tokio::test]
async fn dispatches_get_request() {
    let (addr, rx) = start_mock_backend().await;
    let cfg = PluginConfig::default();
    let client = MirrorClient::new(&cfg).unwrap();

    client
        .dispatch(MirrorRequest {
            target: addr.to_string(),
            scheme: Scheme::Http,
            method: "GET".to_string(),
            path: "/mirror/path".to_string(),
            query: None,
            headers: vec![("X-Test".to_string(), b"hello".to_vec())],
            body: vec![],
            timeout_ms: None,
            retries: None,
        })
        .await;

    let captured = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("backend received request")
        .expect("oneshot ok");
    assert_eq!(captured.method, "GET");
    assert_eq!(captured.path, "/mirror/path");
    assert!(
        captured
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-test") && v == b"hello")
    );
    assert!(captured.body.is_empty());
}

#[tokio::test]
async fn dispatches_post_with_body() {
    let (addr, rx) = start_mock_backend().await;
    let cfg = PluginConfig::default();
    let client = MirrorClient::new(&cfg).unwrap();

    client
        .dispatch(MirrorRequest {
            target: addr.to_string(),
            scheme: Scheme::Http,
            method: "POST".to_string(),
            path: "/submit".to_string(),
            query: Some("k=v".to_string()),
            headers: vec![("Content-Type".to_string(), b"application/json".to_vec())],
            body: br#"{"hello":"world"}"#.to_vec(),
            timeout_ms: None,
            retries: None,
        })
        .await;

    let captured = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("backend received request")
        .expect("oneshot ok");
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/submit");
    assert_eq!(captured.body, br#"{"hello":"world"}"#.to_vec());
    assert!(
        captured
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == b"application/json")
    );
}

#[tokio::test]
async fn hop_by_hop_headers_stripped() {
    let (addr, rx) = start_mock_backend().await;
    let cfg = PluginConfig::default();
    let client = MirrorClient::new(&cfg).unwrap();

    client
        .dispatch(MirrorRequest {
            target: addr.to_string(),
            scheme: Scheme::Http,
            method: "GET".to_string(),
            path: "/".to_string(),
            query: None,
            headers: vec![
                ("Connection".to_string(), b"upgrade".to_vec()),
                ("Transfer-Encoding".to_string(), b"chunked".to_vec()),
                ("X-Keep".to_string(), b"yes".to_vec()),
            ],
            body: vec![],
            timeout_ms: None,
            retries: None,
        })
        .await;

    let captured = tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("backend received request")
        .expect("oneshot ok");
    // X-Keep flows through; the hop-by-hop ones do not.
    assert!(
        captured
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-keep") && v == b"yes")
    );
    assert!(
        !captured
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("connection")
                || k.eq_ignore_ascii_case("transfer-encoding"))
    );
}

/// Start a backend that accepts the connection, signals "request in
/// flight" via the channel, and then never responds (closes when the
/// task is dropped). Used to deterministically hold a mirror permit.
async fn start_slow_backend() -> (SocketAddr, tokio::sync::mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(1);

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let tx = tx.clone();
        let svc = service_fn(move |_req: Request<Incoming>| {
            let tx = tx.clone();
            async move {
                // Signal that we've received the request — the test
                // can now be sure the first dispatch is past the
                // semaphore acquire.
                let _ = tx.send(()).await;
                // Hold the response indefinitely; the client will
                // eventually time out or the test will tear us down.
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())))
            }
        });
        let _ = http1::Builder::new().serve_connection(io, svc).await;
    });

    (addr, rx)
}

#[tokio::test]
async fn dropped_when_concurrency_saturated() {
    // Long timeout so the first dispatch stays in flight while we fire
    // the second. The first request hits a backend that accepts but
    // never replies; we use a channel from the backend to know
    // deterministically when the permit is held.
    let cfg = PluginConfig {
        request_concurrency: Some(1),
        timeout_ms: 60_000,
        ..PluginConfig::default()
    };
    let client = MirrorClient::new(&cfg).unwrap();

    let (addr, mut rx) = start_slow_backend().await;
    let req = MirrorRequest {
        target: addr.to_string(),
        scheme: Scheme::Http,
        method: "GET".to_string(),
        path: "/".to_string(),
        query: None,
        headers: vec![],
        body: vec![],
        timeout_ms: None,
        retries: None,
    };

    let busy = {
        let client = client.clone();
        let req = req.clone();
        tokio::spawn(async move { client.dispatch(req).await })
    };
    // Wait until the backend has actually received the first request —
    // at this point the permit is definitively held.
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("first dispatch reached backend")
        .expect("channel ok");

    // Second dispatch must drop because the only permit is taken.
    client.dispatch(req).await;
    assert_eq!(client.dropped(), 1);

    busy.abort();
}

#[tokio::test]
async fn in_flight_tracks_pending_dispatches() {
    // Verify the in_flight() accessor returns 0 → 1 → 0 around a
    // single dispatch. This is what the plugin's drain() spin-waits
    // on; getting the counter math wrong would silently break drain.
    let (addr, rx) = start_mock_backend().await;
    let cfg = PluginConfig::default();
    let client = MirrorClient::new(&cfg).unwrap();

    assert_eq!(
        client.in_flight(),
        0,
        "fresh client must report no in-flight"
    );

    let req = MirrorRequest {
        target: addr.to_string(),
        scheme: Scheme::Http,
        method: "GET".to_string(),
        path: "/in-flight".to_string(),
        query: None,
        headers: vec![],
        body: vec![],
        timeout_ms: None,
        retries: None,
    };

    // Dispatch returns AFTER the HTTP exchange completes (no spawn
    // inside dispatch — the spawn happens at the plugin level). So
    // racing dispatch.await against in_flight observation requires a
    // tokio::spawn to free this task to observe the counter
    // mid-dispatch. Same pattern as `dropped_on_saturation` above.
    let in_flight_view = {
        let client = client.clone();
        let req = req.clone();
        tokio::spawn(async move { client.dispatch(req).await })
    };

    // Wait until the backend has actually received the request — at
    // this point the permit is definitively held AND in_flight has
    // been incremented (the increment happens before the spawn that
    // sends the HTTP request).
    let captured = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("backend received request")
        .expect("oneshot ok");
    assert_eq!(captured.path, "/in-flight");

    // Mid-dispatch — counter must show 1.
    // (Some scheduler jitter possible; allow a tiny retry window.)
    let mid_count = {
        let mut observed = 0;
        for _ in 0..20 {
            let v = client.in_flight();
            if v >= 1 {
                observed = v;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        observed
    };
    assert_eq!(mid_count, 1, "in_flight must show the one pending dispatch");

    // Let the dispatch complete; in_flight must return to 0.
    in_flight_view.await.expect("dispatch task panicked");

    // After dispatch returns, the permit is released and the
    // counter decremented. The decrement is in the dispatch path's
    // final release_permit call, so by the time `.await` resolves
    // here, in_flight is back to 0 — no polling needed.
    assert_eq!(
        client.in_flight(),
        0,
        "in_flight must return to 0 after dispatch"
    );
}

#[tokio::test]
async fn retries_on_connection_error_until_budget_exhausted() {
    // Per-dispatch retries=2 against a closed port. Each attempt
    // fails with `is_connect()` (TCP RST on connection refused), the
    // dispatcher backs off 100ms and retries. With 2 retries the
    // total attempt count is 3 (initial + 2 retries) and the
    // minimum elapsed includes 2 backoff windows = ≥200ms.
    //
    // Pins the contract that the chart-side wiring relies on: when
    // arg_retries=N is passed via SPOE, the plugin makes (N+1)
    // attempts before giving up. Without this test, a refactor that
    // accidentally caps attempts at 1 (e.g. an off-by-one in the
    // `attempt < max_attempts` predicate) would silently lose the
    // resilience the chart is paying for in latency.
    let cfg = PluginConfig::default();
    let client = MirrorClient::new(&cfg).unwrap();

    // Bind a port and immediately close to guarantee "connection
    // refused" on subsequent connects. Just using a random
    // unbound port works on Linux but is racy under heavy CI load.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let req = MirrorRequest {
        target: addr.to_string(),
        scheme: Scheme::Http,
        method: "GET".to_string(),
        path: "/".to_string(),
        query: None,
        headers: vec![],
        body: vec![],
        timeout_ms: Some(2_000),
        retries: Some(2),
    };

    let started = std::time::Instant::now();
    client.dispatch(req).await;
    let elapsed = started.elapsed();

    // 2 retries × 100ms backoff = 200ms minimum total. The actual
    // per-attempt connect-refused returns immediately on Linux, so
    // the elapsed is dominated by the two backoff sleeps. Use a
    // loose lower bound (180ms) to absorb scheduler jitter in CI.
    assert!(
        elapsed >= Duration::from_millis(180),
        "retried dispatch must wait at least 2 × 100ms backoff, got {:?}",
        elapsed
    );
}

#[tokio::test]
async fn arg_retries_zero_makes_single_attempt() {
    // arg_retries=Some(0) is "no retries" — exactly one attempt.
    // Distinct from arg_retries=None which falls back to plugin
    // config default. The dispatch against an unbound port returns
    // promptly (no backoffs).
    let cfg = PluginConfig::default();
    let client = MirrorClient::new(&cfg).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let req = MirrorRequest {
        target: addr.to_string(),
        scheme: Scheme::Http,
        method: "GET".to_string(),
        path: "/".to_string(),
        query: None,
        headers: vec![],
        body: vec![],
        timeout_ms: Some(2_000),
        retries: Some(0),
    };

    let started = std::time::Instant::now();
    client.dispatch(req).await;
    let elapsed = started.elapsed();

    // No backoff windows at all. Pin a tight upper bound to catch a
    // future refactor that accidentally treats retries=0 as "default
    // retry count" (which would surface as a noticeable >100ms
    // elapsed because of at least one backoff).
    assert!(
        elapsed < Duration::from_millis(80),
        "retries=0 must mean exactly one attempt with no backoff, got {:?}",
        elapsed
    );
}
