#!/usr/bin/env node
/** create-pylair - scaffold a new Pylair app. The bin is a thin shim; the
 * logic lives in run.ts so tests can drive it with fake streams.
 *
 *   npx create-pylair my-app [--mode bridge|wrap] [--framework command|gradio|streamlit]
 *   npx create-pylair --mode wrap --into <existing-dir> --name my-app
 *
 * --local points all three pylair deps (cargo crate, pypi package, npm
 * package) at a local checkout.
 */

import { run } from "./run.js";

process.exit(
  await run(process.argv.slice(2), {
    input: process.stdin,
    output: process.stdout,
    error: process.stderr,
  }),
);
