import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/** The CLI's own version, read from package.json relative to this built
 * file (dist/version.js -> ../package.json), so it works from any cwd and
 * from the published tarball. Falls back to 0.0.0 if unreadable. */
export function cliVersion(): string {
  try {
    const pkg = JSON.parse(
      readFileSync(fileURLToPath(new URL("../package.json", import.meta.url)), "utf-8"),
    );
    return String(pkg.version ?? "0.0.0");
  } catch {
    return "0.0.0";
  }
}
