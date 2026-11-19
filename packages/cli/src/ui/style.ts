/** Colour and glyphs. Colour carries meaning only: green ok, red failure,
 * yellow warning, cyan for commands and paths, grey for detail. */

import type { Caps } from "./caps.js";

export type Role = "ok" | "fail" | "warn" | "accent" | "muted" | "bold";

const CODES: Record<Role, [number, number]> = {
  ok: [32, 39],
  fail: [31, 39],
  warn: [33, 39],
  accent: [36, 39],
  muted: [90, 39],
  bold: [1, 22],
};

/** Wrap `text` in the colour for `role`, or return it untouched when colour
 * is off. Safe to nest: each role closes with its own reset. */
export function paint(caps: Caps, role: Role, text: string): string {
  if (!caps.color || text === "") return text;
  const [open, close] = CODES[role];
  return `\u001b[${open}m${text}\u001b[${close}m`;
}

/** Strip ANSI escape sequences (colour, cursor movement). */
export function stripAnsi(s: string): string {
  // eslint-disable-next-line no-control-regex
  return s.replace(/\u001b\[[0-9;?]*[ -/]*[@-~]/g, "");
}

/** Truncate to `max` visible columns with an ellipsis glyph. */
export function truncate(s: string, max: number, ellipsis: string): string {
  if (max <= ellipsis.length) return s.slice(0, Math.max(0, max));
  return s.length <= max ? s : s.slice(0, max - ellipsis.length) + ellipsis;
}

export interface Symbols {
  ok: string;
  fail: string;
  warn: string;
  skip: string;
  bullet: string;
  arrow: string;
  ellipsis: string;
  frames: readonly string[];
}

const UNICODE: Symbols = {
  ok: "✔", // heavy check mark
  fail: "✖", // heavy multiplication x
  warn: "▲", // black up-pointing triangle
  skip: "○", // white circle
  bullet: "•",
  arrow: "›", // single right-pointing angle quotation mark
  ellipsis: "…",
  frames: ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
};

const ASCII: Symbols = {
  ok: "[ok]",
  fail: "[x]",
  warn: "[!]",
  skip: "-",
  bullet: "-",
  arrow: ">",
  ellipsis: "...",
  frames: ["-", "\\", "|", "/"],
};

export function symbolsFor(caps: Caps): Symbols {
  return caps.unicode ? UNICODE : ASCII;
}

/** 1234 -> "1.2 KiB". */
export function formatSize(bytes: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let n = bytes;
  let u = 0;
  while (n >= 1024 && u < units.length - 1) {
    n /= 1024;
    u++;
  }
  return u === 0 ? `${n} B` : `${n.toFixed(n >= 100 ? 0 : 1)} ${units[u]}`;
}

/** 450 -> "0.5s", 2100 -> "2.1s", 125000 -> "2m 05s". */
export function formatDuration(ms: number): string {
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const s = Math.round(ms / 1000);
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
}
