//! Host<->Python latency bench — `#[ignore]`d, run manually with
//! `cargo test --test bench_latency -- --ignored --nocapture`.
//!
//! Drives a REAL `python -m pylair testapp` runtime through `Handle::call`
//! / `Handle::call_stream` with the same payloads the installed-webview
//! bench times from JS (docs/performance.md, PYLAIR_SELF_TEST_BENCH), so
//! the end-to-end number can be split: webview hop on top, this leg
//! underneath. Emits one `rust-bench:` JSON line per case.
//!
//! Payloads are identical to the webview and dispatch legs:
//!   call_noop          — no args, returns an int
//!   call_64k           — one 64 KiB string argument, echoed back
//!   call_nested        — a nested dataclass argument
//!   stream_first_chunk — time to the first yielded item of counter(n)

use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, StreamItem, SupervisorConfig,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WARMUP: usize = 50;
const N: usize = 500;
const T: Duration = Duration::from_secs(30);

fn python() -> PathBuf {
    if let Ok(p) = std::env::var("PYLAIR_TEST_PYTHON") {
        return p.into();
    }
    for cand in ["python3", "python"] {
        let ok = std::process::Command::new(cand)
            .arg("-c")
            .arg("import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return cand.into();
        }
    }
    panic!("no python >=3.12 found for the bench runtime");
}

fn spec() -> SpawnSpec {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    SpawnSpec {
        program: python(),
        args: vec!["-m".into(), "pylair".into(), "testapp".into()],
        env: vec![(
            "PYTHONPATH".into(),
            root.join("python/pylair/src")
                .to_string_lossy()
                .into_owned(),
        )],
        cwd: Some(root.join("python/pylair/tests")),
        ..Default::default()
    }
}

async fn wait_ready(h: &Handle) {
    let mut rx = h.subscribe_state();
    timeout(T, async {
        while *rx.borrow() != State::Ready {
            rx.changed().await.expect("state channel closed");
        }
    })
    .await
    .expect("runtime never reached Ready");
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((p * sorted.len() as f64).ceil() as usize - 1).min(sorted.len() - 1);
    sorted[idx]
}

fn report(case: &str, samples: &mut [f64]) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "rust-bench: {}",
        json!({
            "case": case,
            "warmup": WARMUP,
            "n": N,
            "p50": percentile(samples, 0.50),
            "p95": percentile(samples, 0.95),
            "p99": percentile(samples, 0.99),
        })
    );
}

/// One plain call — no args, int back.
async fn probe_call(h: &Handle, fn_name: &str, input: &serde_json::Value) {
    h.call(fn_name, input.clone(), None, Some(false))
        .await
        .unwrap_or_else(|e| panic!("{fn_name} call failed: {e:?}"));
}

/// One streaming call — measure to the FIRST chunk only, then drain so
/// the slot frees before the next iteration.
async fn probe_stream_first(h: &Handle) {
    let (_task, mut rx) = h
        .call_stream("counter", json!({"n": 3}), Some("bench".into()), Some(true))
        .await
        .expect("call_stream rejected");
    let first = timeout(T, rx.recv())
        .await
        .expect("first chunk timed out")
        .expect("stream closed without a chunk");
    match first {
        StreamItem::Chunk { .. } => {}
        other => panic!("first stream item was {other:?}, expected a chunk"),
    }
    while let Some(item) = rx.recv().await {
        if matches!(item, StreamItem::End(_)) {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "latency bench — run manually or from bench.yml"]
async fn bench_host_python_latency() {
    let (ev, _rx) = mpsc::channel::<RuntimeEvent>(256);
    let (h, actor) = spawn(spec(), SupervisorConfig::default(), ev);
    tokio::spawn(actor);
    wait_ready(&h).await;

    let s64 = "x".repeat(64 * 1024);
    let nested = json!({
        "shape": {
            "name": "tri",
            "origin": { "x": 0, "y": 0, "label": "o" },
            "points": [
                { "x": 1, "y": 2, "label": "a" },
                { "x": 3, "y": 4, "label": "b" },
                { "x": 5, "y": 6, "label": "c" },
            ],
            "meta": { "n": 3 },
        }
    });

    // call_noop
    for _ in 0..WARMUP {
        probe_call(&h, "bench_noop", &json!({})).await;
    }
    let mut samples = Vec::with_capacity(N);
    for _ in 0..N {
        let t0 = Instant::now();
        probe_call(&h, "bench_noop", &json!({})).await;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    report("call_noop", &mut samples);

    // call_64k
    let input = json!({"s": s64});
    for _ in 0..WARMUP {
        probe_call(&h, "bench_echo", &input).await;
    }
    let mut samples = Vec::with_capacity(N);
    for _ in 0..N {
        let t0 = Instant::now();
        probe_call(&h, "bench_echo", &input).await;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    report("call_64k", &mut samples);

    // call_nested
    for _ in 0..WARMUP {
        probe_call(&h, "bench_nested", &nested).await;
    }
    let mut samples = Vec::with_capacity(N);
    for _ in 0..N {
        let t0 = Instant::now();
        probe_call(&h, "bench_nested", &nested).await;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    report("call_nested", &mut samples);

    // stream_first_chunk
    for _ in 0..WARMUP {
        probe_stream_first(&h).await;
    }
    let mut samples = Vec::with_capacity(N);
    for _ in 0..N {
        let t0 = Instant::now();
        probe_stream_first(&h).await;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    report("stream_first_chunk", &mut samples);

    h.shutdown().await;
}
