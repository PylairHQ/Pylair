import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";

import {
  PromptCancelled,
  askSelect,
  askText,
  interactive,
} from "../dist/ui/prompts.js";
import { stripAnsi } from "../dist/ui/style.js";

const FANCY = { fancy: true, color: true, unicode: true, animate: false, columns: 80 };
const PLAIN = { fancy: false, color: false, unicode: false, animate: false, columns: 80 };

/** A fake terminal: a PassThrough stdin marked as a TTY (no setRawMode;
 * the prompts must work without it) and a PassThrough stdout. */
function io(caps = FANCY) {
  const input = new PassThrough();
  input.isTTY = true;
  const output = new PassThrough();
  let text = "";
  output.on("data", (d) => (text += String(d)));
  return { input, output, caps, text: () => text, plain: () => stripAnsi(text) };
}

describe("prompts: gating", () => {
  test("interactive needs a TTY and no CI", () => {
    assert.equal(interactive({ isTTY: true }, {}), true);
    assert.equal(interactive({ isTTY: true }, { CI: "true" }), false);
    assert.equal(interactive({}, {}), false);
    assert.equal(interactive({}, { CI: "true" }), false);
  });
});

describe("prompts: askText", () => {
  test("a typed line resolves, trimmed", async () => {
    const p = io();
    const q = askText(p, "project name");
    p.input.write("  my-app  \n");
    assert.equal(await q, "my-app");
    assert.match(p.plain(), /project name: /);
    const raw = p.text();
    // the echoed input line is erased, then the resolved line takes its place
    assert.match(raw, /\u001b\[1A\u001b\[0J✔ project name: /);
    assert.match(p.plain(), /✔ project name: my-app/, "an answered text prompt ends with a resolved line");
  });

  test("a failed validation shows the message and re-asks", async () => {
    const p = io();
    const q = askText(p, "project name", (v) =>
      /^[a-z]/.test(v) ? undefined : "use lowercase",
    );
    p.input.write("BAD\n");
    p.input.write("good\n");
    assert.equal(await q, "good");
    assert.match(p.plain(), /use lowercase/);
    // attempt line + error line + final prompt line are all erased (3 rows)
    assert.match(p.text(), /\u001b\[3A\u001b\[0J✔ project name: \u001b\[90mgood/);
  });

  test("a wrapped answer erases every row it occupied", async () => {
    const p = io();
    p.output.columns = 20;
    const q = askText(p, "project name");
    p.input.write("abcdefghij\n"); // "project name: " + 10 chars wraps on 20 cols
    assert.equal(await q, "abcdefghij");
    assert.match(p.text(), /\u001b\[2A\u001b\[0J✔ project name: /);
  });

  test("plain: no resolved line - the typed answer stays on screen once", async () => {
    const p = io(PLAIN);
    const q = askText(p, "project name");
    p.input.write("p-app\n");
    assert.equal(await q, "p-app");
    assert.equal(p.text().split("project name: p-app").length - 1, 1);
    assert.doesNotMatch(p.text(), /\[ok\] project name/);
  });

  test("an empty answer re-asks instead of accepting nothing", async () => {
    const p = io();
    const q = askText(p, "project name");
    p.input.write("\n");
    p.input.write("app\n");
    assert.equal(await q, "app");
    assert.match(p.plain(), /a value is required/);
  });

  test("Ctrl+C rejects with PromptCancelled", async () => {
    const p = io();
    const q = askText(p, "project name");
    p.input.write("\x03");
    await assert.rejects(q, PromptCancelled);
  });

  test("Ctrl+C mid-line still cancels; the typed text resolves nothing", async () => {
    const p = io();
    const q = askText(p, "project name");
    p.input.write("half an answer\x03");
    await assert.rejects(q, PromptCancelled);
  });
});

describe("prompts: askSelect", () => {
  const OPTS = [
    { value: "bridge", label: "a new UI that calls Python" },
    { value: "wrap", label: "wrap an existing Python web app" },
    { value: "other", label: "something else" },
  ];

  test("fancy: arrows move the cursor, Enter resolves the option", async () => {
    const p = io();
    const q = askSelect(p, "what are you building?", OPTS);
    p.input.write("\x1b[B"); // down
    p.input.write("\x1b[B"); // down again, wraps nowhere (last index)
    p.input.write("\r");
    assert.equal(await q, "other");
    const shown = p.plain();
    assert.match(shown, /what are you building\?/);
    assert.match(shown, /✔ what are you building\? something else/);
    assert.doesNotMatch(shown, /\?:/, "a label ending in '?' gains no colon");
  });

  test("fancy: up from the first option wraps to the last", async () => {
    const p = io();
    const q = askSelect(p, "pick", OPTS);
    p.input.write("\x1b[A\r");
    assert.equal(await q, "other");
  });

  test("fancy: Ctrl+C cancels and the cursor is restored", async () => {
    const p = io();
    const q = askSelect(p, "pick", OPTS);
    p.input.write("\x03");
    await assert.rejects(q, PromptCancelled);
    assert.ok(p.text().includes("\x1b[?25h"), "cursor must be un-hidden after cancel");
  });

  test("plain: a numbered list answers with a typed digit", async () => {
    const p = io(PLAIN);
    const q = askSelect(p, "pick", OPTS);
    p.input.write("2\n");
    assert.equal(await q, "wrap");
    const shown = p.text();
    assert.match(shown, /1\) a new UI/);
    assert.match(shown, /choose 1-3/);
    assert.match(shown, /\[ok\] pick: wrap an existing Python web app/, "a plain select ends with a resolved line");
  });

  test("plain: an out-of-range answer re-asks", async () => {
    const p = io(PLAIN);
    const q = askSelect(p, "pick", OPTS);
    p.input.write("9\n");
    p.input.write("1\n");
    assert.equal(await q, "bridge");
    assert.match(p.text(), /between 1 and 3/);
  });

  test("plain: Ctrl+C cancels", async () => {
    const p = io(PLAIN);
    const q = askSelect(p, "pick", OPTS);
    p.input.write("\x03");
    await assert.rejects(q, PromptCancelled);
  });
});
