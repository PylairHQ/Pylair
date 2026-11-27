/** Shared dev watcher for `pylair dev` (bridge and wrap): one fs.watch
 * loop, glob-configurable include/ignore over `[tool.pylair.watch]`,
 * 150ms debounce, and the `<millis> <file>` restart-trigger format the
 * supervisor and inspector already speak. */

import { existsSync, readFileSync, watch, writeFileSync } from "node:fs";
import { join } from "node:path";
import { matchGlob } from "./glob.js";

/** Always-on ignores: tool venvs, caches, VCS, editor temp files, and
 * pylair's own state dir (the trigger write itself must not retrigger). */
export const BUILTIN_IGNORES = [
  "**/.venv/**",
  "**/venv/**",
  "**/__pycache__/**",
  "**/node_modules/**",
  "**/.git/**",
  "**/.#*",
  "**/*~",
  "**/*.swp",
  "**/*.tmp",
  "**/4913",
  "**/.pylair/**",
];

export interface WatchSet {
  include: string[];
  ignore: string[];
}

export const DEFAULT_INCLUDE = ["**/*.py"];

/** One fs.watch event, one decision: is this relative path (forward
 * slashes, relative to the watched source root) a reload candidate? */
export function watchMatch(rel: string, set: WatchSet): boolean {
  const path = rel.replace(/\\/g, "/");
  if (!set.include.some((g) => matchGlob(g, path))) return false;
  if (BUILTIN_IGNORES.some((g) => ignoreMatch(g, path))) return false;
  if (set.ignore.some((g) => ignoreMatch(g, path))) return false;
  return true;
}

/** Ignore semantics: a glob matches the path itself or any ancestor
 * directory of it - the venv ignore must cover everything below a venv
 * no matter how deep. Ancestors are tested with their trailing slash,
 * which is the form the glob compiler's segment semantics expect. */
function ignoreMatch(glob: string, path: string): boolean {
  if (matchGlob(glob, path)) return true;
  let i = path.indexOf("/");
  while (i !== -1) {
    if (matchGlob(glob, path.slice(0, i + 1))) return true;
    i = path.indexOf("/", i + 1);
  }
  return false;
}
/** The trigger content for a source change - `<millis> <file>`. */
export function triggerContent(file: string): string {
  return `${Date.now()} ${file}`;
}

/** The trigger content for a manual restart (`pylair restart`, `r` key):
 * the `manual` marker in place of the file name - the supervisor resets
 * the crash-loop breaker on it and the inspector says "manual restart". */
export function manualTriggerContent(): string {
  return `${Date.now()} manual`;
}

export function writeManualTrigger(trigger: string): string {
  const content = manualTriggerContent();
  writeFileSync(trigger, content);
  return content;
}

/** Watch `srcDir` recursively; on a matching change, debounce 150ms and
 * write the trigger, then fire the callback once per trigger. Returns a
 * close function. */
export function startDevWatch(opts: {
  srcDir: string;
  trigger: string;
  set: WatchSet;
  onTrigger: (file: string) => void;
  /** Fired with the exact trigger content after each own write, so the
   * external-trigger watcher can tell self from `pylair restart`. */
  onWrite?: (content: string) => void;
  debounceMs?: number;
}): { close: () => void } {
  const debounce = opts.debounceMs ?? 150;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let changed: string | null = null;
  const watcher = watch(opts.srcDir, { recursive: true }, (_ev, file) => {
    if (!file) return;
    const rel = file.replace(/\\/g, "/");
    if (!watchMatch(rel, opts.set)) return;
    changed = rel;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      if (!changed) return;
      const f = changed;
      changed = null;
      const content = triggerContent(f);
      writeFileSync(opts.trigger, content);
      opts.onWrite?.(content);
      opts.onTrigger(f);
    }, debounce);
  });
  return {
    close: () => {
      if (timer) clearTimeout(timer);
      watcher.close();
    },
  };
}

/** Poll a JSON status file (runtime-status.json) for state changes.
 * cb receives the parsed object; interval poll like the supervisor's own
 * trigger poll - no fs-watch aliasing puzzles on Windows. */
export function pollStatusFile(
  path: string,
  cb: (status: { generation?: number; state?: string; exit_code?: number | null; detail?: string | null; ts?: number }) => void,
  intervalMs = 200,
): { close: () => void } {
  let lastKey = "";
  const timer = setInterval(() => {
    if (!existsSync(path)) return;
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(readFileSync(path, "utf-8"));
    } catch {
      return; // mid-write - the next poll re-reads
    }
    const key = `${parsed.generation}:${parsed.state}:${parsed.ts}`;
    if (key === lastKey) return;
    lastKey = key;
    cb(parsed as never);
  }, intervalMs);
  return { close: () => clearInterval(timer) };
}

/** Liveness check for `pylair restart`: the dev pid file exists and the
 * process is alive. */
export function devSessionAlive(appDir: string): { alive: boolean; pid?: number } {
  const pidPath = join(appDir, ".pylair", "dev.pid");
  if (!existsSync(pidPath)) return { alive: false };
  const pid = Number(readFileSync(pidPath, "utf-8").trim());
  if (!Number.isInteger(pid) || pid <= 0) return { alive: false };
  try {
    process.kill(pid, 0);
    return { alive: true, pid };
  } catch {
    return { alive: false };
  }
}
