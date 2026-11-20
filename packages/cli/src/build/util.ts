/** Small helpers shared by build.ts and the build/ modules. */

import { spawn, spawnSync } from "node:child_process";
import { platform } from "node:os";
import { stripAnsi, type StepHandle, type Ui } from "../ui/index.js";

export const isWin = platform() === "win32";

export function run(cmd: string, args: string[], cwd: string) {
  const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
  if (r.error) throw new Error(`${cmd} ${args.join(" ")} failed to start: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} exited ${r.status}`);
}

/** Most of a tool's output kept for the failure dump (the tail wins: that is
 * where the error is). */
const MAX_LOG_BYTES = 16 * 2 ** 20;

export interface RunToolOptions {
  env?: NodeJS.ProcessEnv;
  /** The step the tool runs under: its last output line becomes the live
   * progress line. */
  step?: StepHandle;
  /** `--verbose`: stream everything, fold nothing. */
  verbose?: boolean;
}

/** Run a tool and wait for it.
 *
 * Plain output or `--verbose`: the tool inherits our stdio and streams
 * exactly as before. Fancy output: the tool's output is captured and shown as
 * the running step's single progress line (on a live terminal), and the whole
 * captured log is printed only if the tool fails. */
export async function runTool(
  ui: Ui,
  cmd: string,
  args: string[],
  cwd: string,
  o: RunToolOptions = {},
): Promise<void> {
  const label = `${cmd} ${args.join(" ")}`;
  const fold = ui.caps.fancy && !o.verbose && o.step !== undefined;
  const child = spawn(cmd, args, {
    cwd,
    env: o.env ?? process.env,
    stdio: fold ? ["ignore", "pipe", "pipe"] : "inherit",
  });

  let log = "";
  if (fold) {
    let pending = "";
    const feed = (b: Buffer) => {
      const text = b.toString("utf-8");
      log += text;
      if (log.length > MAX_LOG_BYTES) log = log.slice(log.length - MAX_LOG_BYTES);
      pending += text;
      const lines = pending.split(/\r\n|\n|\r/);
      pending = lines.pop() ?? "";
      for (const l of lines) {
        const clean = stripAnsi(l).trim();
        if (clean) o.step!.progress(clean);
      }
    };
    child.stdout!.on("data", feed);
    child.stderr!.on("data", feed);
  }

  const code = await new Promise<number | null>((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (c) => resolve(c));
  }).catch((e: Error) => {
    throw new Error(`${label} failed to start: ${e.message}`);
  });

  if (code !== 0) {
    if (fold && log) ui.dump(log.endsWith("\n") ? log : log + "\n");
    throw new Error(`${label} exited ${code}`);
  }
}
