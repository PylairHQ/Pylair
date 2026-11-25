//! Phase 1 of `run_child`: wait for `hello`, check the protocol version,
//! go Ready (docs/protocol-v1.md section 4).

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::timeout;

use super::super::config::{RuntimeEvent, RuntimeKind, State};
use super::super::fairness::release_queued;
use super::super::handle::{Command, ShutdownOutcome};
use super::{fail_command_fast, kill_and_reap, ChildIo, ExitCause, Supervisor};
use crate::protocol::{self as proto, InMsg};
use crate::util::validate_ready_url;

impl Supervisor {
    /// Spawn-to-Ready. `Continue` hands the `ChildIo` on to the serving
    /// phase; `Break` is the exit cause of a child that never became
    /// Ready (it has already been killed and reaped).
    pub(super) async fn handshake(&mut self, mut io: ChildIo) -> ControlFlow<ExitCause, ChildIo> {
        // ---- handshake: hello within deadline ----
        // At half the deadline, emit a repeated `starting → starting`
        // state event so UIs can show "still starting" — informational
        // only, never a state write (set_state dedupes same-state), and
        // at most once per spawn attempt. The timer dies with this
        // select: it cannot fire after hello, EOF, or a shutdown-driven
        // early return.
        let hello = {
            let mut deadline = Box::pin(tokio::time::sleep(self.config.hello_timeout));
            let half_mark = Box::pin(tokio::time::sleep(self.config.hello_timeout / 2));
            tokio::pin!(half_mark);
            let mut slow_noted = false;
            loop {
                tokio::select! {
                    m = io.msg_in.recv() => break Ok(m),
                    _ = &mut half_mark, if !slow_noted => {
                        slow_noted = true;
                        let _ = self.events.try_send(RuntimeEvent::StateChanged {
                            from: State::Starting,
                            to: State::Starting,
                            detail: format!(
                                "still starting ({:?} elapsed; timeout at {:?}, \
                                 see PYLAIR_HELLO_TIMEOUT_MS)",
                                self.config.hello_timeout / 2,
                                self.config.hello_timeout
                            ),
                        });
                    }
                    _ = &mut deadline => break Err(()),
                }
            }
        };
        let hello = match hello {
            Ok(Some(Ok(InMsg::Notification { method, params }))) if method == "hello" => {
                params.unwrap_or(Value::Null)
            }
            Ok(Some(Ok(_))) => {
                kill_and_reap(&mut io).await;
                return ControlFlow::Break(ExitCause::Crashed("first frame was not hello".into()));
            }
            Ok(Some(Err(e))) => {
                kill_and_reap(&mut io).await;
                return ControlFlow::Break(ExitCause::Crashed(format!(
                    "protocol violation during hello: {e}"
                )));
            }
            Ok(None) | Err(_) => {
                kill_and_reap(&mut io).await;
                return ControlFlow::Break(ExitCause::Crashed(format!(
                    "runtime produced no hello within {:?} \
                     (slow interpreter, first-run venv creation, or a wedged import). \
                     Raise via PYLAIR_HELLO_TIMEOUT_MS or Builder::hello_timeout; \
                     see protocol-v1 §4",
                    self.config.hello_timeout
                )));
            }
        };

        let version_ok = hello["protocol"].as_u64() == Some(proto::PROTOCOL_VERSION as u64);
        if !version_ok {
            kill_and_reap(&mut io).await;
            // Not a crash — a deployment bug. Retrying can't fix it.
            return ControlFlow::Break(ExitCause::Fatal(format!(
                "protocol version mismatch: {:?}",
                hello["protocol"]
            )));
        }
        // The generation's fn_name -> kind manifest: isolated calls
        // are charged per owner for the window-share check (decisions
        // 0005, PR 4). Replaced every generation so a dev reload's
        // new manifest applies; an absent name is non-isolated.
        self.fn_kinds = hello["exposes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some((
                            e["name"].as_str()?.to_string(),
                            e["kind"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Warm-pool membership (decision 0008) rides the manifest as
        // `worker: "warm"` — the inspector's mode column needs it.
        self.fn_warm = hello["exposes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|e| e["worker"].as_str() == Some("warm"))
                    .filter_map(|e| e["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let _ = self
            .events
            .try_send(RuntimeEvent::Exposes(hello["exposes"].clone()));
        if self.config.kind == RuntimeKind::Web {
            // A web runtime is not ready on hello: its server may still be
            // loading. Wait for `ready {url}` (protocol-v1 section 4.1).
            let (io, url) = match self.await_ready(io).await {
                ControlFlow::Continue(ok) => ok,
                ControlFlow::Break(cause) => return ControlFlow::Break(cause),
            };
            self.ready_since = Some(Instant::now());
            self.health.lock().unwrap().note_ready(self.ready_since);
            // The url is published before the transition so that anyone
            // woken by the Ready state already finds it.
            self.set_ready_url(Some(url));
            self.set_state(State::Ready, "web server ready");
            return ControlFlow::Continue(io);
        }
        self.ready_since = Some(Instant::now());
        self.health.lock().unwrap().note_ready(self.ready_since);
        self.set_state(State::Ready, "handshake complete");
        ControlFlow::Continue(io)
    }

    /// `Web` kind, hello to Ready: wait for `ready {url}` for at most
    /// `ready_timeout`, validate the url, and stay responsive to shutdown.
    ///
    /// Unlike the (short) hello wait, this one can last minutes, so the
    /// command channel is served: a shutdown is honoured at once and calls
    /// fail fast (a web runtime exposes nothing to queue them for).
    /// `Break` carries the exit cause of a child that never became Ready
    /// (already killed and reaped, or already exited).
    async fn await_ready(&mut self, mut io: ChildIo) -> ControlFlow<ExitCause, (ChildIo, String)> {
        let deadline = tokio::time::sleep(self.config.ready_timeout);
        tokio::pin!(deadline);
        let url = loop {
            tokio::select! {
                msg = io.msg_in.recv() => match msg {
                    Some(Ok(InMsg::Notification { method, params })) if method == "ready" => {
                        match validate_ready_url(params.as_ref()) {
                            Ok(url) => break url,
                            Err(why) => {
                                kill_and_reap(&mut io).await;
                                return ControlFlow::Break(ExitCause::Crashed(format!(
                                    "protocol violation: bad ready url: {why}"
                                )));
                            }
                        }
                    }
                    // Pongs and any other notification are fine while waiting.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        kill_and_reap(&mut io).await;
                        return ControlFlow::Break(ExitCause::Crashed(format!(
                            "protocol violation before ready: {e}"
                        )));
                    }
                    None => {
                        // stdout closed: the launcher is gone or going.
                        let waited = timeout(Duration::from_secs(5), &mut io.exited).await;
                        let detail = match &waited {
                            Ok(Ok(Ok(status))) => format!("exited {status}"),
                            _ => "runtime gone".into(),
                        };
                        if waited.is_err() {
                            kill_and_reap(&mut io).await;
                        }
                        return ControlFlow::Break(ExitCause::Crashed(format!(
                            "web runtime ended before ready: {detail}"
                        )));
                    }
                },
                status = &mut io.exited => {
                    let detail = match status {
                        Ok(Ok(s)) => format!("exited {s}"),
                        Ok(Err(e)) => format!("wait error: {e}"),
                        Err(_) => "monitor dropped".into(),
                    };
                    return ControlFlow::Break(ExitCause::Crashed(format!(
                        "web runtime ended before ready: {detail}"
                    )));
                }
                cmd = self.cmd_rx.recv() => match cmd {
                    None => {
                        // every handle dropped: nobody is left to serve
                        let outcome = if kill_and_reap(&mut io).await {
                            ShutdownOutcome::ForcedKill
                        } else {
                            ShutdownOutcome::Incomplete
                        };
                        self.shutdown_outcome = Some(outcome);
                        return ControlFlow::Break(ExitCause::CleanShutdown);
                    }
                    Some(Command::Shutdown { ack }) => {
                        self.shutdown_acks.push(ack);
                        let outcome = if kill_and_reap(&mut io).await {
                            ShutdownOutcome::ForcedKill
                        } else {
                            ShutdownOutcome::Incomplete
                        };
                        self.shutdown_outcome = Some(outcome);
                        return ControlFlow::Break(ExitCause::CleanShutdown);
                    }
                    Some(Command::Retry | Command::CancelOwned { .. }) => {}
                    Some(c) => {
                        // A dequeued command always frees its queue
                        // slot, even when it is fail-fasted
                        // (decisions 0005).
                        release_queued(&self.fair, &c);
                        fail_command_fast(c, "web runtime is starting");
                    }
                },
                _ = &mut deadline => {
                    kill_and_reap(&mut io).await;
                    return ControlFlow::Break(ExitCause::Crashed(format!(
                        "web server sent no `ready` within {:?} of hello (a slow import or \
                         model load, or a server that never answers HTTP). \
                         Raise Builder::ready_timeout / SupervisorConfig::ready_timeout; \
                         see protocol-v1 section 4.1",
                        self.config.ready_timeout
                    )));
                }
            }
        };
        ControlFlow::Continue((io, url))
    }
}
