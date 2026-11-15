/** The python-build-standalone download: retry with backoff, Range
 * resume, progress, the no-data timeout, and the cache plus committed
 * SHA-256 check (docs/build.md, "Slow or flaky networks"). */

import { createHash } from "node:crypto";
import {
  createWriteStream,
  existsSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
} from "node:fs";
import type { IncomingMessage } from "node:http";
import { get } from "node:https";
import { basename, resolve } from "node:path";

const DOWNLOAD_IDLE_TIMEOUT_MS = 30_000;
const MAX_REDIRECTS = 5;
const DOWNLOAD_ATTEMPTS = 4;
const DOWNLOAD_PROGRESS_MS = 10_000;
/** Base delay before retries 2, 3 and 4; each gets +-25 percent jitter. */
const RETRY_BASE_MS = [1_000, 3_000, 8_000];

/** A download failure, plus whether trying again could plausibly help:
 * stalls, socket errors, 5xx, 429 and short bodies can; a 404, a refused
 * redirect or a redirect loop will fail the same way every time. */
export class DownloadError extends Error {
  readonly retryable: boolean;
  constructor(message: string, retryable: boolean, cause?: unknown) {
    super(message, cause === undefined ? undefined : { cause });
    this.name = "DownloadError";
    this.retryable = retryable;
  }
}

/** Errors that surface from the socket or the response stream are
 * transient by nature; classified ones keep their own verdict. */
const asTransient = (e: unknown): Error =>
  e instanceof DownloadError
    ? e
    : new DownloadError(e instanceof Error ? e.message : String(e), true, e);

/** The no-data timeout: 30s unless PYLAIR_DOWNLOAD_TIMEOUT_MS says
 * otherwise, clamped to 5s..600s. A value that is not a whole number is an
 * error, not a silent fallback, so a typo does not hide. */
export function downloadTimeoutMs(env: NodeJS.ProcessEnv = process.env): number {
  const raw = env.PYLAIR_DOWNLOAD_TIMEOUT_MS?.trim();
  if (raw === undefined || raw === "") return DOWNLOAD_IDLE_TIMEOUT_MS;
  if (!/^\d+$/.test(raw))
    throw new DownloadError(
      `PYLAIR_DOWNLOAD_TIMEOUT_MS must be a whole number of milliseconds ` +
        `(5000 to 600000), got "${raw}"`,
      false,
    );
  return Math.min(600_000, Math.max(5_000, Number(raw)));
}

/** "downloaded 12.3 of 45.6 MB (27%)", or without the total when the server
 * did not send a length. */
function progressLine(received: number, total: number): string {
  const mb = (n: number) => (n / 1e6).toFixed(1);
  return Number.isFinite(total) && total > 0
    ? `downloaded ${mb(received)} of ${mb(total)} MB (${Math.floor((received / total) * 100)}%)`
    : `downloaded ${mb(received)} MB`;
}

/** Delay before retry number `retry` (1-based), with jitter. */
export function backoffMs(retry: number, rand: () => number = Math.random): number {
  const base = RETRY_BASE_MS[Math.min(retry, RETRY_BASE_MS.length) - 1]!;
  return Math.round(base * (0.75 + 0.5 * rand()));
}

/** Download `url` to `to`, following up to `maxRedirects` redirects and
 * failing after `timeoutMs` with no bytes. The body lands in `to.part`
 * and is renamed into place only once it is complete — a dropped
 * connection must never leave a partial file at the cache path (the
 * SHA check would then refuse every later build until it is deleted by
 * hand). `get` is injectable so tests need no network. */
