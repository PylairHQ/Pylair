/** Interactive prompts, hand-rolled on node:readline keypress events so the
 * packages sharing this file (the CLI directly, create-pylair vendored) stay
 * dependency-free.
 *
 * Callers gate on interactive(): prompts never fire without a TTY or under
 * CI. Ctrl+C cancels through PromptCancelled; the caller catches it, prints
 * a short line and exits before writing any files.
 */

import { emitKeypressEvents, type Key } from "node:readline";
import type { Caps } from "./caps.js";
import { paint, symbolsFor } from "./style.js";

/** The readable side of a terminal. `setRawMode` exists on a real TTY; fake
 * streams (tests) omit it and the prompts still work byte for byte. */
export interface PromptInput extends NodeJS.ReadableStream {
  isTTY?: boolean;
  setRawMode?(mode: boolean): unknown;
}

export interface PromptIo {
  input: PromptInput;
  output: NodeJS.WritableStream;
  caps: Caps;
}

/** Prompts are for a person at a keyboard: interactive stdin, never CI. */
export function interactive(input: { isTTY?: boolean }, env: NodeJS.ProcessEnv): boolean {
  return Boolean(input.isTTY) && !env.CI;
}

/** Ctrl+C at a prompt. Callers treat it as "clean exit, nothing written". */
export class PromptCancelled extends Error {
  constructor() {
    super("cancelled");
    this.name = "PromptCancelled";
  }
}

/** While a prompt owns stdin, ^C should cancel the prompt, not kill the
 * process mid-question. In raw mode ^C arrives as a keypress; in cooked mode
 * it may reach the process as SIGINT. Cover both until the prompt ends. */
function onProcessSigint(fn: () => void): () => void {
  process.on("SIGINT", fn);
  return () => process.removeListener("SIGINT", fn);
}

/** Streams that already keep the keypress decoder alive. emitKeypressEvents
 * attaches its 'data' decoder lazily on the first 'keypress' listener and
 * detaches it when the count returns to zero, without re-arming, so a
 * second prompt on the same input would receive nothing. One permanent
 * listener per stream keeps the decoder attached for the run's lifetime. */
const DECODER_KEEPALIVE = new WeakSet<PromptInput>();

/** Run `onKey` for each keypress until it returns true (done) or the user
 * cancels. `start` renders the prompt: it runs AFTER the listener attaches,
 * so input that arrives while the prompt draws still lands on it. Raw mode
 * is entered when the stream supports it and always left again. Rejects
 * with PromptCancelled on ^C. */
function keypressLoop(
  io: PromptIo,
  onKey: (str: string | undefined, key: Key) => boolean | void,
  start?: () => void,
): Promise<void> {
  return new Promise((resolve, reject) => {
    function finish() {
      done();
      resolve();
    }
    function cancel() {
      done();
      reject(new PromptCancelled());
    }
    function listener(str: string | undefined, key: Key) {
      if (key?.ctrl && key.name === "c") return cancel();
      if (onKey(str, key) === true) finish();
    }
    function done() {
      io.input.removeListener("keypress", listener);
      stopSigint();
      if (raw) io.input.setRawMode!(false);
    }
    emitKeypressEvents(io.input);
    if (!DECODER_KEEPALIVE.has(io.input)) {
      DECODER_KEEPALIVE.add(io.input);
      io.input.on("keypress", () => {});
    }
    const raw = typeof io.input.setRawMode === "function";
    if (raw) io.input.setRawMode!(true);
    const stopSigint = onProcessSigint(cancel);
    io.input.on("keypress", listener);
    start?.();
  });
}

/** Echo printable input and return it on Enter. Backspace edits; other
 * control keys are ignored. `onEnter` inspects the buffered line exactly as
 * echoed: return the answer to finish, or a message to reject and keep
 * asking. The whole retry stays inside one keypress loop so no input is
 * lost between tries. */
async function readLineRetry(
  io: PromptIo,
  start: () => void,
  onEnter: (line: string) => string | undefined,
): Promise<string> {
  const write = (s: string) => io.output.write(s);
  let buf = "";
  await keypressLoop(io, (str, key) => {
    // \r -> "return" (raw TTY), \n -> "enter" (cooked or piped input)
    if (key.name === "return" || key.name === "enter") {
      const bad = onEnter(buf);
      if (bad === undefined) {
        write("\n");
        return true;
      }
      buf = "";
      write(`\n${bad}\n`);
      start();
      return;
    }
    if (key.name === "backspace") {
      if (buf) {
        buf = buf.slice(0, -1);
        write("\b \b");
      }
      return;
    }
    if (key.ctrl || key.meta) return;
    if (str && str >= " ") {
      buf += str;
      write(str);
    }
  }, start);
  return buf.trim();
}

