/**
 * Internal plumbing shared by index.ts and selftest.ts: the plugin command
 * names, the injectable IPC bridge and the live-task set.
 *
 * NOT public API. This module is not listed in package.json "exports", so
 * consumers cannot import it. `setBridge` and `_resetForTests` reach users
 * through re-exports from the package root (index.ts).
 */

import type { IpcBridge, StreamEvent } from "./index.js";

export const CMD = {
  selftestReport: "plugin:pylair|python_selftest_report",
  call: "plugin:pylair|python_call",
  callStream: "plugin:pylair|python_call_stream",
  grantCredit: "plugin:pylair|python_grant_credit",
  cancel: "plugin:pylair|python_cancel",
  retry: "plugin:pylair|python_retry",
  state: "plugin:pylair|python_state",
  shutdown: "plugin:pylair|python_shutdown",
} as const;

// ---- bridge management -----------------------------------------------------

let _bridge: IpcBridge | null = null;
let _defaultBridge: Promise<IpcBridge> | null = null;

function defaultBridge(): Promise<IpcBridge> {
  _defaultBridge ??= Promise.all([
    import("@tauri-apps/api/core"),
    import("@tauri-apps/api/event"),
    import("@tauri-apps/api/webview"),
  ]).then(([core, event, webviewApi]): IpcBridge => {
    // Label-scoped listeners: an `Any`-target handler receives every event
    // regardless of the emit destination (Tauri's match_any_or_filter), so
    // a stock window on plain `listen` would see stderr addressed to other
    // windows. Webview-scoped handlers are filtered by `emit_to` — only a
    // deliberate Any-listen still sees other windows' events.
    let webviewListen: IpcBridge["listen"] | null = null;
    try {
      const wv = webviewApi.getCurrentWebview();
      webviewListen = <T,>(ev: string, cb: (p: T) => void) =>
        wv.listen<T>(ev, (e: { payload: T }) => cb(e.payload));
    } catch {
      // No webview context (tests/headless) — fall back to plain listen.
    }
    const listen =
      webviewListen ??
      (<T,>(ev: string, cb: (p: T) => void) =>
        event.listen<T>(ev, (e: { payload: T }) => cb(e.payload)));
    return {
      invoke: (cmd, args) => core.invoke(cmd, args),
      listen,
      invokeChannel: (cmd, args, onEvent) => {
        // onmessage is wired BEFORE invoke — the first chunk can race
        // the invoke response and must not be lost.
        const channel = new core.Channel<StreamEvent>();
        channel.onmessage = onEvent;
        return core.invoke(cmd, { ...args, onEvent: channel });
      },
    };
  });
  return _defaultBridge;
}

/** Inject a custom bridge (tests, non-webview hosts). */
export function setBridge(bridge: IpcBridge | null): void {
  _bridge = bridge;
}

export async function ipc(): Promise<IpcBridge> {
  return _bridge ?? defaultBridge();
}

/**
 * Live task ids in THIS window — not for routing (channels do that), only
 * so a page unload can cancel them. A reload kills every channel but
 * `Channel::send` does NOT reliably error on a reload (the webview object
 * survives; the eval lands in a dead JS context and the callback is a
 * no-op). The plugin's on_page_load hook already reaps the document's
 * tasks server-side when a navigation commits; this hook stays as a best
 * effort belt for engines that never surface a load event.
 */
export const liveTasks = new Set<number>();

/** For tests: reset the injected bridge between cases. */
export function _resetForTests(): void {
  liveTasks.clear();
  _bridge = null;
  _defaultBridge = null;
}