export function download(
  url: string,
  to: string,
  opts?: {
    get?: typeof get;
    timeoutMs?: number;
    maxRedirects?: number;
    /** Keep `<to>.part` on failure and continue it with a Range request. */
    resume?: boolean;
    /** Progress line sink (default stderr) and its minimum interval. */
    progress?: (line: string) => void;
    progressMs?: number;
    /** Filled in as bytes arrive, for the caller's failure message. */
    state?: { total?: number };
  },
): Promise<void> {
  const getImpl = opts?.get ?? get;
  const maxRedirects = opts?.maxRedirects ?? MAX_REDIRECTS;
  const part = `${to}.part`;
  const resume = opts?.resume ?? false;
  return new Promise((res, rej) => {
    // Read inside the executor so a bad PYLAIR_DOWNLOAD_TIMEOUT_MS rejects
    // the promise instead of throwing synchronously.
    const timeoutMs = opts?.timeoutMs ?? downloadTimeoutMs();
    let settled = false;
    let out: ReturnType<typeof createWriteStream> | undefined;
    let ticker: ReturnType<typeof setInterval> | undefined;
    const stopTicker = () => {
      if (ticker) clearInterval(ticker);
      ticker = undefined;
    };
    // A resumable download keeps its partial file for the next attempt.
    const dropPart = () => {
      if (!resume) rmSync(part, { force: true });
    };
    const fail = (e: Error) => {
      if (settled) return;
      settled = true;
      stopTicker();
      if (out === undefined) {
        dropPart();
        rej(e);
        return;
      }
      // The stream owns the fd. destroy() waits for in-flight writes
      // before closing it — closing early (closeSync) or twice lets a
      // reused fd number absorb the stale op: the write lands in some
      // other file, or the close kills it. .part is removed only once
      // 'close' says the fd is really gone, keeping the rejection
      // ordered after cleanup.
      const s = out;
      out = undefined;
      s.once("close", () => {
        dropPart();
        rej(e);
      });
      s.destroy();
    };
    const onErr = (e: unknown) =>
      fail(e instanceof Error ? e : new Error(String(e)));
    // Bytes already on disk from an earlier attempt, asked for with Range.
    // The header goes on every hop, so a redirect to a CDN keeps it.
    let offset = 0;
    const restart = () => {
      rmSync(part, { force: true });
      offset = 0;
      follow(url, 0);
    };
    const follow = (u: string, redirects: number): void => {
      try {
        if (redirects > maxRedirects)
          return fail(
            new DownloadError(
              `GET ${url}: too many redirects (cap ${maxRedirects})`,
              false,
            ),
          );
        const onResponse = (r: IncomingMessage) => {
          try {
            if (r.statusCode! >= 300 && r.statusCode! < 400 && r.headers.location) {
              // Location may be relative, and only https targets are
              // fetchable: resolve first, refuse anything else.
              const target = new URL(r.headers.location, u);
              if (target.protocol !== "https:")
                return fail(
                  new DownloadError(
                    `GET ${u}: refusing non-https redirect to ${target.origin}`,
                    false,
                  ),
                );
              return follow(target.href, redirects + 1);
            }
            // The kept .part is unusable (longer than the file, or not a
            // range the server can serve): start over without Range.
            if (r.statusCode === 416 && offset > 0) {
              r.resume();
              return restart();
            }
            const partial = r.statusCode === 206;
            if (r.statusCode !== 200 && !partial)
              return fail(
                new DownloadError(
                  `GET ${u} -> ${r.statusCode}`,
                  r.statusCode! >= 500 || r.statusCode === 429,
                ),
              );
            if (partial) {
              if (offset === 0)
                return fail(
                  new DownloadError(`GET ${u}: 206 to a request without Range`, false),
                );
              const m = /^bytes (\d+)-\d+\/(?:\d+|\*)$/.exec(
                String(r.headers["content-range"] ?? ""),
              );
              // Appending at the wrong position would corrupt the file.
              if (!m || Number(m[1]) !== offset) {
                r.resume();
                return restart();
              }
            } else {
              offset = 0; // 200: the server ignored Range, so write it all
            }
            const want = Number(r.headers["content-length"]);
            const total = partial
              ? Number(/\/(\d+)$/.exec(String(r.headers["content-range"]))?.[1])
              : want;
            if (opts?.state && Number.isFinite(total)) opts.state.total = total;
            let got = 0;
            let ended = false;
            // A slow link should look slow, not dead: one plain line on
            // stderr at most every 10s, nothing at all for a fast download.
            stopTicker();
            ticker = setInterval(
              () => (opts?.progress ?? console.error)(progressLine(offset + got, total)),
              opts?.progressMs ?? DOWNLOAD_PROGRESS_MS,
            );
            ticker.unref();
            r.on("data", (c: Buffer) => {
              got += c.length;
            });
            r.on("end", () => {
              ended = true;
            });
            r.on("error", (e) => fail(asTransient(e)));
            r.on("close", () => {
              if (!ended)
                fail(
                  new DownloadError(`GET ${u}: connection dropped mid-body`, true),
                );
            });
            // Open eagerly: <part>'s existence is synchronous, so no
            // deferred fs.open can land after fail()'s rmSync and recreate
            // the file. autoClose: false keeps fd closing on our explicit
            // destroy() calls only (see fail()).
            const stream = createWriteStream(part, {
              fd: openSync(part, partial ? "a" : "w"),
              autoClose: false,
            });
            out = stream;
            r.pipe(stream);
            stream.on("finish", () => {
              if (settled) return;
              out = undefined;
              stream.once("close", () => {
                if (!Number.isNaN(want) && got !== want)
                  return fail(
                    new DownloadError(
                      `GET ${u}: incomplete body: ${got} of ${want} bytes`,
                      true,
                    ),
                  );
                settled = true;
                stopTicker();
                renameSync(part, to);
                res();
              });
              stream.destroy();
            });
            stream.on("error", fail);
          } catch (e) {
            onErr(e);
          }
        };
        const req =
          offset > 0
            ? getImpl(u, { headers: { Range: `bytes=${offset}-` } }, onResponse)
            : getImpl(u, onResponse);
        req.setTimeout(timeoutMs, () =>
          req.destroy(
            new DownloadError(
              `GET ${u}: timed out: no data for ${timeoutMs / 1000}s`,
              true,
            ),
          ),
        );
        req.on("error", (e) => fail(asTransient(e)));
      } catch (e) {
        onErr(e);
      }
    };
    if (resume && existsSync(part)) offset = statSync(part).size;
    follow(url, 0);
  });
}

