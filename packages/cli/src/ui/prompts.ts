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
 * control keys are ignored. `onEnter` inspects the buffered line: return
 * the answer to finish, or a message to reject and keep asking. The whole
 * retry stays inside one keypress loop so no input is lost between tries. */
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
      const bad = onEnter(buf.trim());
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

/** Free-text question. `validate` returns an error message to show (then the
 * question repeats) or undefined to accept. Empty answers repeat. */
export async function askText(
  io: PromptIo,
  label: string,
  validate?: (value: string) => string | undefined,
): Promise<string> {
  let answer = "";
  await readLineRetry(io, () => io.output.write(`${label}: `), (v) => {
    const bad = v ? validate?.(v) : "a value is required";
    if (bad === undefined) {
      answer = v;
      return undefined;
    }
    return bad;
  });
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
    let chosen: T | undefined;
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
          chosen = options[n - 1].value;
          return undefined;
        }
        return `type a number between 1 and ${options.length}`;
      },
    );
    return chosen as T;
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
  write(`${syms.ok} ${label}: ${paint(io.caps, "muted", options[idx].label)}\n`);
  return options[idx].value;
}
