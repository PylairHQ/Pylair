/** python-build-standalone: the pinned release, the supported versions and
 * host triples, and the SHA-256 of every asset committed in-repo. */

import { arch, platform } from "node:os";

export const PBS_TAG = "20251120";
export const PBS_VERSIONS: Record<string, string> = { "3.12": "3.12.12", "3.13": "3.13.9" };
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
  "3.12.12-aarch64-apple-darwin": "9955ac87eff0d561e64920dc46fd8625912d2c2be4a53c66ad80125d6d9fbdb7",
  "3.12.12-aarch64-unknown-linux-gnu": "4c4594cb38ddf6769f11f86818a4c6617652f4994e5a962eb4f8cf0ec977a1be",
  "3.12.12-x86_64-apple-darwin": "1b4d8eec597ff69a3b67946f30622a6c25c6612e6d15aa431b864ebff66ac8a1",
  "3.12.12-x86_64-pc-windows-msvc": "70ecfaf0bfc58298db11d6389be92c29ec4ef8cd93ebb6a7e54b41d9fc056b10",
  "3.12.12-x86_64-unknown-linux-gnu": "8d2cd1393c1aec9d701cebeccb83d1519951f71d3ecb42d5e70c70af4db9be34",
  "3.13.9-aarch64-apple-darwin": "29d38bd5fbe267b37cecac990015c662796edcf51c2f5e321d59ec06a1850816",
  "3.13.9-aarch64-unknown-linux-gnu": "fdc5d80488b64fcef3b99f297618f68e5374c13d89e888a66abeff4967afd308",
  "3.13.9-x86_64-apple-darwin": "9da51afb00480a8ccd4909bd302c80d9c104d633c5e865cf37dd5e587185e125",
  "3.13.9-x86_64-pc-windows-msvc": "f4c22b31ddbf8d7824cbcba2d8707621c2c8fab1fb6d2c1810c2bb0304d8e9a8",
  "3.13.9-x86_64-unknown-linux-gnu": "1bc9a94ab2425fb6ae58e45f69cab61647759b57895649701390822d976fa923",
};

export function hostTriple(): string {
  const t = HOST_TRIPLES[`${platform()}:${arch()}`];
  if (!t) throw new Error(`no standalone CPython build for ${platform()}/${arch()}`);
  return t;
}
