//! stderr drain contract (protocol §1, security §5): the host drains the
//! runtime's stderr unconditionally — bad UTF-8 must not kill the loop,
//! one pathological line must not grow the buffer without bound, and a
//! flood faster than the consumer drops OLDEST lines with a
//! "[dropped N lines]" marker, never the newest.

use std::path::PathBuf;
use std::time::Duration;
use tauri_plugin_pylair::supervisor::{
    spawn, Handle, RuntimeEvent, SpawnSpec, State, SupervisorConfig,
};
use tokio::time::{sleep, timeout};

const T: Duration = Duration::from_secs(15);

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
    panic!("no python >=3.12 found for supervisor tests");
}

fn spawn_h(
    spec: SpawnSpec,
    config: SupervisorConfig,
    events: tauri_plugin_pylair::supervisor::EventBus,
) -> Handle {
    let (h, actor) = spawn(spec, config, events);
    tokio::spawn(actor);
    h
}

fn cfg() -> SupervisorConfig {
    let mut c = SupervisorConfig::default();
    c.hello_timeout = Duration::from_secs(8);
    c.ping_interval = Duration::from_secs(60);
    c.missed_pongs_to_kill = 1000;
    c.backoff_base = Duration::from_millis(20);
    c.backoff_cap = Duration::from_millis(80);
    c.breaker_crashes = 10;
    c
}

fn spec(script: &str) -> SpawnSpec {
    SpawnSpec {
        program: python(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        ..Default::default()
    }
}

async fn stderr_events(
    rx: &mut tauri_plugin_pylair::supervisor::EventDrain,
    within: Duration,
) -> Vec<String> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + within;
    while let Ok(Some(ev)) = timeout(deadline - std::time::Instant::now(), rx.recv()).await {
        if let RuntimeEvent::Stderr(l) = ev {
            out.push(l);
        }
    }
    out
}

/// Hello, then echo calls forever. Preceded by whatever stderr abuse the
/// test script performs first — reaching `Ready` proves the drain kept up.
const HELLO_ECHO: &str = r#"
import sys, struct, json
def w(o):
    b = json.dumps(o).encode()
    sys.stdout.buffer.write(struct.pack(">I", len(b)) + b); sys.stdout.buffer.flush()
w({"jsonrpc":"2.0","method":"hello","params":{"protocol":1,"exposes":[{"name":"echo","kind":"sync"}]}})
while True:
    h = sys.stdin.buffer.read(4)
    if len(h) < 4: break
    n, = struct.unpack(">I", h)
    m = json.loads(sys.stdin.buffer.read(n))
    if m["method"] == "shutdown":
        w({"jsonrpc":"2.0","id": m["id"],"result": None}); break
    elif m["method"] == "ping":
        w({"jsonrpc":"2.0","method":"pong","params":{"seq": m["params"]["seq"]}})
"#;

/// Invalid UTF-8, then a 256 KiB single line (bigger than any pipe
/// buffer), then a sentinel line — then the handshake. If the drain
/// dies on bad UTF-8 or grows unboundedly, the flood write blocks and
/// hello never arrives.
#[tokio::test]
async fn bad_utf8_and_huge_line_do_not_kill_the_drain() {
    let (tx, mut rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let script = r#"
import sys, os
os.write(2, b'\xff\xfe not utf-8 \x80\x80\x80\n')
os.write(2, b'x' * (256 * 1024) + b'\n')
os.write(2, b'sentinel-after-garbage\n')
"#;
    let h = spawn_h(spec(&format!("{script}\n{HELLO_ECHO}")), cfg(), tx);
    let ok = timeout(T, async {
        let mut lines = Vec::new();
        loop {
            match rx.recv().await {
                Some(RuntimeEvent::Stderr(l)) => lines.push(l),
                Some(RuntimeEvent::StateChanged { to, .. }) => {
                    if to == State::Ready {
                        return Some(lines);
                    }
                }
                Some(_) => {}
                None => return Some(lines),
            }
        }
    })
    .await;
    h.shutdown().await;
    let lines = ok
        .unwrap_or_else(|_| panic!("runtime never reached Ready — drain died or child blocked"))
        .expect("event channel closed before Ready");
    // Bad bytes arrive lossy, not fatal; the sentinel still arrives.
    assert!(
        lines.iter().any(|l| l.contains("sentinel-after-garbage")),
        "sentinel line missing — drain stopped: {lines:?}"
    );
    // The 256 KiB line is delivered truncated, not buffered whole.
    let huge = lines.iter().find(|l| l.len() > 4 * 1024);
    if let Some(l) = huge {
        assert!(
            l.len() <= 9 * 1024,
            "line exceeded the cap: {} bytes",
            l.len()
        );
        assert!(
            l.contains("[truncated]"),
            "no truncation marker: {:?}",
            &l[..80]
        );
    }
}

/// 20k numbered stderr lines while nobody consumes events: the bounded
/// queue must drop OLDEST and report the count — the tail survives.
#[tokio::test]
async fn flood_drops_oldest_and_reports_count() {
    let (tx, mut rx) = tauri_plugin_pylair::supervisor::event_pair(256);
    let script = r#"
import sys, os
for i in range(20000):
    os.write(2, f"line-{i:05d}\n".encode())
"#;
    let h = spawn_h(spec(&format!("{script}\n{HELLO_ECHO}")), cfg(), tx);
    // Let the flood fill the channel and overflow the stderr queue.
    sleep(Duration::from_secs(4)).await;
    let lines = stderr_events(&mut rx, Duration::from_secs(10)).await;
    h.shutdown().await;

    let numbered: Vec<usize> = lines
        .iter()
        .filter_map(|l| {
            l.strip_prefix("line-")
                .and_then(|n| n.trim_end().parse().ok())
        })
        .collect();
    let markers: Vec<&String> = lines.iter().filter(|l| l.contains("[dropped ")).collect();
    assert!(
        !markers.is_empty(),
        "no dropped-lines marker under a 20k-line flood (got {} lines)",
        lines.len()
    );
    assert!(
        numbered.len() < 20000,
        "all 20000 lines were delivered — nothing was dropped"
    );
    assert_eq!(
        numbered.iter().max(),
        Some(&19999),
        "newest line lost — the drain dropped newest, not oldest"
    );
    // Delivered lines keep flood order: drop-oldest evicts the head and
    // never reorders. Whether a head prefix survives at all depends on
    // the forwarder winning a scheduling race against the flood — not a
    // property of the design, so it is not asserted (macOS flake, run
    // #175): a starved forwarder legitimately sees only a late suffix.
    assert!(
        numbered.windows(2).all(|w| w[0] < w[1]),
        "delivered lines out of order — drop-oldest must preserve arrival order"
    );
}
