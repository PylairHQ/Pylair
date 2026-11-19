/** The one output module every `pylair` command prints through.
 *
 * Two renderings, chosen once at startup (see caps.ts):
 *
 * - fancy: colour only for meaning, consistent symbols, aligned columns, a
 *   spinner on the step that is running and a timing on each finished step;
 * - plain: stable text with no ANSI codes and no spinner frames. Wherever
 *   something parses today's output (the doctor check lines, the "pylair
 *   <command>: message" errors), plain keeps today's wording.
 *
 * Nothing here prints an em dash or any other non-ASCII glyph unless the
 * terminal was shown to handle it, so it cannot come out as mojibake.
 */

import type { Caps } from "./caps.js";
import { detectProcessCaps } from "./caps.js";
import { formatDuration, paint, stripAnsi, symbolsFor, truncate, type Symbols } from "./style.js";

export { detectCaps, detectProcessCaps } from "./caps.js";
export type { Caps, CapsInput } from "./caps.js";
export { formatDuration, formatSize, paint, stripAnsi, symbolsFor, truncate } from "./style.js";

export interface Sink {
  out: (s: string) => void;
  err: (s: string) => void;
}

export interface Clock {
  now: () => number;
  setInterval: (fn: () => void, ms: number) => unknown;
  clearInterval: (h: unknown) => void;
}

const realClock: Clock = {
  now: () => Date.now(),
  setInterval: (fn, ms) => {
    const t = setInterval(fn, ms);
    t.unref?.();
    return t;
  },
  clearInterval: (h) => clearInterval(h as NodeJS.Timeout),
};

export interface StepHandle {
  /** Short result shown after the step finishes ("3.12.16, cached"). */
  detail(text: string): void;
  /** Live detail for the running line (the last line of a tool's output). */
  progress(text: string): void;
}

export interface Hint {
  file?: string;
  fix?: string;
}

const CLEAR_LINE = "\r\u001b[2K";
const HIDE_CURSOR = "\u001b[?25l";
const SHOW_CURSOR = "\u001b[?25h";

interface Active {
  label: string;
  detail: string;
  frame: number;
  timer?: unknown;
}

export class Ui {
  readonly sym: Symbols;
  private active?: Active;
  private labelWidth = 0;
  private stepHandle?: StepHandle;

  constructor(
    readonly caps: Caps,
    private readonly sink: Sink,
    private readonly clock: Clock = realClock,
  ) {
    this.sym = symbolsFor(caps);
  }

  private p(role: Parameters<typeof paint>[1], text: string): string {
    return paint(this.caps, role, text);
  }

  /** A status glyph padded to a fixed width, so columns after it line up in
   * both the Unicode (1 wide) and ASCII (`[ok]`, 4 wide) sets. */
  private mark(role: "ok" | "fail" | "warn", glyph: string): string {
    return this.p(role, glyph.padEnd(this.caps.unicode ? 1 : 4));
  }

  // ---- low level: never corrupt the live spinner line -----------------------

  private clearActive(): void {
    if (this.active && this.caps.animate) this.sink.out(CLEAR_LINE);
  }

  private drawActive(): void {
    const a = this.active;
    if (!a || !this.caps.animate) return;
    const frame = this.sym.frames[a.frame % this.sym.frames.length];
    const head = `${frame} ${a.label}`;
    const room = Math.max(0, this.caps.columns - head.length - 3);
    const detail = a.detail ? `  ${this.p("muted", truncate(stripAnsi(a.detail), room, this.sym.ellipsis))}` : "";
    this.sink.out(`${CLEAR_LINE}${this.p("accent", frame)} ${a.label}${detail}`);
  }

  private emit(channel: "out" | "err", text: string): void {
    this.clearActive();
    this.sink[channel](text);
    this.drawActive();
  }

  // ---- lines -------------------------------------------------------------------

  /** Raw text to stderr, above the live line (a failed tool's captured log). */
  dump(text: string): void {
    this.emit("err", text);
  }

