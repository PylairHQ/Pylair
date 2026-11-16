//! Phase 1 of `run_child`: wait for `hello`, check the protocol version,
//! go Ready (docs/protocol-v1.md section 4).

use std::ops::ControlFlow;
use std::time::Instant;

use serde_json::Value;

use super::super::config::{RuntimeEvent, State};
use super::{kill_and_reap, ChildIo, ExitCause, Supervisor};
use crate::protocol::{self as proto, InMsg};

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
        let _ = self
            .events
            .try_send(RuntimeEvent::Exposes(hello["exposes"].clone()));
        self.ready_since = Some(Instant::now());
        self.set_state(State::Ready, "handshake complete");
        ControlFlow::Continue(io)
    }
}
