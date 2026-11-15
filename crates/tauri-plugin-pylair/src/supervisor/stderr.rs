//! Runtime stderr: always drained, split into bounded lossy lines, and
//! queued drop-oldest ahead of the event channel (protocol-v1.md section 9).

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::config::RuntimeEvent;

/// stderr line + queue plumbing (protocol §9: drain unconditionally,
/// drop OLDEST with a "[dropped N lines]" marker under backpressure).
///
/// One delivered line caps at this many raw bytes; the rest of the line
/// is swallowed and the delivered text ends with the trunc marker.
pub(super) const STDERR_LINE_CAP: usize = 8 * 1024;
/// Lines buffered between the always-draining reader and the bounded
/// event channel. Full → oldest entries are evicted and counted.
pub(super) const STDERR_QUEUE_CAP: usize = 512;
pub(super) const STDERR_TRUNC_MARK: &str = "…[truncated]";

/// Bounded, lossy stderr line assembler: splits raw bytes on `\n`,
/// converts each line with `from_utf8_lossy` (bad bytes become `U+FFFD`,
/// never an error), and caps a single line at `STDERR_LINE_CAP` input
/// bytes — an over-long line is delivered truncated with a visible
/// marker instead of growing the buffer without bound.
pub(super) struct StderrLines {
    buf: Vec<u8>,
    /// Swallowing the tail of an over-cap line until its `\n`.
    over: bool,
}

impl StderrLines {
    pub(super) fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1024),
            over: false,
        }
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                out.push(self.take());
            } else if !self.over {
                if self.buf.len() < STDERR_LINE_CAP {
                    self.buf.push(b);
                } else {
                    // A byte is actually being dropped — only now is the
                    // line truncated. A line of exactly STDERR_LINE_CAP
                    // bytes is delivered unmarked.
                    self.over = true;
                }
            } // else: swallow — cap already reached for this line
        }
        out
    }

    /// Flush a partial line at EOF (no trailing newline).
    pub(super) fn finish(&mut self) -> Option<String> {
        (!self.buf.is_empty() || self.over).then(|| self.take())
    }

    fn take(&mut self) -> String {
        let mut s = String::from_utf8_lossy(&self.buf).into_owned();
        if self.over {
            s.push_str(STDERR_TRUNC_MARK);
        }
        self.buf.clear();
        self.over = false;
        s
    }
}

enum SinkItem {
    Line(String),
    /// Drain ended — forward exits after the queue empties.
    Eof,
}

#[derive(Default)]
struct SinkState {
    q: VecDeque<SinkItem>,
    /// Lines evicted but not yet reported. The forwarder emits the
    /// cumulative "[dropped N stderr lines]" marker at delivery time —
    /// markers can't be evicted and the count can never be lost or
    /// double-counted.
    dropped: u64,
}

/// The drop-oldest bound the event channel can't express: a bounded
/// queue between the always-draining pipe reader (never blocks) and the
/// `mpsc` forwarder (blocks on backpressure). Under overflow the oldest
/// undelivered line is evicted and counted; the next pushed line is
/// preceded by a `[dropped N stderr lines]` marker — the documented
/// §9/§5 contract `try_send` (drop-newest, silent) could not provide.
pub(super) struct StderrSink {
    state: std::sync::Mutex<SinkState>,
    notify: tokio::sync::Notify,
}

impl StderrSink {
    pub(super) fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SinkState::default()),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub(super) fn push(&self, line: String) {
        {
            let mut st = self.state.lock().unwrap();
            while st.q.len() >= STDERR_QUEUE_CAP {
                match st.q.pop_front() {
                    Some(SinkItem::Line(_)) => st.dropped += 1,
                    // Eof only ever sits at the tail of a draining queue;
                    // if we ever see it at the front the invariant broke —
                    // keep it rather than swallow it.
                    Some(SinkItem::Eof) => {
                        st.q.push_back(SinkItem::Eof);
                        break;
                    }
                    None => break,
                }
            }
            st.q.push_back(SinkItem::Line(line));
        }
        self.notify.notify_one();
    }

    pub(super) fn finish(&self) {
        self.state.lock().unwrap().q.push_back(SinkItem::Eof);
        self.notify.notify_one();
    }

    /// Forward queued lines onto the event channel; exits on Eof or a
    /// closed receiver. Blocks only on `send` — never on the reader.
    pub(super) async fn forward(self: Arc<Self>, ev: mpsc::Sender<RuntimeEvent>) {
        loop {
            // Register interest before polling so a push between the
            // check and the wait can't be missed (Notify keeps a permit,
            // but this order is the canonical no-race form).
            let notified = self.notify.notified();
            let (item, dropped) = {
                let mut st = self.state.lock().unwrap();
                (st.q.pop_front(), std::mem::take(&mut st.dropped))
            };
            if dropped > 0 {
                let marker = format!("[dropped {dropped} stderr lines]");
                if ev.send(RuntimeEvent::Stderr(marker)).await.is_err() {
                    return;
                }
            }
            match item {
                Some(SinkItem::Line(line)) => {
                    if ev.send(RuntimeEvent::Stderr(line)).await.is_err() {
                        return;
                    }
                }
                Some(SinkItem::Eof) => return,
                None => notified.await,
            }
        }
    }
}
