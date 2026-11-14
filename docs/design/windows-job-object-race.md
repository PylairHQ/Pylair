# Windows: the spawn-to-job-assignment gap (K2)

Status: option (a) implemented (owner decision). The design below is kept
as the record of why. What changed in code: `spawn_child` passes
`CREATE_SUSPENDED`, assigns to the job, then calls `resume_main_thread`
on every path; a debug-only `PYLAIR_TEST_ASSIGN_DELAY_MS` seam widens
the gap for `tests/job_race.rs`. Sections 1 to 4 describe the situation
before the change.

Related: `docs/protocol-v1.md` section 6 (orphan prevention),
`docs/security.md` section 7.

## 1. The race

On Windows the supervisor starts the runtime and only then puts it in
the Job Object (`spawn_child` in `supervisor.rs`: `cmd.spawn()`, then
`assign_to_job_object(pid)`, which does `CreateJobObjectW`,
`SetInformationJobObject`, `OpenProcess`, `AssignProcessToJobObject`).
`AssignProcessToJobObject` is not retroactive for descendants: a process
that was already created when the assignment happens stays outside the
job. Two consequences:

1. **Grandchild escape.** If the child spawns a process before it is
   assigned, that process is not in the job. Killing the job, or closing
   it when the host dies, leaves it running.
2. **Host dies in the gap.** If the host exits between spawn and
   assignment, the runtime has no kill-on-close coverage. A healthy
   runtime still exits on stdin EOF (protocol section 6). A runtime
   wedged in native code does not.

### How wide is the window

Measured on this repo's Windows dev machine (release build, 12 cores,
CPython 3.15 from uv, `python -S -c ...`, `CREATE_NO_WINDOW`, 200 runs;
throwaway harness, not committed):

| interval | p50 | p99 | extreme |
|---|---|---|---|
| host: `spawn()` returns to assignment finished | 33 us | 101 us | max 337 us |
| spawn to the child's first Python statement | 57 ms | 69 ms | min 48 ms |

There is no `.await` between `cmd.spawn()` and `assign_to_job_object` in
`spawn_child`, so the host side is one synchronous run of syscalls. The
child needs more than 1000x longer than that just to reach its first line
of Python. What the real runtime does in that time: loader and CRT
start, interpreter init, `python -m pylair`, stdout hygiene, import of
the app module (seconds for torch-class apps), then `hello`. Isolated
workers are only spawned after a host `call`, which needs `hello` first.
So with the bundled python-build-standalone `python.exe`, the runtime
itself spawns nothing before assignment.

Where a real escape is possible:

- **Dev mode with a venv interpreter.** `.venv\Scripts\python.exe` is a
  launcher: it starts the real interpreter as a child process (verified
  here: the interpreter's parent pid is not the shell that started it).
  Pylair's dev spec points at that launcher (`PYLAIR_PYTHON`). If the
  host thread is descheduled between `spawn()` and assignment for longer
  than the launcher needs to create its child, the real interpreter, which
  is the runtime, is outside the job.
- **Import-time subprocesses** in app code. These start tens of ms in at
  the earliest, so they need a stall of that size.
- **A loaded host.** A descheduled thread can lose a scheduler quantum
  (about 15 ms) or more under contention.

I tried to provoke the launcher case: 1920 spawns of the venv launcher
with 32 hosts running in parallel on 12 cores, checking
`IsProcessInJob` for the real interpreter. Result: 1920 inside the job,
0 outside. So the race is real by construction and I could not make it
happen on a healthy machine.

## 2. What exists today (verified vs inferred)

Verified by reading the code:

- The job is created unnamed with null security attributes (not
  inheritable), and `LimitFlags` is exactly
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. No breakaway flags, so a
  descendant cannot opt out with `CREATE_BREAKAWAY_FROM_JOB`.
- The only long-lived handle is `JobHandle`, owned by the
  `monitor_child` task. The temporary process handle used for assignment
  is closed. No child can hold a copy of the job handle, so host death
  closes the last handle.
- If any step fails the function returns `None`, the host emits a stderr
  event, and kill falls back to `taskkill /T`.

Verified by running on this machine:
`cargo test -p tauri-plugin-pylair --test hardening_step0 host_sigkill`
passes 3 of 3: the host is killed with `taskkill /F` (deliberately not
`/T`), and a wedged runtime, a healthy runtime and an isolated worker
are all gone afterwards. That is the answer to "what happens to the
runtime if the host crashes outright": the kernel terminates every
process in the job as soon as the host's handle table is torn down, plus
the stdin-EOF path as a second mechanism. Both hold only for processes
that were in the job at that moment.

Inferred, not tested:

- That the kernel kill-on-close is what removes the runtime in that
  test, rather than EOF. The wedged-runtime variant disables the
  liveness kill and cannot read EOF, so it is the strongest evidence,
  but I did not run it with the job disabled.
- That a host death inside the 33 us to 337 us gap leaves a wedged
  runtime alive. It follows from the code, I did not hit it.

Two doc and code mismatches found on the way:

- The task text puts the assignment call "around line 2646". On current
  `main` the call site is in `spawn_child` (about line 2210) and the
  function is at about line 2689.
- `docs/protocol-v1.md` section 6 says the runtime puts its isolated
  workers in a Job Object too. It does not: `_run_isolated` and
  `_kill_worker` in `_tasks.py` use `creationflags = 0` and
  `taskkill /T`. Workers are covered only because they inherit the
  runtime's job. If the runtime is not in a job (assignment failed, or
  this race), workers have no kernel backstop and `taskkill /T`, which
  walks parent pids, misses a grandchild whose parent already exited.
  Not changed here: protocol section 6 is a contract document and should
  change together with whatever decision is made.

