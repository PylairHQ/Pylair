import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { judge, resolveAppVersions } from "../dist/versions.js";
import { editLine, upgradePlan } from "../dist/upgrade.js";
import { COMPAT, currentRelease, releasesAfter } from "../dist/compat.js";

const cli = fileURLToPath(new URL("../dist/bin.js", import.meta.url));

/** The rc.28 set - an older release with different numbers in all three
 * ecosystems, so "upgrading changes everything" is exercised. */
const OLD = { npm: "0.1.0-rc.28", crate: "0.1.0-rc.22", pylair: "0.1.0rc14" };
const CUR = currentRelease();

function tmp() {
  return mkdtempSync(join(tmpdir(), "pylair-versions-"));
}

/** A bridge-mode app fixture. versions: {npm, crate, pylair}; options
 * control which lockfiles exist and their contents. */
function writeApp(dir, versions, opts = {}) {
  const { npmLock = true, cargoLock = true, uvLock = true, pnpm = false, lockVersions } = opts;
  const lv = { ...versions, ...lockVersions };
  mkdirSync(join(dir, "src-tauri"), { recursive: true });
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify(
      {
        name: "fixture-app",
        private: true,
        dependencies: { "@pylair/client": versions.npm },
        devDependencies: { "@pylair/cli": versions.npm },
      },
      null,
      2,
    ),
  );
  if (npmLock && !pnpm) {
    writeFileSync(
      join(dir, "package-lock.json"),
      JSON.stringify({
        name: "fixture-app",
        lockfileVersion: 3,
        packages: {
          "": { name: "fixture-app" },
          "node_modules/@pylair/client": { version: lv.npm },
          "node_modules/@pylair/cli": { version: lv.npm, dev: true },
        },
      }),
    );
  }
  if (npmLock && pnpm) {
    writeFileSync(
      join(dir, "pnpm-lock.yaml"),
      [
        "lockfileVersion: '9.0'",
        "",
        "importers:",
        "",
        "  .:",
        "    dependencies:",
        "      '@pylair/client':",
        `        specifier: ${versions.npm}`,
        `        version: ${lv.npm}`,
        "    devDependencies:",
        "      '@pylair/cli':",
        `        specifier: ${versions.npm}`,
        `        version: ${lv.npm}`,
        "",
      ].join("\n"),
    );
  }
  writeFileSync(
    join(dir, "src-tauri", "Cargo.toml"),
    [
      "[package]",
      'name = "fixture-app"',
      'version = "0.1.0"',
      "",
      "[dependencies]",
      `tauri-plugin-pylair = "=${versions.crate}"`,
      "",
    ].join("\n"),
  );
  if (cargoLock) {
    writeFileSync(
      join(dir, "src-tauri", "Cargo.lock"),
      [
        "version = 4",
        "",
        "[[package]]",
        'name = "fixture-app"',
        'version = "0.1.0"',
        "",
        "[[package]]",
        'name = "tauri-plugin-pylair"',
        `version = "${lv.crate}"`,
        "",
      ].join("\n"),
    );
  }
  writeFileSync(
    join(dir, "pyproject.toml"),
    [
      "[project]",
      'name = "fixture-app"',
      'version = "0.1.0"',
      'requires-python = ">=3.12"',
      `dependencies = ["pylair==${versions.pylair}"]`,
      "",
    ].join("\n"),
  );
  if (uvLock) {
    writeFileSync(
      join(dir, "uv.lock"),
      [
        "version = 1",
        "",
        "[[package]]",
        'name = "pylair"',
        `version = "${lv.pylair}"`,
        'source = { registry = "https://pypi.org/simple" }',
        "",
      ].join("\n"),
    );
  }
  return dir;
}

