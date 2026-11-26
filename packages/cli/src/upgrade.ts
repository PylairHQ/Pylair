/** `pylair upgrade`: version alignment against the compatibility
 * manifest. `--check` is the read-only mode (this PR implements nothing
 * else): report the exact manifest edits and commands an upgrade takes,
 * plus the manual notes for every release in between. Fully offline by
 * default; `--online` additionally asks the npm registry whether the
 * manifest in this CLI is itself behind.
 *
 * Exit codes: 0 up to date, 1 upgrade available, 2 mismatched or
 * unresolvable. */

import { currentRelease, releasesAfter, type CompatRelease } from "./compat.js";
import { judge, releaseVersion, resolveAppVersions, type AppVersions } from "./versions.js";
import { ui } from "./ui/index.js";

export interface UpgradeCheckOptions {
  appDir: string;
  online?: boolean;
  /** Test seam: registry probe. Returns the newest published npm version
   * (the `next` dist-tag), or null when unreachable. */
  fetchLatest?: () => Promise<string | null>;
}

const COMPONENT_LABELS: Record<string, string> = {
  client: "@pylair/client",
  cli: "@pylair/cli",
  crate: "tauri-plugin-pylair",
  pylair: "pylair",
};

function setLine(v: AppVersions, r: CompatRelease): string {
  const parts: string[] = [];
  for (const key of ["client", "cli", "crate", "pylair"] as const) {
    if (v[key].status !== "absent") parts.push(`${COMPONENT_LABELS[key]} ${releaseVersion(r, key)}`);
  }
  return parts.join(", ");
}

/** The exact before/after edit for one component, or null when the
 * component is absent or already at the target version. */
export function editLine(
  v: AppVersions,
  key: keyof AppVersions,
  target: CompatRelease,
): { file: string; from: string; to: string } | null {
  const c = v[key];
  if (c.status !== "resolved" || !c.resolved) return null;
  const want = releaseVersion(target, key);
  if (c.resolved === want) return null;
  if (key === "client" || key === "cli") {
    return {
      file: "package.json",
      from: `"${COMPONENT_LABELS[key]}": "${c.resolved}"`,
      to: `"${COMPONENT_LABELS[key]}": "${want}"`,
    };
  }
  if (key === "crate") {
    // String and table forms both pin with `=`.
    const strip = (s: string) => s.replace(/^[^0-9]*/, "");
    return {
      file: "src-tauri/Cargo.toml",
      from: `version "=${strip(c.resolved)}"`,
      to: `version "=${want}"`,
    };
  }
  return {
    file: "pyproject.toml",
    from: `"pylair==${c.resolved}"`,
    to: `"pylair==${want}"`,
  };
}

interface Plan {
  edits: { file: string; from: string; to: string }[];
  commands: string[];
  notes: { release: string; notes: string[] }[];
}

export function upgradePlan(v: AppVersions, from: CompatRelease): Plan {
  const target = currentRelease();
  const edits: Plan["edits"] = [];
  const commands: string[] = [];
  for (const key of ["client", "cli", "crate", "pylair"] as const) {
    const e = editLine(v, key, target);
    if (e) edits.push(e);
  }
  const npmChanged = edits.some((e) => e.file === "package.json");
  const crateChanged = edits.some((e) => e.file === "src-tauri/Cargo.toml");
  const pyChanged = edits.some((e) => e.file === "pyproject.toml");
  if (npmChanged) commands.push("npm install   # or pnpm install, matching your lockfile");
  if (crateChanged) commands.push("cargo update -p tauri-plugin-pylair   # in src-tauri");
  if (pyChanged) commands.push("uv lock   # then uv sync");
  const notes = releasesAfter(from.npm)
    .filter((r) => r.notes.length > 0)
    .map((r) => ({ release: r.npm, notes: r.notes }));
  return { edits, commands, notes };
}

/** The default registry probe: npm's dist-tags for @pylair/cli. Publishes
 * go under `--tag next` (AGENTS.md release train), so `next` is the tag
 * that tracks the newest release candidate. */
async function fetchLatestNpm(): Promise<string | null> {
  try {
    const r = await fetch("https://registry.npmjs.org/@pylair%2fcli", {
      signal: AbortSignal.timeout(5000),
      headers: { accept: "application/json" },
    });
    if (!r.ok) return null;
    const j = (await r.json()) as { "dist-tags"?: Record<string, string> };
    return j["dist-tags"]?.next ?? j["dist-tags"]?.latest ?? null;
  } catch {
    return null;
  }
}