## 3. Options

### (a) `CREATE_SUSPENDED`, assign, then resume

Spawn with `creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)`, assign
to the job, resume. The child executes no instruction before it is in
the job, so every descendant is born inside it.

- **API fit:** stays on `tokio::process::Command`. `creation_flags` is
  already used there. The catch: std and tokio close the primary thread
  handle internally, so there is no public way to `ResumeThread`.
  Two ways to resume:
  - Enumerate threads with `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD)`,
    pick the one thread owned by the pid, `OpenThread(THREAD_SUSPEND_RESUME)`,
    `ResumeThread`. All documented kernel32 APIs, already in
    `windows-sys` 0.59; it needs one extra cargo feature
    (`Win32_System_Diagnostics_ToolHelp`), no new crate.
  - `NtResumeProcess` from ntdll. Shorter, but not in `windows-sys`
    (checked), so it needs a hand-written `extern` block and relies on an
    undocumented export.
- **Size:** about 30 lines in `supervisor.rs`: take the pid in
  `assign_to_job_object`, add `PROCESS_SUSPEND_RESUME` to the
  `OpenProcess` access mask, a roughly 20 line `resume_main_thread(pid)`
  helper, and a guarantee that resume runs on every path, including when
  job creation or assignment fails (otherwise the child hangs suspended
  until the hello timeout).
- **Risk:**
  - A suspended child that is never resumed is an inert orphan, and it
    cannot see stdin EOF because it never runs. So if the host dies in
    the gap, the outcome changes from "healthy runtime exits by itself,
    wedged one leaks" to "always leaks a suspended process". The window
    is the same 33 us to 337 us, but this sub-case gets slightly worse
    while the grandchild-escape case gets closed.
  - Some EDR products flag `CREATE_SUSPENDED` followed by thread resume
    as a hollowing pattern. Chromium and debuggers do the same, so false
    positives should be rare, but it is a new signal for the packaged
    app.
  - Hello timeout clock starts at spawn and now includes the assign and
    resume, a few hundred microseconds. Not a concern.
- **Test:** section 5.

### (b) `PROC_THREAD_ATTRIBUTE_JOB_LIST` at `CreateProcess`

The child is created inside the job: no gap of any kind, and no
suspended-orphan case.

