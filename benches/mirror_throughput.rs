//! Microbenchmark on the dispatch hot path: how fast can we extract a
//! mirror request from an SPOE message and turn it into a reqwest
//! `Request`? The real bottleneck in production is the upstream TCP
//! roundtrip; this benchmark just guards against accidental regressions
//! in the synchronous chunk of work the SPOA worker thread is on the
//! hook for.

use criterion::{Criterion, criterion_group, criterion_main};
use haproxy_spoa_hub_plugin_api::{RHashMap, RString, SpoeMessage, SpoeValue};

use mirror_plugin::request::extract_mirror_request;

fn make_message(body_len: usize) -> SpoeMessage {
    let mut args = RHashMap::new();
    args.insert(
        RString::from("arg_target"),
        SpoeValue::String("backend.svc.cluster.local:80".into()),
    );
    args.insert(
        RString::from("arg_scheme"),
        SpoeValue::String("http".into()),
    );
    args.insert(
        RString::from("arg_method"),
        SpoeValue::String("POST".into()),
    );
    args.insert(
        RString::from("arg_path"),
        SpoeValue::String("/api/mirror".into()),
    );
    args.insert(RString::from("arg_query"), SpoeValue::String("k=v".into()));

    // Synthetic req.hdrs_bin: 5 small headers.
    let mut hdrs = Vec::new();
    for (n, v) in &[
        ("user-agent", "curl/8.0"),
        ("accept", "*/*"),
        ("x-trace", "abc123"),
        ("content-type", "application/json"),
        ("authorization", "Bearer xyz"),
    ] {
        hdrs.push(n.len() as u8);
        hdrs.extend_from_slice(n.as_bytes());
        hdrs.push(v.len() as u8);
        hdrs.extend_from_slice(v.as_bytes());
    }
    hdrs.push(0u8);
    args.insert(RString::from("arg_hdrs"), SpoeValue::Binary(hdrs.into()));

    let body = vec![0xAB; body_len];
    args.insert(RString::from("arg_body"), SpoeValue::Binary(body.into()));

    SpoeMessage {
        name: RString::from("mirror"),
        args,
        stream_id: 1,
        frame_id: 1,
    }
}

fn bench_extract(c: &mut Criterion) {
    let small = make_message(256);
    let medium = make_message(16 * 1024);
    let large = make_message(1024 * 1024);

    c.bench_function("extract_small_body", |b| {
        b.iter(|| extract_mirror_request(&small, 1024 * 1024))
    });
    c.bench_function("extract_16k_body", |b| {
        b.iter(|| extract_mirror_request(&medium, 1024 * 1024))
    });
    c.bench_function("extract_1m_body", |b| {
        b.iter(|| extract_mirror_request(&large, 1024 * 1024))
    });
}

criterion_group!(benches, bench_extract);
criterion_main!(benches);
