/** `pylair models resolve org/model[@ref]` - an authoring-time helper
 * (decision 0013 §1): snapshot pins require a full commit sha, and
 * getting one must be easy. Queries the Hugging Face revision endpoint
 * and prints the sha plus a ready-to-paste line. Dev CLI only - nothing
 * here ships into the runtime. */

const REPO_RE = /^[A-Za-z0-9][\w.-]*\/[A-Za-z0-9][\w.-]*$/;
const SHA_RE = /^[0-9a-f]{40}$/;

export interface ModelsResolveOptions {
  endpoint?: string;
  token?: string | undefined;
  now?: Date;
}

export function parseSpec(spec: string): { repo: string; ref: string } {
  const at = spec.lastIndexOf("@");
  const repo = at === -1 ? spec : spec.slice(0, at);
  const ref = at === -1 ? "main" : spec.slice(at + 1);
  if (!REPO_RE.test(repo)) {
    throw new Error(
      `model spec must look like 'org/model' or 'org/model@ref', got '${spec}'`,
    );
  }
  if (!ref) {
    throw new Error(`empty ref in '${spec}' - use 'org/model@main' or a branch, tag or sha`);
  }
  return { repo, ref };
}

interface ApiResult {
  status: number;
  body: unknown;
}

async function apiGet(url: string, token: string | undefined): Promise<ApiResult> {
  const headers: Record<string, string> = {};
  if (token) headers["Authorization"] = `Bearer ${token}`;
  let res: Response;
  try {
    res = await fetch(url, {
      headers,
      redirect: "follow",
      signal: AbortSignal.timeout(15_000),
    });
  } catch (e) {
    throw new Error(
      `could not reach the Hugging Face API: ${(e as Error).message} - ` +
        `check your network connection (offline?)`,
    );
  }
  let body: unknown = null;
  try {
    body = await res.json();
  } catch {
    // a non-JSON error body carries nothing we use
  }
  return { status: res.status, body };
}

function shaFrom(body: unknown): string | null {
  if (body && typeof body === "object" && "sha" in body) {
    const sha = (body as { sha: unknown }).sha;
    if (typeof sha === "string" && SHA_RE.test(sha)) return sha;
  }
  return null;
}

/** Resolve one spec, returning the printed lines. Errors are thrown as
 * plain Errors with user-facing messages; the caller prints and maps
 * the exit code. */
export async function resolveModelSpec(
  spec: string,
  opts: ModelsResolveOptions = {},
): Promise<{ repo: string; ref: string; sha: string; lines: string[] }> {
  const { repo, ref } = parseSpec(spec);
  const endpoint = (opts.endpoint ?? process.env.HF_ENDPOINT ?? "https://huggingface.co").replace(
    /\/+$/,
    "",
  );
  const token = opts.token ?? process.env.HF_TOKEN;

  const rev = await apiGet(
    `${endpoint}/api/models/${repo}/revision/${encodeURIComponent(ref)}`,
    token,
  );
  if (rev.status === 401 || rev.status === 403) {
    throw new Error(
      `'${repo}' is gated or private and the request was rejected - ` +
        `set HF_TOKEN to a Hugging Face access token with access to the repo`,
    );
  }
  if (rev.status === 404) {
    // The revision endpoint 404s for an unknown repo AND for a bad ref
    // on a real repo; one extra metadata lookup tells them apart.
    const meta = await apiGet(`${endpoint}/api/models/${repo}`, token);
    if (meta.status === 404) {
      throw new Error(`unknown repository '${repo}' - check the org/name spelling`);
    }
    if (meta.status === 401 || meta.status === 403) {
      throw new Error(
        `'${repo}' is gated or private and the request was rejected - ` +
          `set HF_TOKEN to a Hugging Face access token with access to the repo`,
      );
    }
    throw new Error(`no revision '${ref}' on '${repo}' - check the branch or tag name`);
  }
  if (rev.status !== 200) {
    throw new Error(`the Hugging Face API returned HTTP ${rev.status} for '${spec}'`);
  }
  const sha = shaFrom(rev.body);
  if (!sha) {
    throw new Error(
      `the Hugging Face API response for '${spec}' carried no full commit sha - ` +
        `unexpected response shape`,
    );
  }
  const day = (opts.now ?? new Date()).toISOString().slice(0, 10);
  return {
    repo,
    ref,
    sha,
    lines: [sha, `revision="${sha}"  # ${repo}@${ref}, resolved ${day}`],
  };
}
