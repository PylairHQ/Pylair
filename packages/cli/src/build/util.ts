/** Small helpers shared by build.ts and the build/ modules. */

import { spawnSync } from "node:child_process";
import { platform } from "node:os";

export const isWin = platform() === "win32";

export function run(cmd: string, args: string[], cwd: string) {
  const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
  if (r.error) throw new Error(`${cmd} ${args.join(" ")} failed to start: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`${cmd} ${args.join(" ")} exited ${r.status}`);
}