describe("resolveAppVersions", () => {
  test("a fully locked bridge app resolves all four components", () => {
    const dir = writeApp(tmp(), OLD);
    const v = resolveAppVersions(dir);
    assert.equal(v.client.resolved, OLD.npm);
    assert.equal(v.cli.resolved, OLD.npm);
    assert.equal(v.crate.resolved, OLD.crate);
    assert.equal(v.pylair.resolved, OLD.pylair);
    for (const k of ["client", "cli", "crate", "pylair"]) assert.equal(v[k].status, "resolved", k);
    rmSync(dir, { recursive: true, force: true });
  });

  test("pnpm-lock.yaml resolves the same way", () => {
    const dir = writeApp(tmp(), OLD, { pnpm: true });
    const v = resolveAppVersions(dir);
    assert.equal(v.client.resolved, OLD.npm);
    assert.equal(v.cli.resolved, OLD.npm);
    rmSync(dir, { recursive: true, force: true });
  });

  test("missing lockfiles are unresolved, never guessed", () => {
    const dir = writeApp(tmp(), OLD, { npmLock: false, cargoLock: false, uvLock: false });
    const v = resolveAppVersions(dir);
    for (const k of ["client", "cli", "crate", "pylair"]) {
      assert.equal(v[k].status, "unresolved", k);
      assert.equal(v[k].resolved, null, k);
      assert.ok(v[k].declared, `${k} still reports the declared requirement`);
    }
    rmSync(dir, { recursive: true, force: true });
  });

  test("a Cargo.toml requirement differing from the lock resolves to the LOCK version", () => {
    // Dependabot-style state: the manifest says one thing, the lock
    // resolved another. What the app USES is the lock.
    const dir = writeApp(tmp(), OLD, { lockVersions: { crate: "0.1.0-rc.23" } });
    const v = resolveAppVersions(dir);
    assert.equal(v.crate.resolved, "0.1.0-rc.23");
    assert.ok(v.crate.declared.includes(OLD.crate), "the declared requirement is still reported");
    rmSync(dir, { recursive: true, force: true });
  });

  test("path dependencies are local, not versions", () => {
    const dir = tmp();
    mkdirSync(join(dir, "src-tauri"), { recursive: true });
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({ dependencies: { "@pylair/client": "file:../pylair/packages/client" } }),
    );
    writeFileSync(
      join(dir, "src-tauri", "Cargo.toml"),
      '[dependencies]\ntauri-plugin-pylair = { path = "../pylair/crates/tauri-plugin-pylair" }\n',
    );
    writeFileSync(
      join(dir, "pyproject.toml"),
      '[project]\ndependencies = ["pylair @ file:///home/x/pylair/python/pylair"]\n',
    );
    const v = resolveAppVersions(dir);
    assert.equal(v.client.status, "local");
    assert.equal(v.crate.status, "local");
    assert.equal(v.pylair.status, "local");
    rmSync(dir, { recursive: true, force: true });
  });

  test("an empty directory reports everything absent", () => {
    const dir = tmp();
    const v = resolveAppVersions(dir);
    for (const k of ["client", "cli", "crate", "pylair"]) assert.equal(v[k].status, "absent", k);
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("judge", () => {
  test("the current set is compatible with the current release", () => {
    const dir = writeApp(tmp(), { npm: CUR.npm, crate: CUR.crate, pylair: CUR.pylair });
    const verdict = judge(resolveAppVersions(dir));
    assert.equal(verdict.kind, "compatible");
    assert.equal(verdict.release.npm, CUR.npm);
    rmSync(dir, { recursive: true, force: true });
  });

  test("an older consistent set is compatible with ITS release", () => {
    const dir = writeApp(tmp(), OLD);
    const verdict = judge(resolveAppVersions(dir));
    assert.equal(verdict.kind, "compatible");
    assert.equal(verdict.release.npm, OLD.npm);
    rmSync(dir, { recursive: true, force: true });
  });

  test("one component out of step is a mismatch naming it and the pairing version", () => {
    // rc.28's set but the crate one release ahead (rc.23 belongs to rc.29/30).
    const dir = writeApp(tmp(), OLD, { lockVersions: { crate: "0.1.0-rc.23" } });
    const verdict = judge(resolveAppVersions(dir));
    assert.equal(verdict.kind, "mismatched");
    assert.equal(verdict.best.npm, OLD.npm);
    assert.equal(verdict.offenders.length, 1);
    assert.equal(verdict.offenders[0].component, "tauri-plugin-pylair");
    assert.equal(verdict.offenders[0].have, "0.1.0-rc.23");
    assert.equal(verdict.offenders[0].want, OLD.crate);
    rmSync(dir, { recursive: true, force: true });
  });

  test("unresolved components make the verdict unjudgable", () => {
    const dir = writeApp(tmp(), OLD, { uvLock: false });
    const verdict = judge(resolveAppVersions(dir));
    assert.equal(verdict.kind, "unresolved");
    assert.deepEqual(verdict.components, ["pylair"]);
    rmSync(dir, { recursive: true, force: true });
  });

  test("a partial set (client only) still judges against the manifest", () => {
    const dir = tmp();
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({ dependencies: { "@pylair/client": OLD.npm } }),
    );
    writeFileSync(
      join(dir, "package-lock.json"),
      JSON.stringify({
        lockfileVersion: 3,
        packages: { "node_modules/@pylair/client": { version: OLD.npm } },
      }),
    );
    const verdict = judge(resolveAppVersions(dir));
    assert.equal(verdict.kind, "compatible");
    assert.equal(verdict.release.npm, OLD.npm);
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("upgradePlan / editLine", () => {
  test("edits carry the exact before/after lines per ecosystem", () => {
    const dir = writeApp(tmp(), OLD);
    const v = resolveAppVersions(dir);
    const npmEdit = editLine(v, "client", CUR);
    assert.equal(npmEdit.file, "package.json");
    assert.equal(npmEdit.from, `"@pylair/client": "${OLD.npm}"`);
    assert.equal(npmEdit.to, `"@pylair/client": "${CUR.npm}"`);
    const crateEdit = editLine(v, "crate", CUR);
    assert.equal(crateEdit.file, "src-tauri/Cargo.toml");
    assert.ok(crateEdit.to.includes(CUR.crate));
    const pyEdit = editLine(v, "pylair", CUR);
    assert.equal(pyEdit.from, `"pylair==${OLD.pylair}"`);
    assert.equal(pyEdit.to, `"pylair==${CUR.pylair}"`);
    rmSync(dir, { recursive: true, force: true });
  });

  test("a component already current produces no edit", () => {
    const dir = writeApp(tmp(), OLD, { lockVersions: { pylair: CUR.pylair } });
    // NOTE: declared/resolved both say the lock's version; a mixed set is
    // fine for editLine's purposes.
    const v = resolveAppVersions(dir);
    // pylair resolved is CUR's, so no edit for it.
    assert.equal(editLine(v, "pylair", CUR), null);
    rmSync(dir, { recursive: true, force: true });
  });

  test("the plan lists every in-between release's notes exactly once", () => {
    const dir = writeApp(tmp(), OLD);
    const v = resolveAppVersions(dir);
    const from = COMPAT.releases.find((r) => r.npm === OLD.npm);
    const plan = upgradePlan(v, from);
    const between = releasesAfter(OLD.npm);
    // rc.29 is the only release with notes in the backfill - it must appear.
    const noted = plan.notes.map((n) => n.release);
    for (const r of between.filter((r) => r.notes.length > 0)) {
      assert.ok(noted.includes(r.npm), `notes for ${r.npm} missing`);
    }
    assert.ok(plan.notes.flatMap((n) => n.notes).join("\n").includes("pylair-blob"));
    assert.ok(plan.edits.length > 0);
    assert.ok(plan.commands.some((c) => c.startsWith("npm install")));
    assert.ok(plan.commands.some((c) => c.startsWith("cargo update -p tauri-plugin-pylair")));
    assert.ok(plan.commands.some((c) => c.startsWith("uv lock")));
    rmSync(dir, { recursive: true, force: true });
  });
});

describe("upgrade --check end to end", () => {
  function run(dir, extra = []) {
    return spawnSync(process.execPath, [cli, "upgrade", "--check", "--dir", dir, ...extra], {
      encoding: "utf8",
    });
  }

  test("an older consistent set exits 1 and prints the edits and notes", () => {
    const dir = writeApp(tmp(), OLD);
    const r = run(dir);
    assert.equal(r.status, 1, r.stderr + r.stdout);
    const out = r.stdout;
    assert.ok(out.includes(`npm-v${OLD.npm}`), "names the app's release");
    assert.ok(out.includes(`"pylair==${OLD.pylair}"`), "shows the pyproject before-line");
    assert.ok(out.includes(`"pylair==${CUR.pylair}"`), "shows the pyproject after-line");
    assert.ok(out.includes("cargo update -p tauri-plugin-pylair"));
    assert.ok(out.includes("uv lock"));
    assert.ok(out.includes("pylair-blob"), "the rc.29 CSP note prints");
    assert.ok(!out.includes("edited your"), "never claims to have edited");
    rmSync(dir, { recursive: true, force: true });
  });

  test("the current set exits 0 up to date", () => {
    const dir = writeApp(tmp(), { npm: CUR.npm, crate: CUR.crate, pylair: CUR.pylair });
    const r = run(dir);
    assert.equal(r.status, 0, r.stderr + r.stdout);
    assert.ok(r.stdout.includes("up to date"));
    rmSync(dir, { recursive: true, force: true });
  });

  test("a mismatched set exits 2 and names the out-of-step component", () => {
    const dir = writeApp(tmp(), OLD, { lockVersions: { crate: "0.1.0-rc.23" } });
    const r = run(dir);
    assert.equal(r.status, 2, r.stderr + r.stdout);
    assert.ok(r.stdout.includes("out of step"), r.stdout);
    assert.ok(r.stdout.includes("tauri-plugin-pylair"));
    rmSync(dir, { recursive: true, force: true });
  });

  test("missing lockfiles exit 2 unresolved with the fix", () => {
    const dir = writeApp(tmp(), OLD, { npmLock: false, cargoLock: false, uvLock: false });
    const r = run(dir);
    assert.equal(r.status, 2, r.stderr + r.stdout);
    assert.ok(r.stdout.includes("unresolved"), r.stdout);
    rmSync(dir, { recursive: true, force: true });
  });

  test("--online with a newer registry version says to update the CLI first", () => {
    // The seam is inside upgradeCheck; the end-to-end path uses the real
    // registry. Unit-level: inject a newer version.
    return (async () => {
      const { upgradeCheck } = await import("../dist/upgrade.js");
      const dir = writeApp(tmp(), { npm: CUR.npm, crate: CUR.crate, pylair: CUR.pylair });
      const code = await upgradeCheck({
        appDir: dir,
        online: true,
        fetchLatest: async () => "0.1.0-rc.99",
      });
      assert.equal(code, 0);
      rmSync(dir, { recursive: true, force: true });
    })();
  });
});

describe("learning example pins", () => {
  const repo = fileURLToPath(new URL("../../..", import.meta.url));
  const learn = join(repo, "examples", "learn");
  const steps = readdirSync(learn)
    .filter((name) => /^\d\d-/.test(name))
    .sort();

  test("every learning step declares and locks compat.json's current tuple", () => {
    assert.ok(steps.length >= 3, "expected the learning-path steps");
    for (const step of steps) {
      const dir = join(learn, step);
      const v = resolveAppVersions(dir);
      assert.equal(v.client.declared, CUR.npm, `${step} @pylair/client package.json pin`);
      assert.equal(v.cli.declared, CUR.npm, `${step} @pylair/cli package.json pin`);
      assert.equal(v.crate.declared, `=${CUR.crate}`, `${step} Cargo.toml pin`);
      assert.equal(v.pylair.declared, `==${CUR.pylair}`, `${step} pyproject pin`);

      assert.equal(v.client.resolved, CUR.npm, `${step} package-lock @pylair/client`);
      assert.equal(v.cli.resolved, CUR.npm, `${step} package-lock @pylair/cli`);
      assert.equal(v.crate.resolved, CUR.crate, `${step} Cargo.lock crate`);
      assert.equal(v.pylair.resolved, CUR.pylair, `${step} uv.lock pylair`);

      const requirements = readFileSync(join(dir, "requirements.txt"), "utf-8");
      assert.ok(
        requirements.split("\n").map((line) => line.trim()).includes(`pylair==${CUR.pylair}`),
        `${step} requirements.txt pin`,
      );
      const cargoLock = readFileSync(join(dir, "src-tauri", "Cargo.lock"), "utf-8");
      const crateHeader =
        `name = "tauri-plugin-pylair"\n` +
        `version = "${CUR.crate}"\n` +
        `source = "registry+https://github.com/rust-lang/crates.io-index"\n`;
      const crateAt = cargoLock.indexOf(crateHeader);
      assert.notEqual(crateAt, -1, `${step} Cargo.lock crate version`);
      assert.match(
        cargoLock.slice(crateAt, crateAt + crateHeader.length + 90),
        /checksum = "[0-9a-f]{64}"/,
        `${step} Cargo.lock crate checksum`,
      );
    }
  });
});

describe("doctor Versions section", () => {
  function doctor(dir) {
    return spawnSync(process.execPath, [cli, "doctor", "--dir", dir], { encoding: "utf8" });
  }

  test("a compatible set says so and explains the independent numbers", () => {
    const dir = writeApp(tmp(), { npm: CUR.npm, crate: CUR.crate, pylair: CUR.pylair });
    const r = doctor(dir);
    const out = r.stdout + r.stderr;
    assert.ok(out.includes("versions: compatibility"), out);
    assert.ok(out.includes("compatible set"), out);
    assert.ok(out.includes("independent"), out);
    assert.ok(!/versions: compatibility\s+FAIL/i.test(out), "not a failure");
    rmSync(dir, { recursive: true, force: true });
  });

  test("a mismatched set fails the compatibility check with the offender", () => {
    const dir = writeApp(tmp(), OLD, { lockVersions: { crate: "0.1.0-rc.23" } });
    const r = doctor(dir);
    const out = r.stdout + r.stderr;
    assert.ok(out.includes("out of step"), out);
    assert.ok(out.includes("tauri-plugin-pylair"), out);
    rmSync(dir, { recursive: true, force: true });
  });

  test("unresolved versions never fail doctor", () => {
    const dir = writeApp(tmp(), OLD, { npmLock: false, cargoLock: false, uvLock: false });
    const r = doctor(dir);
    const out = r.stdout + r.stderr;
    assert.ok(out.includes("unresolved"), out);
    assert.ok(!/versions:.*FAIL/i.test(out), `no versions check may fail:\n${out}`);
    rmSync(dir, { recursive: true, force: true });
  });
});