/** `download` with up to 4 attempts and exponential backoff. Only failures
 * marked retryable are retried; SHA verification stays with
 * ensureTarball. `sleep` is injectable so tests do not wait. */
export async function downloadWithRetry(
  url: string,
  to: string,
  opts?: Parameters<typeof download>[2] & {
    attempts?: number;
    sleep?: (ms: number) => Promise<void>;
    log?: (line: string) => void;
    /** The committed SHA-256, quoted in the manual-download advice. */
    sha256?: string;
  },
): Promise<void> {
  const state: { total?: number } = {};
  const attempts = opts?.attempts ?? DOWNLOAD_ATTEMPTS;
  const sleep =
    opts?.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  const log = opts?.log ?? ((line: string) => console.warn(line));
  for (let attempt = 1; ; attempt++) {
    try {
      return await download(url, to, { resume: true, state, ...opts });
    } catch (e) {
      if (!(e instanceof DownloadError && e.retryable)) throw e;
      if (attempt >= attempts) {
        const part = `${to}.part`;
        const received = existsSync(part) ? statSync(part).size : 0;
        const size = state.total === undefined ? "unknown" : `${state.total} bytes`;
        throw new DownloadError(
          `could not download ${url} after ${attempts} attempts: received ` +
            `${received} of ${state.total ?? "an unknown number of"} bytes ` +
            `(last error: ${e.message}). ` +
            `Download the file on a better connection and place it at ${resolve(to)}. ` +
            `Expected size ${size}` +
            (opts?.sha256 ? `, SHA-256 ${opts.sha256}` : "") +
            `. The build verifies it and re-downloads if it does not match.`,
          false,
          e,
        );
      }
      const wait = backoffMs(attempt);
      log(
        `retrying (${attempt + 1} of ${attempts}) in ${Math.round(wait / 1000)}s: ${e.message}`,
      );
      await sleep(wait);
    }
  }
}

const sha256File = (p: string) =>
  createHash("sha256").update(readFileSync(p)).digest("hex");

/** Cache + integrity for a downloaded asset: a cached file that fails
 * its SHA is deleted and re-downloaded ONCE — a second mismatch still
 * refuses, naming the cache so it is fixable by hand. `dl` injectable
 * for tests. */
export async function ensureTarball(
  url: string,
  tarball: string,
  want: string,
  dl: (u: string, to: string) => Promise<void> = (u2, to2) =>
    downloadWithRetry(u2, to2, { sha256: want }),
): Promise<void> {
  const cached = existsSync(tarball);
  if (!cached) {
    console.log(`fetching ${basename(tarball)}`);
    await dl(url, tarball);
  } else {
    console.log(`using cached ${basename(tarball)}`);
  }
  if (sha256File(tarball) === want) return;
  if (!cached)
    throw new Error(
      `SHA256 mismatch for ${basename(tarball)}: got ${sha256File(tarball)}, ` +
        `want ${want} — refusing to bundle`,
    );
  // A cached-but-corrupt file is the dropped-partial failure mode: one
  // retry, then refuse.
  console.warn(
    `cached ${basename(tarball)} failed its SHA256 — ` +
      `deleting ${tarball} and re-downloading once`,
  );
  rmSync(tarball, { force: true });
  await dl(url, tarball);
  const got = sha256File(tarball);
  if (got !== want)
    throw new Error(
      `SHA256 mismatch for ${basename(tarball)} after re-download: got ${got}, ` +
        `want ${want} — refusing to bundle (cache file: ${tarball})`,
    );
}