export async function upgradeCheck(opts: UpgradeCheckOptions): Promise<number> {
  const v = resolveAppVersions(opts.appDir);
  ui.header("pylair upgrade --check", [["app", opts.appDir]], []);
  if (!opts.online) ui.note("offline: comparing against the manifest bundled in this CLI");

  const verdict = judge(v);

  if (verdict.kind === "empty") {
    ui.error("no Pylair components found - is this a pylair app? (need package.json / src-tauri / pyproject.toml)");
    return 2;
  }

  if (verdict.kind === "unresolved") {
    ui.line("cannot judge versions yet - unresolved components:");
    for (const key of ["client", "cli", "crate", "pylair"] as const) {
      const c = v[key];
      if (c.status === "unresolved")
        ui.line(`  ${COMPONENT_LABELS[key]}: declared ${c.declared} - ${c.detail}`);
      if (c.status === "local")
        ui.line(`  ${COMPONENT_LABELS[key]}: ${c.detail} (${c.declared})`);
    }
    ui.line("");
    ui.line("resolve first (install / build / uv sync), or pin registry versions; then re-run.");
    return 2;
  }

  if (verdict.kind === "mismatched") {
    ui.line(`mismatched set - not any single release's combination:`);
    ui.line(`  most of the app agrees with npm-v${verdict.best.npm}'s set, but:`);
    for (const o of verdict.offenders)
      ui.line(`  ${o.component} ${o.have} is out of step - ${o.want} pairs with the rest`);
    ui.line("");
    ui.line("align every component to one release set first; `pylair doctor` shows the full table.");
    return 2;
  }

  const release = verdict.release;
  const target = currentRelease();
  ui.line(`app set:      ${setLine(v, release)}  (npm-v${release.npm})`);

  if (release.npm === target.npm) {
    ui.line(`latest set:   the same - this is the newest set this CLI knows (${target.date})`);
    if (opts.online) await onlineNote(opts, target);
    ui.summary("ok", "up to date");
    return 0;
  }

  ui.line(`latest set:   npm ${target.npm}, crate ${target.crate}, pylair ${target.pylair}  (npm-v${target.npm}, ${target.date})`);
  ui.line("");
  const plan = upgradePlan(v, release);
  if (plan.edits.length > 0) {
    ui.line("edits:");
    let file = "";
    for (const e of plan.edits) {
      if (e.file !== file) {
        ui.line(`  ${e.file}:`);
        file = e.file;
      }
      ui.line(`    - ${e.from}`);
      ui.line(`    + ${e.to}`);
    }
    ui.line("");
    ui.line("then re-resolve the lockfiles:");
    for (const c of plan.commands) ui.line(`  ${c}`);
  } else {
    ui.line("no manifest edits needed - only the lockfiles re-resolve:");
    for (const c of plan.commands) ui.line(`  ${c}`);
  }
  if (plan.notes.length > 0) {
    ui.line("");
    ui.line("manual upgrade notes for the releases in between:");
    for (const n of plan.notes) {
      ui.line(`  npm-v${n.release}:`);
      for (const note of n.notes) ui.line(`    - ${note}`);
    }
  }
  if (opts.online) await onlineNote(opts, target);
  ui.line("");
  ui.summary("warn", `upgrade available: npm-v${release.npm} -> npm-v${target.npm} (nothing was edited)`);
  return 1;
}

async function onlineNote(opts: UpgradeCheckOptions, target: CompatRelease): Promise<void> {
  const probe = opts.fetchLatest ?? fetchLatestNpm;
  const latest = await probe();
  ui.line("");
  if (!latest) {
    ui.note("--online: registry unreachable; offline verdict stands");
  } else if (latest !== target.npm) {
    ui.line(
      `--online: npm has @pylair/cli ${latest} but this CLI's manifest stops at ${target.npm}.` +
        ` Update the CLI first (npm install -D @pylair/cli@${latest}), then re-run - its manifest knows the newer set.`,
    );
  } else {
    ui.note(`--online: registry agrees, ${latest} is the newest published CLI`);
  }
}


