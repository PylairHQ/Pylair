use crate::fn_scope::{self, FnScope};
use crate::registry::{subscribed, subscribed_live};
use crate::util::state_reply;
use crate::{protocol, supervisor, ErrorObj, Handle, PyRuntime, Registry, StreamItem};
use serde_json::{json, Value};
use tauri::ipc::CommandScope;
use tauri::{Emitter, Manager, Runtime};

#[tauri::command]
async fn python_call<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    scope: CommandScope<FnScope>,
    fn_name: String,
    input: Value,
) -> Result<Value, ErrorObj> {
    // Per-function permissions: checked before the window is subscribed or
    // anything reaches the supervisor, so a denial touches no state.
    fn_scope::check(&scope, window.label(), &fn_name)?;
    python_call_impl(&window, &reg, &rt, fn_name, input).await
}

/// `python_call`'s body, callable in tests — the real command path minus
/// Tauri's invoke-arg deserialization and the `FnScope` check. Joins the
/// window to the fairness live set (`subscribed_live`) before dispatch:
/// decisions 0006 §3, amending 0005 §3.
#[doc(hidden)]
pub async fn python_call_impl<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    reg: &Registry,
    rt: &PyRuntime,
    fn_name: String,
    input: Value,
) -> Result<Value, ErrorObj> {
    subscribed_live(window, reg);
    rt.0.call(
        &fn_name,
        input,
        Some(window.label().to_string()),
        Some(false),
    )
    .await
}

/// The `python_call_stream` answer. `protocol` is the client↔plugin
/// bridge version (2 = per-call channel delivery), NOT the host↔Python
/// wire protocol (`protocol::PROTOCOL_VERSION` = 1). It exists so a new
/// client can spot an old plugin: the old command returned a bare task
/// id and ignored `onEvent`, which would hang the call forever — a
/// bare-number answer tells the client to fail loudly instead.
pub(super) const BRIDGE_PROTOCOL: u32 = 2;

#[derive(serde::Serialize)]
pub struct StreamAck {
    pub task: u64,
    pub protocol: u32,
}

/// Start a call with per-call channel delivery. Returns `{task,
/// protocol}`; chunks and the terminal arrive on `on_event` — delivery
/// is bound to the invoking webview, no shared event bus (event-scoping
/// fix).
///
/// `stream` is the client's declared intent: `false` for `call()`
/// (task-slot accounting only — no stream slot, no initial credit), or
/// `true` for `stream()`. Absent (older clients) the task is accounted
/// as a stream exactly as before but NO `stream` field reaches the
/// runtime, so no mismatch validation applies.
///
/// The window's per-function scope is checked first (same rule as
/// `python_call`), before the window is subscribed, a task is allocated,
/// credit is granted or anything is sent on `on_event`. The managed
/// registry and runtime are read from the window instead of being separate
/// parameters, which keeps the command within clippy's argument limit.
#[tauri::command]
async fn python_call_stream<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    scope: CommandScope<FnScope>,
    on_event: tauri::ipc::Channel<Value>,
    fn_name: String,
    input: Value,
    stream: Option<bool>,
) -> Result<StreamAck, ErrorObj> {
    fn_scope::check(&scope, window.label(), &fn_name)?;
    let reg = window.state::<Registry>();
    let rt = window.state::<PyRuntime>();
    python_call_stream_impl(&window, &reg, &rt, on_event, fn_name, input, stream).await
}

/// `python_call_stream`'s body, callable in tests — driving it is the
/// real command path minus Tauri's invoke-arg deserialization.
#[doc(hidden)]
pub async fn python_call_stream_impl<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    reg: &Registry,
    rt: &PyRuntime,
    on_event: tauri::ipc::Channel<Value>,
    fn_name: String,
    input: Value,
    stream: Option<bool>,
) -> Result<StreamAck, ErrorObj> {
    subscribed_live(window, reg);
    let label = window.label().to_string();
    let (task, items) =
        rt.0.call_stream(&fn_name, input, Some(label.clone()), stream)
            .await?;
    forward_stream(on_event, items, rt.0.clone(), task, label);
    Ok(StreamAck {
        task,
        protocol: BRIDGE_PROTOCOL,
    })
}

