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
