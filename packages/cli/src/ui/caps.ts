/** What the terminal can show, decided ONCE at startup.
 *
 * Two independent questions:
 *
 * - fancy or plain? Fancy is colour, symbols, a spinner and folded tool
 *   output; plain is stable, readable text with no ANSI codes and no spinner
 *   frames (logs, CI, pipes, `--plain`).
 * - unicode or ascii symbols? Only asked when fancy. A glyph the console
 *   cannot show comes out as mojibake (a UTF-8 em dash read as code page 437
 *   prints "ΓÇö"), so on Windows unicode is used only where it is known to
 *   work.
 *
 * Pure: everything it needs comes in as arguments, so it is unit-tested for
 * every TTY / CI / NO_COLOR / FORCE_COLOR / Windows-console case.
 */

import { execFileSync } from "node:child_process";

export interface Caps {
  /** Colour, symbols, spinner, folded tool output. False means plain text. */
  fancy: boolean;
  /** Emit ANSI colour codes (equal to `fancy`). */
  color: boolean;
  /** Use Unicode symbols (otherwise ASCII). Always false when plain. */
  unicode: boolean;
  /** Redraw lines in place (a live spinner): fancy AND an interactive TTY. */
  animate: boolean;
  /** Terminal width, for truncating folded progress lines. */
  columns: number;
}

export interface CapsInput {
  env: Record<string, string | undefined>;
  platform: string;
  stdoutIsTTY: boolean;
  /** Command-line arguments (for `--plain`). */
  argv: readonly string[];
  /** Windows only: the console's output code page, asked lazily and at most
   * once, and only when the answer matters. */
  codePage?: () => number | undefined;
  columns?: number;
}

/** FORCE_COLOR: "0" and "false" force colour OFF, any other non-empty value
 * forces it ON, unset or empty leaves the decision to the terminal. */
function forceColor(env: CapsInput["env"]): "on" | "off" | undefined {
  const v = env.FORCE_COLOR;
  if (v === undefined || v === "") return undefined;
  return v === "0" || v.toLowerCase() === "false" ? "off" : "on";
}

function localeIsUtf8(env: CapsInput["env"]): boolean {
  const loc = env.LC_ALL || env.LC_CTYPE || env.LANG || "";
  return /utf-?8/i.test(loc);
}

export function detectCaps(i: CapsInput): Caps {
  const { env } = i;
  const force = forceColor(env);
  const auto =
    i.stdoutIsTTY && !env.CI && !env.NO_COLOR && env.TERM !== "dumb";
  // Precedence: --plain, then FORCE_COLOR (off, then on), then the terminal.
  const fancy = !i.argv.includes("--plain") && force !== "off" && (force === "on" || auto);
  const animate = fancy && i.stdoutIsTTY && env.TERM !== "dumb";

  let unicode = false;
  if (fancy) {
    if (i.platform === "win32") {
      // Windows Terminal renders Unicode whatever the code page says; the
      // classic console (PowerShell 5.1, cmd) only when it was switched to
      // UTF-8 (code page 65001).
      unicode = !!env.WT_SESSION || i.codePage?.() === 65001;
    } else if (env.TERM === "linux") {
      unicode = false; // the bare Linux virtual console
    } else {
      unicode = localeIsUtf8(env) || i.platform === "darwin";
    }
  }
  return { fancy, color: fancy, unicode, animate, columns: i.columns && i.columns > 0 ? i.columns : 80 };
}

/** Windows: the active console code page, from `chcp`. Undefined when it
 * cannot be read (treated as "not UTF-8": ASCII is always safe). */
export function windowsCodePage(): number | undefined {
  try {
    const out = execFileSync("cmd.exe", ["/d", "/c", "chcp"], {
      encoding: "utf8",
      timeout: 2000,
      windowsHide: true,
      stdio: ["ignore", "pipe", "ignore"],
    });
    const m = /(\d{3,5})\s*$/.exec(out.trim());
    return m ? Number(m[1]) : undefined;
  } catch {
    return undefined;
  }
}

/** The real thing, from the running process. */
export function detectProcessCaps(argv: readonly string[] = process.argv.slice(2)): Caps {
  return detectCaps({
    env: process.env,
    platform: process.platform,
    stdoutIsTTY: !!process.stdout.isTTY,
    argv,
    codePage: windowsCodePage,
    columns: process.stdout.columns,
  });
}