/// Forward one call's stream items to its per-call channel. A dead
/// consumer — `Channel::send` fails when the webview is gone — reaps
/// that task and stops forwarding, so no stream slot or credit is left
/// tied up. The reap is per-task, not per-label: a webview closed and
/// reopened under the same label can hold newer live calls that a stale
/// forwarder must not kill. Uses `cancel_task`: the awaited send cannot
/// be dropped by a full command queue the way a `try_send` cancel can.
/// Returns the forwarder's join handle (tests await it).
#[doc(hidden)]
pub fn forward_stream(
    on_event: tauri::ipc::Channel<Value>,
    mut items: supervisor::StreamRx,
    rt: Handle,
    task: u64,
    owner: String,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        while let Some(item) = items.recv().await {
            let payload = match item {
                StreamItem::Chunk { seq, value } => {
                    json!({"task": task, "seq": seq, "value": value, "done": false})
                }
                StreamItem::End(Ok(result)) => {
                    json!({"task": task, "done": true, "result": result})
                }
                StreamItem::End(Err(e)) => {
                    json!({"task": task, "done": true, "error": e})
                }
            };
            if on_event.send(payload).is_err() {
                rt.cancel_task(task, owner.clone()).await;
                break;
            }
        }
    })
}

#[tauri::command]
async fn python_grant_credit<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    task: u64,
    n: u64,
) -> Result<(), ErrorObj> {
    subscribed(&window, &reg);
    rt.0.grant_credit(task, n, Some(window.label().to_string()))
        .await
}

/// End of the installed-webview self-test: print the verdict and exit with
/// it. Inert unless PYLAIR_SELF_TEST_WEBVIEW is set, so shipping the
/// permission in a release app grants nothing.
#[tauri::command]
fn python_selftest_report<R: Runtime>(
    app: tauri::AppHandle<R>,
    result: Value,
) -> Result<(), ErrorObj> {
    if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_none() {
        return Err(ErrorObj {
            code: protocol::INVALID_REQUEST,
            message: "webview self-test not enabled".into(),
            data: None,
        });
    }
    let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
    println!("webview-self-test: {result}");
    app.exit(if ok { 0 } else { 5 });
    Ok(())
}

/// Latency bench report (docs/performance.md): the webview bench under
/// PYLAIR_SELF_TEST_BENCH hands its percentile JSON here; the host prints
/// it as one `webview-bench:` line (the same pattern as the self-test
/// verdict, so CI extracts it the same way) and exits 0 — a measurement,
/// never a pass/fail. Inert unless the flag is set.
#[tauri::command]
fn python_bench_report<R: Runtime>(
    app: tauri::AppHandle<R>,
    result: Value,
) -> Result<(), ErrorObj> {
    if std::env::var_os("PYLAIR_SELF_TEST_BENCH").is_none() {
        return Err(ErrorObj {
            code: protocol::INVALID_REQUEST,
            message: "webview bench not enabled".into(),
            data: None,
        });
    }
    println!("webview-bench: {result}");
    app.exit(0);
    Ok(())
}

/// Intruder's verdict for the two-window self-test. win-b cannot send it
/// via JS emit/emit_to: a `core:event:deny-emit(-to)` entry (as the default
/// template ships for the main webview) denies those commands origin-wide —
/// Tauri's authority ignores `windows:` scoping for denials — so the report
/// relays through this ACL-checked command and the Rust side emits. Rust
/// emit paths are not subject to webview permissions. Unlike
/// python_selftest_report this does not exit the app, so win-b can hold it
/// without gaining shutdown power. Inert unless PYLAIR_SELF_TEST_WEBVIEW.
#[tauri::command]
fn python_selftest_b_report<R: Runtime>(
    app: tauri::AppHandle<R>,
    report: Value,
) -> Result<(), ErrorObj> {
    if std::env::var_os("PYLAIR_SELF_TEST_WEBVIEW").is_none() {
        return Err(ErrorObj {
            code: protocol::INVALID_REQUEST,
            message: "webview self-test not enabled".into(),
            data: None,
        });
    }
    app.emit_to("main", "pylair://selftest-b", report)
        .map_err(|e| ErrorObj {
            code: protocol::INTERNAL,
            message: e.to_string(),
            data: None,
        })
}

/// Client `handle.release()` (decision 0010 §4/§6): drop a blob handle
/// and unlink its file. Owner-checked from `window.label()` like every
/// command — a second window cannot release (or even probe) another's
/// blobs: a missing id and a foreign id are both `false`/`denied`… except
/// a foreign id answers PERMISSION_DENIED, matching `python_cancel`'s
/// owner-rejected outcome.
#[tauri::command]
async fn python_release_blob<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    id: String,
) -> Result<bool, ErrorObj> {
    subscribed(&window, &reg);
    rt.0.release_blob(&id, window.label()).await
}

