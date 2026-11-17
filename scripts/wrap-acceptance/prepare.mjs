// Turn a freshly scaffolded wrap app (FastAPI, command mode) into the
// acceptance app for another framework. Used by the `wrap-acceptance` CI job:
//
//   node scripts/wrap-acceptance/prepare.mjs <app-dir> <command|gradio|streamlit>
//
// command: nothing to do (the template is the FastAPI sample).
// gradio / streamlit: replace app/main.py with the fixture app, swap the
// FastAPI dependencies for the framework (pinned to the majors the adapters
// support), and point [tool.pylair.wrap] at the entry file.

import { copyFileSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [appDir, framework] = process.argv.slice(2);
const FRAMEWORKS = {
  gradio: { dep: "gradio>=5,<6", fixture: "gradio_app.py" },
  streamlit: { dep: "streamlit>=1,<2", fixture: "streamlit_app.py" },
};

if (!appDir || !["command", ...Object.keys(FRAMEWORKS)].includes(framework)) {
  console.error("usage: prepare.mjs <app-dir> <command|gradio|streamlit>");
  process.exit(2);
}
if (framework === "command") process.exit(0);

const here = dirname(fileURLToPath(import.meta.url));
const { dep, fixture } = FRAMEWORKS[framework];

rmSync(join(appDir, "app", "main.py"));
copyFileSync(join(here, fixture), join(appDir, "app", "app.py"));

const pyproject = join(appDir, "pyproject.toml");
let py = readFileSync(pyproject, "utf-8");
const before = py;
// dependencies = ["pylair @ ...", "fastapi>=0.115", "uvicorn>=0.30"]: keep pylair.
py = py.replace(/^dependencies = \[(.*)\]$/m, (_all, inner) => {
  const pylair = inner.split('", "')[0].replace(/^"|"$/g, "");
  return `dependencies = ["${pylair}", "${dep}"]`;
});
py = py.replace(
  /\[tool\.pylair\.wrap\][\s\S]*$/,
  `[tool.pylair.wrap]\nframework = "${framework}"\nentry = "app.py"\n`,
);
if (py === before || !py.includes(`framework = "${framework}"`) || !py.includes(dep)) {
  console.error("prepare.mjs: the scaffolded pyproject.toml did not have the expected shape");
  process.exit(1);
}
writeFileSync(pyproject, py);
console.log(`prepared ${appDir} for ${framework}`);