/** `label: ` for bare labels, `label? ` when the label already ends in
 * question punctuation, so a question mark never gains a colon (`label?:`). */
function questionSuffix(label: string): string {
  return /[?!.:]$/.test(label) ? " " : ": ";
}

/** Every prompt resolves the same way: the ok mark, the question and the
 * answer, styled like a completed step. */
function resolved(io: PromptIo, label: string, answer: string): void {
  io.output.write(
    `${symbolsFor(io.caps).ok} ${label}${questionSuffix(label)}${paint(io.caps, "muted", answer)}\n`,
  );
}

/** Free-text question. `validate` returns an error message to show (then the
 * question repeats) or undefined to accept. Empty answers repeat. */
export async function askText(
  io: PromptIo,
  label: string,
  validate?: (value: string) => string | undefined,
): Promise<string> {
  const prefix = `${label}${questionSuffix(label)}`;
  let answer = "";
  // What each attempt echoed (failed ones included), so the prompt region
  // can be erased exactly once the answer is accepted.
  const attempts: { typed: string; error: string }[] = [];
  await readLineRetry(
    io,
    () => io.output.write(prefix),
    (raw) => {
      const v = raw.trim();
      const bad = v ? validate?.(v) : "a value is required";
      if (bad === undefined) {
        answer = v;
        return undefined;
      }
      attempts.push({ typed: raw, error: bad });
      return bad;
    },
  );
  // Fancy consoles erase the prompt region (last prompt line and any prior
  // attempt and error lines) so the question appears once: the resolved
  // line. Plain consoles cannot move the cursor, and the typed answer is
  // already on screen, so they show no resolved line at all.
  if (io.caps.fancy) {
    const cols = (io.output as { columns?: number }).columns ?? 0;
    const rowsOf = (text: string) => (cols > 0 ? Math.max(1, Math.ceil(text.length / cols)) : 1);
    let rows = rowsOf(prefix + answer);
    for (const a of attempts) rows += rowsOf(a.error) + rowsOf(prefix + a.typed);
    io.output.write(`\x1b[${rows}A\x1b[0J`);
    resolved(io, label, answer);
  }
  return answer;
}

export interface SelectOption<T> {
  value: T;
  label: string;
}

/** Pick one of `options`. Fancy consoles get an arrow-key menu; plain ones
 * get a numbered list answered by typing a digit. */
export async function askSelect<T>(io: PromptIo, label: string, options: SelectOption<T>[]): Promise<T> {
  if (options.length === 0) throw new Error("askSelect needs at least one option");
  if (!io.caps.fancy) {
    const list = options.map((o, i) => `  ${i + 1}) ${o.label}`).join("\n");
    let first = true;
    let chosen: SelectOption<T> | undefined;
    await readLineRetry(
      io,
      () => {
        if (first) {
          io.output.write(`${label}\n${list}\n`);
          first = false;
        }
        io.output.write(`choose 1-${options.length}: `);
      },
      (v) => {
        const n = Number(v);
        if (Number.isInteger(n) && n >= 1 && n <= options.length) {
          chosen = options[n - 1];
          return undefined;
        }
        return `type a number between 1 and ${options.length}`;
      },
    );
    resolved(io, label, (chosen as SelectOption<T>).label);
    return (chosen as SelectOption<T>).value;
  }

  const syms = symbolsFor(io.caps);
  const write = (s: string) => io.output.write(s);
  let idx = 0;
  const menu = () =>
    options.map((o, i) => (i === idx ? `  ${paint(io.caps, "accent", syms.arrow)} ${o.label}` : `    ${o.label}`)).join("\n");
  try {
    await keypressLoop(
      io,
      (_str, key) => {
        if (key.name === "up") idx = (idx - 1 + options.length) % options.length;
        else if (key.name === "down") idx = (idx + 1) % options.length;
        else if (key.name === "return" || key.name === "enter") return true;
        else return;
        write(`\x1b[${options.length}A\x1b[0J${menu()}\n`);
      },
      () => {
        write(`${paint(io.caps, "accent", syms.arrow)} ${label}\n${menu()}\n`);
        write("\x1b[?25l"); // hide the cursor while the menu is live
      },
    );
  } finally {
    write("\x1b[?25h");
  }
  // Collapse the menu into a resolved line, like a completed step.
  write(`\x1b[${options.length + 1}A\x1b[0J`);
  resolved(io, label, options[idx].label);
  return options[idx].value;
}
