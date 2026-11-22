# 0001: Python packaging — M0 findings

## Context

M0 (docs/maintainers/PROJECT_BRIEF.md section 7) validates the riskiest assumption: bundle a
relocatable interpreter + a locked environment into a Tauri installer that
runs with no system Python, including with a torch-class dependency.

Approach tested in `spikes/m0-packaging/`:

- `cpython-3.12.16+20261110` via astral-sh/python-build-standalone,
  `install_only_stripped` variant, unpacked as an app resource.
- Locked env (`uv pip compile` → `uv pip install --target site-packages`)
  installed *with the bundled interpreter itself*, so wheel tags always
  match the target. numpy = native-wheel canary.
- Backend files staged under `src-tauri/resources/` so the staged tree is
  byte-identical to the installed layout (`python/`, `site-packages/`,
  `backend/`); `app.py --self-test` verifies the payload with zero framing
  or GUI involved.
- Rust `ping_python` command spawns the bundled interpreter and
  round-trips one length-prefixed JSON frame (4-byte BE length + JSON).

## Measured (x86_64-apple-darwin, Intel MBP 16,1)

| Variant | Staged resources | .app | .dmg | Release build |
|---|---|---|---|---|
| numpy only | ~97 MiB (69 MiB interpreter + 28 MiB env) | 134 MiB | 47.8 MiB | cold 2m54s, incr ~1-48s |
| + torch | ~692 MiB | 731 MiB | ~282 MiB* | incr ~48s (cargo) + ~35s dmg |

\* The tauri `dmg` bundling step failed locally with
`hdiutil: couldn't unmount — Resource busy` (a process on this machine
holds the interstitial mount during the ~700 MiB copy; reproducible here,
not expected on clean CI runners). The 282 MiB figure is from an
equivalent manual `hdiutil create -format UDZO`. `.app` bundling itself is
unaffected.

## Findings

1. **The premise holds.** Installed `.app` runs the bundled interpreter
   with no system Python: `sys.executable` and `sys.prefix` resolve inside
   `Contents/Resources`, numpy 2.5.4 imports and computes, torch 2.2.2
   runs real matmul.
2. **Platform wheel asymmetry is real and matters.** On
   x86_64-apple-darwin, `uv pip install torch` resolves torch **2.2.2** —
   the last release with Intel-Mac wheels (PyTorch dropped them). aarch64
   / linux / windows get current torch. `uv` resolving against the bundled
   interpreter handles this *correctly and silently* — exactly what we
   want, but version pinning in `pylair doctor` should surface it.
3. **Install size is env-dominated.** Interpreter ≈ 69 MiB; torch env adds
   ~593 MiB. A torch-class app ships ~300 MiB installers. Acceptable for
   local-AI apps; document it rather than fight it. (Trimming: stripped
   PBS is already minimal; `uv pip install --no-cache` and pruning tests/
   `__pycache__` from site-packages are cheap wins for later.)
4. **Spawn-per-call works for the spike**; supervisor (persistent process,
   restart, drain) is M1 as planned.
5. **Resource staging via a `resources/` mirror tree is the right shape** —
   `verify-bundled.sh` runs identically against staged and installed
   layouts, giving clean-machine checks that need no GUI and no Tauri
   runtime.
6. **`--self-test` mode in the backend is the verification hook** for
   no-Python environments (no frame client needed on the clean machine).

## Decisions

- Proceed with python-build-standalone + `uv pip install --target` as the
  v0.1 packaging path (no PyInstaller-style freezing). Confirmed by M0.
- Keep the spike's committed env at numpy only; the torch numbers above
  are the recorded heavyweight run.
- CI `m0-spike` job builds installers on all three OSes and runs
  `verify-bundled` against the installed payload: bare `ubuntu:22.04`
  container for Linux (genuinely no Python), `.app/Contents/Resources`
  for macOS, silent NSIS install for Windows.

## Open caveats

- GUI launch on truly clean VMs isn't automatable with GitHub runners
  (no hypervisor for macOS/Windows); Linux gets closest via the bare
  container. Manual clean-VM pass remains an M6 exit item for the
  reference app.
- Windows/Linux numbers come from CI; only macOS was measured locally.

## CI confirmation (2026-11-11)

`m0-spike` green on all three OSes: installers built and bundled payload
verified — bare `ubuntu:22.04` container (Linux), silent NSIS install +
payload check (Windows), `.app` resource tree (macOS). Two CI lessons
folded back into the workflow: don't execute AppImages on shared runners
(extract the deb via `dpkg -x` instead), and normalize path separators
before asserting on JSON-reported `sys.executable` on Windows.
**M0 exit criteria met.**
