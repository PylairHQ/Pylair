import { test, describe, after } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";

import { parseSpec, resolveModelSpec } from "../dist/models.js";

const SHA = "a".repeat(40);
const DAY = new Date("2026-11-27T12:00:00Z");

/** Loopback stub of the Hugging Face API: revision and metadata
 * endpoints with a scripted response table, recording auth headers. */
function stubApi(table) {
  const requests = [];
  const srv = createServer((req, res) => {
    requests.push({ url: req.url, auth: req.headers.authorization ?? null });
    const key = `${req.method} ${req.url}`;
    const hit = table[key];
    if (!hit) {
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "not found" }));
      return;
    }
    res.writeHead(hit.status, { "content-type": "application/json" });
    res.end(JSON.stringify(hit.body ?? {}));
  });
  return new Promise((resolve) => {
    srv.listen(0, "127.0.0.1", () => {
      resolve({
        base: `http://127.0.0.1:${srv.address().port}`,
        requests,
        close: () => new Promise((done) => srv.close(done)),
      });
    });
  });
}

const servers = [];
after(() => Promise.all(servers.map((s) => s.close())));

async function api(table) {
  const s = await stubApi(table);
  servers.push(s);
  return s;
}

describe("parseSpec", () => {
  test("repo only defaults the ref to main", () => {
    assert.deepEqual(parseSpec("org/model"), { repo: "org/model", ref: "main" });
  });
  test("org/model@ref splits on the last @", () => {
    assert.deepEqual(parseSpec("org/model@v1.2"), { repo: "org/model", ref: "v1.2" });
  });
  test("malformed specs are argument errors", () => {
    assert.throws(() => parseSpec("model"), /org\/model/);
    assert.throws(() => parseSpec("org/"), /org\/model/);
    assert.throws(() => parseSpec("org/model@"), /empty ref/);
  });
});

describe("resolveModelSpec", () => {
  test("happy path prints the sha and the paste line", async () => {
    const s = await api({
      "GET /api/models/org/model/revision/main": { status: 200, body: { sha: SHA } },
    });
    const r = await resolveModelSpec("org/model", { endpoint: s.base, now: DAY });
    assert.equal(r.sha, SHA);
    assert.deepEqual(r.lines, [
      SHA,
      `revision="${SHA}"  # org/model@main, resolved 2026-11-27`,
    ]);
  });

  test("an explicit ref lands in the query path and the paste line", async () => {
    const s = await api({
      "GET /api/models/org/model/revision/v1.2": { status: 200, body: { sha: SHA } },
    });
    const r = await resolveModelSpec("org/model@v1.2", { endpoint: s.base, now: DAY });
    assert.equal(r.lines[1], `revision="${SHA}"  # org/model@v1.2, resolved 2026-11-27`);
  });

  test("the token rides as a Bearer header and never in output", async () => {
    const s = await api({
      "GET /api/models/org/model/revision/main": { status: 200, body: { sha: SHA } },
    });
    const r = await resolveModelSpec("org/model", {
      endpoint: s.base,
      token: "hf_secret",
      now: DAY,
    });
    assert.equal(s.requests[0].auth, "Bearer hf_secret");
    assert.ok(!r.lines.join("\n").includes("hf_secret"));
  });

  test("unknown repo: revision 404 plus metadata 404", async () => {
    const s = await api({});
    await assert.rejects(
      resolveModelSpec("org/ghost", { endpoint: s.base }),
      /unknown repository 'org\/ghost'/,
    );
  });

  test("bad ref: revision 404 but metadata 200", async () => {
    const s = await api({
      "GET /api/models/org/model": { status: 200, body: { sha: SHA } },
    });
    await assert.rejects(
      resolveModelSpec("org/model@nosuchbranch", { endpoint: s.base }),
      /no revision 'nosuchbranch' on 'org\/model'/,
    );
  });

  test("gated without a token names HF_TOKEN", async () => {
    const s = await api({
      "GET /api/models/org/model/revision/main": { status: 403, body: {} },
    });
    await assert.rejects(
      resolveModelSpec("org/model", { endpoint: s.base }),
      /gated or private.*HF_TOKEN/s,
    );
  });

  test("offline is a clear network error", async () => {
    // nothing listens on this port - guaranteed refused
    await assert.rejects(
      resolveModelSpec("org/model", { endpoint: "http://127.0.0.1:1" }),
      /could not reach the Hugging Face API.*offline/,
    );
  });

  test("a non-200, non-error-class status is surfaced", async () => {
    const s = await api({
      "GET /api/models/org/model/revision/main": { status: 500, body: {} },
    });
    await assert.rejects(
      resolveModelSpec("org/model", { endpoint: s.base }),
      /HTTP 500/,
    );
  });

  test("a response without a 40-hex sha is rejected", async () => {
    const s = await api({
      "GET /api/models/org/model/revision/main": { status: 200, body: { sha: "abc" } },
    });
    await assert.rejects(
      resolveModelSpec("org/model", { endpoint: s.base }),
      /no full commit sha/,
    );
  });
});