  /** A line as is (stdout). */
  line(text = ""): void {
    this.emit("out", text + "\n");
  }

  /** A fact: `ok text` in fancy mode, just the text in plain. */
  ok(text: string): void {
    this.line(this.caps.fancy ? `${this.p("ok", this.sym.ok)} ${text}` : text);
  }

  /** Neutral information. */
  info(text: string): void {
    this.line(this.caps.fancy ? `${this.p("accent", this.sym.bullet)} ${text}` : text);
  }

  /** Detail, de-emphasised. */
  note(text: string): void {
    this.line(this.p("muted", text));
  }

  /** A warning (stderr). A leading "warning: " is dropped in fancy mode (the
   * symbol says it) and kept in plain mode, as it always was. `bare` marks a
   * message that never carried the prefix (a retry notice): plain prints it as is. */
  warn(text: string, opts: { bare?: boolean; plain?: string } = {}): void {
    const body = text.replace(/^warn(ing)?:\s*/i, "");
    const plain = opts.plain ?? (opts.bare ? text : `warning: ${body}`);
    this.emit(
      "err",
      (this.caps.fancy ? `${this.p("warn", this.sym.warn)} ${this.p("warn", body)}` : plain) + "\n",
    );
  }

  /** A status note from deep inside a step (a download source, a cache hit).
   * Fancy: it becomes the running step's detail, so the step list stays one
   * line per stage. Plain, or outside a step: a line of its own. */
  status(text: string): void {
    if (this.caps.fancy && this.stepHandle) this.stepHandle.detail(text);
    else this.line(text);
  }

  /** A failure (stderr). Plain mode prints `text` exactly (callers pass the
   * `pylair <command>: message` form the tests and scripts see today). */
  error(text: string): void {
    this.emit("err", (this.caps.fancy ? `${this.p("fail", this.sym.fail)} ${this.p("fail", text)}` : text) + "\n");
  }

  /** The final line of a command. */
  summary(kind: "ok" | "fail" | "warn", text: string): void {
    if (!this.caps.fancy) return this.line(text);
    const sym = kind === "ok" ? this.sym.ok : kind === "fail" ? this.sym.fail : this.sym.warn;
    this.line(`${this.p(kind, sym)} ${this.p("bold", text)}`);
  }

  /** Aligned `key  value` rows. */
  rows(pairs: ReadonlyArray<readonly [string, string]>, indent = 2): void {
    const w = Math.max(0, ...pairs.map(([k]) => k.length));
    for (const [k, v] of pairs)
      this.line(`${" ".repeat(indent)}${this.p("muted", k.padEnd(w))}  ${v}`);
  }

  /** A command header. Fancy: a bold title and aligned rows. Plain: the
   * legacy lines the command printed before (kept for anything that reads them). */
  header(title: string, pairs: ReadonlyArray<readonly [string, string]>, plainLines: readonly string[]): void {
    if (!this.caps.fancy) {
      for (const l of plainLines) this.line(l);
      return;
    }
    this.line(this.p("bold", title));
    this.rows(pairs);
    this.line();
  }

  /** One doctor check. Plain keeps the exact legacy line (` ok ` / `FAIL`,
   * name padded, two spaces, detail); fancy aligns a symbol, the name and the
   * detail. A failure gets its file and a one-line fix underneath in both. */
  check(ok: boolean, name: string, nameWidth: number, detail: string, hint?: Hint): void {
    if (!this.caps.fancy) {
      this.line(`${ok ? " ok " : "FAIL"} ${name.padEnd(nameWidth)}  ${detail}`);
    } else {
      const sym = ok ? this.mark("ok", this.sym.ok) : this.mark("fail", this.sym.fail);
      const lines = detail.split("\n");
      this.line(`  ${sym} ${name.padEnd(nameWidth)}  ${this.p("muted", lines[0])}`);
      for (const extra of lines.slice(1))
        this.line(`    ${" ".repeat(nameWidth + 2)}${this.p("muted", extra.trim())}`);
    }
    if (!ok && hint) {
      const pad = this.caps.fancy ? "      " : "     ";
      const arrow = this.sym.arrow;
      if (hint.file) this.line(`${pad}${this.p("muted", `${arrow} file`)}  ${this.p("accent", hint.file)}`);
      if (hint.fix) this.line(`${pad}${this.p("muted", `${arrow} fix`)}   ${hint.fix}`);
    }
  }