- **API fit:** poor. `std::os::windows::process::CommandExt::raw_attribute`
  is still unstable (checked: E0599 on the pinned stable 1.92.0). That
  means calling `CreateProcessW` ourselves with `STARTUPINFOEXW`,
  `InitializeProcThreadAttributeList` and `UpdateProcThreadAttribute`.
  Doing that also means re-implementing what `Command` gives us today:
  command-line quoting, the environment block (inherit, `env_remove`,
  overrides), cwd, `CREATE_NO_WINDOW`, three stdio pipes with explicit
  handle inheritance (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`, or every
  inheritable handle in the host leaks into the runtime), and
  `kill_on_drop`. And `tokio::process::Child` cannot be built from a raw
  process handle, so we also lose tokio's wait, exit status plumbing and
  async stdio and would have to build them (overlapped named pipes or
  blocking threads, plus a wait via `spawn_blocking` or
  `RegisterWaitForSingleObject`).
- **Size:** my estimate is 250 to 400 lines and substantially more
  `unsafe` than today, in the most delicate function of the supervisor.
  Not a few lines.
- **Risk:** high for an rc: it replaces the spawn path that every
  fault-injection, stderr drain and shutdown test goes through. Requires
  Windows 10 or newer for the attribute (the project minimum is 1809).
- **Test:** the section 5 test, plus the existing suites as a regression
  net for the new spawn path.

### (c) A tiny launcher that assigns itself first

- **API fit:** the host still has to own the job, or host death stops
  killing anything. So the host creates the job, passes the launcher an
  inheritable handle to it, and the launcher assigns itself and closes its
  own copy before starting the runtime. The runtime is then born inside
  the job and the launcher does nothing risky in the gap. Costs: a new
  `.exe` in every Windows bundle, an extra process in the tree, stdio
  that has to pass through or be inherited by the launcher, a handle
  lifetime rule that is easy to get wrong (a copy left open in the
  launcher or runtime defeats kill-on-close), and the host-dies-in-gap
  case is unchanged (the launcher is outside the job until it assigns
  itself).
- **Risk:** new signed binary, AV exposure, bundle size, build pipeline
  for a Rust helper in `pylair build`. Highest cost for the least gain.
- **Test:** same as (a) plus launcher unit tests.

Not recommended. Listed because it was asked about.

### (d) Document the gap and accept it

- **Cost:** zero code. Add a Known limitation and fix the protocol
  section 6 sentence about workers.
- **Basis:** measured host-side gap about 33 us (max 337 us) against a
  child that needs about 50 ms to reach its first statement, and 0 of 1920
  launcher escapes under CPU contention. For the bundled interpreter
  there is no spawn in the window at all; the exposure is dev mode and
  stalled hosts.
- **Risk:** a rare orphan in dev mode or on a starved machine, found only
  by someone who notices a stray `python.exe`.
- **Test:** none new.

## 4. Recommendation

**Do (a), with the Toolhelp resume.** It closes the grandchild-escape
race structurally (including the venv launcher case, which is the one
real exposure), costs about 30 lines, stays on `tokio::process::Command`,
and adds no new unsafe pattern: the same `windows-sys` FFI style as
`assign_to_job_object`, plus one cargo feature. (b) is the only option
that closes the host-dies-in-gap case too, but at 250+ lines and a
rewrite of the spawn path it is not worth it for a 33 us window.

It sits at the edge of "a few lines", it touches `supervisor.rs`
containment, and it has the one trade-off in section 3(a) (inert orphan
if the host dies in the gap), so I am stopping at the design for an owner
decision rather than implementing. If the owner prefers zero risk to the
supervisor for rc.16, (d) is defensible on the numbers above.

Sketch of (a), for sizing only:

```text
cmd.creation_flags(child_creation_flags(console_env) | CREATE_SUSPENDED);
let child = cmd.spawn()?;
let job = assign_to_job_object(pid);          // as today, may be None
let resumed = resume_main_thread(pid);        // ALWAYS, even if job is None
if !resumed { kill child; return Err("could not start the runtime ...") }
```

`resume_main_thread`: snapshot threads, find the entry whose
`th32OwnerProcessID == pid`, `OpenThread(THREAD_SUSPEND_RESUME)`,
`ResumeThread`, close handles. A process created suspended has exactly
one thread, so the first match is the right one.

## 5. How a regression would be tested on Windows

The gap is microseconds wide, so a test cannot rely on timing luck. It
needs a seam that widens the gap on purpose.

**Seam:** a debug-build-only env var, for example
`PYLAIR_TEST_ASSIGN_DELAY_MS`, read in `spawn_child` between spawn (and
resume, in the new code) and assignment. Same convention as the existing
debug-only dev overrides in `lib.rs`; a release build ignores it.

**Test 1, grandchild escape (the one that fails today):**

1. Fake runtime script: immediately `subprocess.Popen` a long-lived
   grandchild (`python -c "import time; time.sleep(600)"`), report the
   grandchild pid on stderr, then do a normal `hello`.
2. Set the delay to about 300 ms and spawn through the real supervisor.
3. Read the grandchild pid from the stderr events.
4. Kill the runtime through the supervisor (or drop the handle) so the
   job closes.
5. Assert the grandchild is gone, using the existing `alive_since`
   helper in `hardening_step0.rs` (start-time token plus `HasExited`, so
   a recycled pid does not read as a survivor).

Before the change the grandchild survives, so the test is red first. After
(a) the child is suspended during the delay, so the grandchild is born
inside the job and dies with it. Under (d) the test is not added.

**Test 2, venv-launcher variant (statistical, not a gate):** the
harness from section 1 as an `#[ignore]` stress test: many spawns of a
launcher-style interpreter under CPU load, assert zero outside the job.

**Test 3, host death in the gap (not written):** the host
role pattern of `host_sigkill_does_not_orphan_wedged_runtime`, with the
delay set long and the host killed during it. Today the runtime may
survive if wedged; with (a) a suspended child survives. If (a) is chosen,
write this test to assert the real behavior and name the residual, so the
Known limitation is backed by a test instead of prose.

CI note: all three run on the `Rust (windows-latest)` leg only; they are
`#[cfg(windows)]`. The existing three `host_sigkill_*` tests keep running
on Windows and Linux and must stay green.

## 6. After implementation

- `tests/job_race.rs` fails on the unfixed code (grandchild survives the
  job closing, with the delay seam on) and passes with (a). It also
  asserts the runtime still reaches `Ready`, so a resume bug cannot hide.
- Test 3 above was not written: asserting that a suspended child leaks
  would pin a bug-shaped behavior. The residual is documented in
  `docs/security.md` section 7 instead and is not covered by a test.
- Worker containment: `docs/protocol-v1.md` section 6 now says workers
  inherit the runtime's job rather than claiming a job of their own.
