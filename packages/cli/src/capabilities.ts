/**
 * Static checks over `src-tauri/capabilities/*.json|toml` for the
 * per-function permissions of `tauri-plugin-pylair` (docs/security.md,
 * "Restricting functions per window"). Run by `pylair build` and
 * `pylair doctor`.
 *
 * The plugin enforces the scopes at runtime and fails closed; these checks
 * exist to catch the configurations that are legal but almost certainly not
 * what the author meant, before an app ships:
 *
 *  - `allow: []` on a pylair permission: Tauri treats an empty list as "no
 *    scope entries", which permits EVERY function. (error)
 *  - a window that scopes `python-call` but not `python-call-stream`, or the
 *    reverse: the unscoped command is an open side door. (error)
 *  - a malformed scope entry: the plugin refuses every call it covers. (error)
 *  - a pattern that matches no exposed function: a typo, or a function that
 *    was renamed. (warning, only when the exposed names are known)
 *  - different scopes on the two commands of one window. (warning)
 */

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { parse as parseToml } from "smol-toml";

export interface Finding {
  level: "error" | "warn";
  /** Path relative to the app dir, forward slashes. */
  file: string;
  /** Capability `identifier`; empty when the file did not parse. */
  capability: string;
  message: string;
}

export interface Capability {
  file: string;
  identifier: string;
  windows: string[];
  permissions: unknown[];
}

const CALL = "python-call";
const STREAM = "python-call-stream";

/** Permission identifier -> the call commands it grants. */
const GRANTS: Record<string, string[]> = {
  "pylair:default": [CALL, STREAM],
  "pylair:allow-python-call": [CALL],
  "pylair:allow-python-call-stream": [STREAM],
};

/** The plugin's matching rule: exact, or a single trailing `*` as a prefix
 * wildcard. A `*` anywhere else is a literal character. Keep in step with
 * `fn_scope::pattern_matches` in the plugin. */
export function patternMatches(pattern: string, name: string): boolean {
  return pattern.endsWith("*") ? name.startsWith(pattern.slice(0, -1)) : pattern === name;
}

const isObj = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

const strings = (v: unknown): string[] =>
  Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];

export function loadCapabilities(appDir: string): {
  capabilities: Capability[];
  findings: Finding[];
} {
  const dir = join(appDir, "src-tauri", "capabilities");
  const capabilities: Capability[] = [];
  const findings: Finding[] = [];
  if (!existsSync(dir)) return { capabilities, findings };
  for (const name of readdirSync(dir).sort()) {
    if (!/\.(json|toml)$/.test(name)) continue;
    const file = `src-tauri/capabilities/${name}`;
    let raw: unknown;
    try {
      const text = readFileSync(join(dir, name), "utf-8");
      raw = name.endsWith(".toml") ? parseToml(text) : JSON.parse(text);
    } catch (e) {
      findings.push({
        level: "error",
        file,
        capability: "",
        message: `cannot parse this capability file: ${(e as Error).message}`,
      });
      continue;
    }
    if (!isObj(raw)) {
      findings.push({
        level: "error",
        file,
        capability: "",
        message: "a capability file must hold one capability object",
      });
      continue;
    }
    capabilities.push({
      file,
      identifier: typeof raw.identifier === "string" ? raw.identifier : name,
      windows: strings(raw.windows).length ? strings(raw.windows) : strings(raw.webviews),
      permissions: Array.isArray(raw.permissions) ? raw.permissions : [],
    });
  }
  return { capabilities, findings };
}

/** A pylair permission entry that carries (or may carry) a scope. */
interface Scoped {
  cap: Capability;
  id: string;
  /** Valid, non-empty names from allow and deny. */
  names: { list: "allow" | "deny"; name: string }[];
  /** Any non-empty allow or deny list, valid or not (malformed entries
   * fail closed at runtime, so they still scope). */
  scoped: boolean;
}

function scopedEntries(cap: Capability, out: Finding[]): Scoped[] {
  const res: Scoped[] = [];
  for (const p of cap.permissions) {
    // A bare string grants the permission with no scope entries.
    if (typeof p === "string" && p.startsWith("pylair:")) {
      res.push({ cap, id: p, names: [], scoped: false });
      continue;
    }
    if (!isObj(p) || typeof p.identifier !== "string") continue;
    const id = p.identifier;
    if (!id.startsWith("pylair:")) continue;
    const at = (message: string) =>
      out.push({ level: "error", file: cap.file, capability: cap.identifier, message });
    const s: Scoped = { cap, id, names: [], scoped: false };
    for (const list of ["allow", "deny"] as const) {
      const v = p[list];
      if (v === undefined) continue;
      if (!Array.isArray(v)) {
        at(`\`${id}\`: \`${list}\` must be a list of { "name": ... } entries`);
        s.scoped = true;
        continue;
      }
      if (v.length === 0) {
        if (list === "allow")
          at(
            `\`${id}\` has \`allow: []\`. An empty allow list is the same as no scope ` +
              "entries and permits everything; it does NOT mean allow nothing. " +
              "List the functions to allow, add a `deny` list, or drop the permission " +
              "to withhold the command.",
          );
        continue;
      }
      s.scoped = true;
      for (const e of v) {
        if (!isObj(e)) {
          at(`\`${id}\`: ${list} entry ${JSON.stringify(e)} must be an object like { "name": "fn" }; the plugin refuses every call this entry covers`);
          continue;
        }
        const extra = Object.keys(e).filter((k) => k !== "name");
        if (typeof e.name !== "string" || extra.length) {
          const why = [
            typeof e.name !== "string" ? 'needs a string "name"' : "",
            extra.length ? `has unknown key(s) ${extra.map((k) => `"${k}"`).join(", ")}` : "",
          ]
            .filter(Boolean)
            .join(" and ");
          at(`\`${id}\`: ${list} entry ${JSON.stringify(e)} ${why}; the plugin refuses every call this entry covers`);
          continue;
        }
        s.names.push({ list, name: e.name });
      }
    }
    res.push(s);
  }
  return res;
}