  // ---- steps ---------------------------------------------------------------------

  /** Align the finished step lines on the longest label. */
  setStepWidth(w: number): void {
    this.labelWidth = w;
  }

  /** Run one stage of work with a live spinner (when the terminal can show
   * one) and a timing line when it ends. Rethrows what `fn` throws, after
   * marking the step failed. */
  async step<T>(label: string, fn: (s: StepHandle) => T | Promise<T>): Promise<T> {
    const start = this.clock.now();
    let detail = "";
    const handle: StepHandle = {
      detail: (t) => {
        detail = t;
      },
      progress: (t) => {
        if (this.active) this.active.detail = t;
      },
    };
    this.stepHandle = handle;
    if (this.caps.animate) {
      this.active = { label, detail: "", frame: 0 };
      this.sink.out(HIDE_CURSOR);
      this.drawActive();
      this.active.timer = this.clock.setInterval(() => {
        if (!this.active) return;
        this.active.frame++;
        this.drawActive();
      }, 80);
    } else if (!this.caps.fancy) {
      this.line(`-> ${label}`);
    }
    const finish = (ok: boolean): void => {
      this.stepHandle = undefined;
      if (this.active) {
        this.clock.clearInterval(this.active.timer);
        this.sink.out(CLEAR_LINE + SHOW_CURSOR);
        this.active = undefined;
      }
      const time = formatDuration(this.clock.now() - start);
      if (!this.caps.fancy) {
        this.line(`${ok ? "[ok]" : "[x]"} ${label} (${time})${detail ? `: ${detail}` : ""}`);
        return;
      }
      const sym = ok ? this.mark("ok", this.sym.ok) : this.mark("fail", this.sym.fail);
      const w = Math.max(this.labelWidth, label.length);
      const used = (this.caps.unicode ? 1 : 4) + 1 + w + 2 + 7 + 2; // symbol, space, label, gap, time, gap
      const shown = detail ? truncate(detail, Math.max(12, this.caps.columns - used - 1), this.sym.ellipsis) : "";
      this.line(
        `${sym} ${label.padEnd(w)}  ${this.p("muted", time.padStart(7))}${shown ? `  ${this.p("muted", shown)}` : ""}`,
      );
    };
    try {
      const r = await fn(handle);
      finish(true);
      return r;
    } catch (e) {
      finish(false);
      throw e;
    }
  }

  /** Restore the cursor if the process is about to exit mid-spinner. */
  dispose(): void {
    if (this.active) {
      this.clock.clearInterval(this.active.timer);
      this.sink.out(CLEAR_LINE + SHOW_CURSOR);
      this.active = undefined;
    }
  }
}

// ---- the process-wide instance ------------------------------------------------------

let current: Ui | undefined;

/** Decide capabilities once and install the process-wide Ui. Called first
 * thing by the CLI entry; commands then print through `ui`. */
export function initUi(argv: readonly string[] = process.argv.slice(2)): Ui {
  current?.dispose();
  current = new Ui(detectProcessCaps(argv), {
    out: (s) => void process.stdout.write(s),
    err: (s) => void process.stderr.write(s),
  });
  const u = current;
  process.once("exit", () => u.dispose());
  return current;
}

/** Install a Ui built elsewhere (tests). */
export function setUi(u: Ui): void {
  current = u;
}

export function getUi(): Ui {
  return current ?? initUi();
}

/** `ui.ok(...)`, `ui.step(...)`: the process-wide instance, created on first use. */
export const ui: Ui = new Proxy({} as Ui, {
  get(_t, key: string) {
    const u = getUi();
    const v = (u as unknown as Record<string, unknown>)[key];
    return typeof v === "function" ? v.bind(u) : v;
  },
});
