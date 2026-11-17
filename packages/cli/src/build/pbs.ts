/** python-build-standalone: the pinned release, the supported versions and
 * host triples, and the SHA-256 of every asset committed in-repo. */

import { arch, platform } from "node:os";

export const PBS_TAG = "20261110";
export const PBS_VERSIONS: Record<string, string> = { "3.12": "3.12.16", "3.13": "3.13.18" };
const HOST_TRIPLES: Record<string, string> = {
  "darwin:arm64": "aarch64-apple-darwin",
  "darwin:x64": "x86_64-apple-darwin",
  "win32:x64": "x86_64-pc-windows-msvc",
  "linux:x64": "x86_64-unknown-linux-gnu",
  "linux:arm64": "aarch64-unknown-linux-gnu",
};

/** Expected SHA256 per asset, committed in-repo — a hash fetched from the
 * same release as the tarball catches corruption but not a tampered
 * release. Bump together with PBS_TAG/PBS_VERSIONS; verify each value
 * against upstream SHA256SUMS before committing.
 */
export const PBS_SHA256: Record<string, string> = {
  "3.12.16-aarch64-apple-darwin": "2f11b8aa3726bdbd87dd9eb76568412faddf4f47e26913fa45d6a2e424972acd",
  "3.12.16-aarch64-unknown-linux-gnu": "b6a41defec4975d94b4ff1950236e7383e5c02ba40ea48c9ec33c66087a35aab",
  "3.12.16-x86_64-apple-darwin": "b78a4c479f1964ec484880bee567568266f68da102269fb93a05fe8716b45f91",
  "3.12.16-x86_64-pc-windows-msvc": "355ee61b8207b91574491b1c6dc9b0ebb4408fc56bfafe498f3ef439314b52dc",
  "3.12.16-x86_64-unknown-linux-gnu": "6a9e62dad033790aef722dbd370e0ce76ce17b73627ba3ae679f537049350b39",
  "3.13.18-aarch64-apple-darwin": "7a2384641584ab9984221fcb0fc9265df465922af71be71410529e2a9c3f0540",
  "3.13.18-aarch64-unknown-linux-gnu": "55c00325f7c1fddf325dfc621aabdfbbac4dc54adda224af88ec411dc12db239",
  "3.13.18-x86_64-apple-darwin": "d27d59a722db9f69926eb6d9564071247221f45a04da920ea7f3001c97e32f9b",
  "3.13.18-x86_64-pc-windows-msvc": "2e0fed09d48989f64c9305da9ff2d37a41c0d13ab2aa6283405b2b620dfbf805",
  "3.13.18-x86_64-unknown-linux-gnu": "f2d2199eb25880e07863b391cf7124e7ffa1338b0d65600dfcec117c4223a7fb",
};

export function hostTriple(): string {
  const t = HOST_TRIPLES[`${platform()}:${arch()}`];
  if (!t) throw new Error(`no standalone CPython build for ${platform()}/${arch()}`);
  return t;
}