/** Warn for every pattern that matches no name in `exposed`. */
export function checkPatterns(caps: Capability[], exposed: string[]): Finding[] {
  const out: Finding[] = [];
  const sink: Finding[] = [];
  for (const cap of caps)
    for (const s of scopedEntries(cap, sink))
      for (const { list, name } of s.names)
        if (!exposed.some((f) => patternMatches(name, f)))
          out.push({
            level: "warn",
            file: cap.file,
            capability: cap.identifier,
            message:
              `\`${s.id}\`: ${list} pattern "${name}" matches no exposed function ` +
              "(typo, or a renamed function?)",
          });
  return out;
}

export function checkCapabilities(caps: Capability[], exposed?: string[]): Finding[] {
  const findings: Finding[] = [];
  const perWindow = new Map<string, { s: Scoped; cmds: string[] }[]>();
  for (const cap of caps) {
    for (const s of scopedEntries(cap, findings)) {
      const cmds = GRANTS[s.id];
      if (!cmds) continue;
      for (const w of cap.windows) {
        if (!perWindow.has(w)) perWindow.set(w, []);
        perWindow.get(w)!.push({ s, cmds });
      }
    }
  }

  for (const [window, entries] of perWindow) {
    const scopeOf = (cmd: string) => {
      const mine = entries.filter((e) => e.cmds.includes(cmd));
      const scoped = mine.filter((e) => e.s.scoped);
      return {
        granted: mine.length > 0,
        scoped,
        sig: scoped
          .flatMap((e) => e.s.names.map((n) => `${n.list}:${n.name}`))
          .sort()
          .join("\n"),
      };
    };
    const call = scopeOf(CALL);
    const stream = scopeOf(STREAM);
    const half = (scopedCmd: string, openCmd: string, from: typeof call) => {
      const e = from.scoped[0];
      findings.push({
        level: "error",
        file: e.s.cap.file,
        capability: e.s.cap.identifier,
        message:
          `window "${window}" scopes \`${scopedCmd}\` (via \`${e.s.id}\`) but not \`${openCmd}\`; ` +
          `the unscoped command bypasses the function restriction. Scope both: use the ` +
          "set-level form { \"identifier\": \"pylair:default\", \"deny\": [...] } or give " +
          "both commands the same entries.",
      });
    };
    if (call.scoped.length && !stream.scoped.length && stream.granted) half(CALL, STREAM, call);
    else if (stream.scoped.length && !call.scoped.length && call.granted)
      half(STREAM, CALL, stream);
    else if (call.scoped.length && stream.scoped.length && call.sig !== stream.sig) {
      const e = call.scoped[0];
      findings.push({
        level: "warn",
        file: e.s.cap.file,
        capability: e.s.cap.identifier,
        message:
          `window "${window}" has different function scopes on \`${CALL}\` and ` +
          `\`${STREAM}\`; a function may be callable one way and denied the other`,
      });
    }
  }

  if (exposed) findings.push(...checkPatterns(caps, exposed));
  return findings;
}

export function formatFinding(f: Finding): string {
  const cap = f.capability ? ` (capability "${f.capability}")` : "";
  return `${f.level}: ${f.file}${cap}: ${f.message}`;
}

/**
 * The static checks `pylair build` runs before any slow work: throws,
 * listing every error, if any; otherwise returns the warnings.
 */
export function assertCapabilities(appDir: string, exposed?: string[]): Finding[] {
  const { capabilities, findings } = loadCapabilities(appDir);
  const all = [...findings, ...checkCapabilities(capabilities, exposed)];
  const errors = all.filter((f) => f.level === "error");
  if (errors.length)
    throw new Error(
      `capability check failed (${errors.length} error${errors.length === 1 ? "" : "s"}):\n` +
        errors.map((f) => `  ${formatFinding(f)}`).join("\n"),
    );
  return all.filter((f) => f.level === "warn");
}