#[tauri::command]
async fn python_cancel<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
    task: u64,
) -> Result<supervisor::CancelOutcome, ErrorObj> {
    subscribed(&window, &reg);
    rt.0.cancel(task, Some(window.label().to_string())).await
}

/// Operator-initiated recovery: respawn the runtime out of the Failed state.
#[tauri::command]
async fn python_retry<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> Result<(), ErrorObj> {
    subscribed(&window, &reg);
    rt.0.retry().await
}

/// The lifecycle state. The bridge kind answers the bare state string, as it
/// always has. A web runtime (wrap mode) answers `{state, url}` instead,
/// where `url` is the validated address the window should show (`null`
/// until the web server is ready; protocol-v1 section 4.1).
#[tauri::command]
fn python_state<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> Value {
    python_state_impl(&window, &reg, &rt)
}

/// `python_state`'s body, callable in tests — the real command path minus
/// Tauri's invoke-arg deserialization. Subscribes the window for event
/// delivery WITHOUT joining the fairness live set: a state-only observer
/// reserves no share (decisions 0006 §3).
#[doc(hidden)]
pub fn python_state_impl<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    reg: &Registry,
    rt: &PyRuntime,
) -> Value {
    subscribed(window, reg);
    state_reply(rt.0.kind(), rt.0.state(), rt.0.ready_url())
}

/// The runtime-health snapshot (docs/decisions/0006 §4): state and
/// time-in-state, restart accounting, the transport/loop liveness
/// verdicts, load totals, and THIS window's own usage/ceilings — never
/// another window's row.
#[tauri::command]
fn python_health<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> Result<Value, ErrorObj> {
    python_health_impl(&window, &reg, &rt)
}

/// `python_health`'s body, callable in tests — the real command path
/// minus Tauri's invoke-arg deserialization. `subscribed()` only, like
/// `python_state`: a health poller reserves no fairness share
/// (decisions 0006 §3). The `self` row is caller-scoped from
/// `window.label()`, never JS input.
#[doc(hidden)]
pub fn python_health_impl<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    reg: &Registry,
    rt: &PyRuntime,
) -> Result<Value, ErrorObj> {
    subscribed(window, reg);
    let mut v = serde_json::to_value(rt.0.health(Some(window.label()))).map_err(|e| ErrorObj {
        code: protocol::INTERNAL,
        message: format!("health snapshot failed to serialize: {e}"),
        data: None,
    })?;
    // Updater-enabled apps additionally carry `update` and an
    // `updating` readiness component (decision 0011 §2) — merged here
    // rather than in HealthSnapshot because the file state is owned by
    // the update gate, not the supervisor.
    if let Some(gate) = window
        .app_handle()
        .try_state::<std::sync::Arc<crate::update::UpdateGate>>()
    {
        v["update"] = gate.health();
        if let Some(state) = gate.component_state() {
            if !v["app"].is_object() {
                v["app"] = json!({"components": {}});
            }
            v["app"]["components"]["updating"] = json!({"phase": state});
        }
    }
    Ok(v)
}

#[tauri::command]
async fn python_shutdown<R: Runtime>(
    window: tauri::WebviewWindow<R>,
    reg: tauri::State<'_, Registry>,
    rt: tauri::State<'_, PyRuntime>,
) -> Result<supervisor::ShutdownOutcome, ErrorObj> {
    subscribed(&window, &reg);
    Ok(rt.0.shutdown().await)
}

pub(super) fn invoke_handler<R: Runtime>(
) -> impl Fn(tauri::ipc::Invoke<R>) -> bool + Send + Sync + 'static {
    #[cfg(feature = "updater")]
    {
        tauri::generate_handler![
            python_call,
            python_call_stream,
            python_grant_credit,
            python_release_blob,
            python_cancel,
            python_retry,
            python_state,
            python_health,
            python_shutdown,
            python_selftest_report,
            python_selftest_b_report,
            python_bench_report,
            crate::update::cmd::python_update_check,
            crate::update::cmd::python_update_download,
            crate::update::cmd::python_update_install,
            crate::update::cmd::python_update_relaunch,
        ]
    }
    #[cfg(not(feature = "updater"))]
    {
        tauri::generate_handler![
            python_call,
            python_call_stream,
            python_grant_credit,
            python_release_blob,
            python_cancel,
            python_retry,
            python_state,
            python_health,
            python_shutdown,
            python_selftest_report,
            python_selftest_b_report,
            python_bench_report,
        ]
    }
}
