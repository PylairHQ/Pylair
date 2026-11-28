# 2. Real work: progress, cancellation and errors

**What you'll build:** Inkwell gains a scan button that streams progress, a
cancel button that really stops the scan, useful error display, and keyword
analysis inside a warm worker process.

**Time:** about 25 minutes.

Start from your guide 1 app, or copy
[`examples/learn/02-real-work/`](../../examples/learn/02-real-work/) and run
`npm install` and `uv sync` inside it.

## Stream progress from Python

A scan is a natural streaming API: every finished file is one progress item.
In Python, an exposed async generator becomes an `AsyncIterable` in
TypeScript.

<!-- snippet: 02-real-work/backend/app.py -->
```python
@expose
async def scan_notes() -> AsyncIterator[ScanProgress]:
    """Scan every note, yielding one progress item per file."""
    paths = sorted(notes_dir().glob("*.md"))
    total = len(paths)
    for index, path in enumerate(paths, start=1):
        text = path.read_text(encoding="utf-8")
        yield ScanProgress(
            done=index,
            total=total,
            note=path.stem,
            words=word_count(text),
            headings=sum(1 for line in text.splitlines() if line.startswith("#")),
            links=text.count("](") + text.count("https://"),
        )
        # The pause keeps progress visible for the tiny sample corpus. Real
        # indexing work usually yields because parsing itself takes time.
        await asyncio.sleep(0.25)
```

The pause is there so progress is visible with only three sample notes. In a
real indexer you would not need it.

`ScanProgress` is another dataclass, so the generated TypeScript stream item
has typed `done`, `total`, `note`, `words`, `headings` and `links` fields.
Pylair also applies stream credit behind the scenes: a fast Python producer
cannot outrun the front end.

## Consume and cancel the stream

The generated `scanNotes` function accepts the same call options as a plain
call. Passing an `AbortSignal` is the cancellation path:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
  const startScan = async () => {
    const ctrl = new AbortController();
    scanCancel.current = ctrl;
    setScan(null);
    try {
      for await (const progress of scanNotes({ signal: ctrl.signal })) {
        setScan(progress);
      }
      setStatus("scan finished");
    } catch (error) {
      if (isCancelled(error)) setStatus("scan cancelled");
      else setStatus(describeError(error));
    } finally {
      scanCancel.current = null;
    }
  };
```

The cancel button only aborts the controller:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
            <button onClick={() => scanCancel.current?.abort()}>Cancel scan</button>
```

For async Python, cancellation arrives at the next `await` point. Python's
`finally` blocks and async cleanup run. Chunks already delivered stay
delivered; the stream then ends with `TASK_CANCELLED`, which the client
exposes as `isCancelled(error)`. The exact contract is in
[docs/cancellation.md](../cancellation.md).

## Show useful errors

Errors from Python are `PythonError` instances. Three optional fields are
especially useful while developing:

- `error.details`: structured input validation failures, each with `path`,
  `expected` and `received`.
- `error.suggestions`: close exposed-function names for a wrong function
  name.
- `error.traceback`: the trimmed Python traceback in dev builds. Release
  builds never include it.

Inkwell formats all three in one helper:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
function describeError(error: unknown): string {
  if (error instanceof PythonError) {
    const parts = [error.message];
    for (const detail of error.details) {
      parts.push(`${detail.path}: expected ${detail.expected}, got ${detail.received}`);
    }
    if (error.suggestions.length > 0) {
      parts.push(`did you mean ${error.suggestions.join(", ")}?`);
    }
    if (error.traceback) {
      parts.push("Python traceback printed in the console");
      console.error(error.traceback);
    }
    return parts.join(" · ");
  }
  return error instanceof Error ? error.message : String(error);
}
```

The app has a **Try an error** button that deliberately sends a number where
`readNote` expects a string:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
  const showWireError = async () => {
    try {
      // Intentionally lie to TypeScript for one call so the runtime's wire
      // validation has something to reject.
      await readNote(123 as unknown as string);
    } catch (error) {
      setStatus(describeError(error));
    }
  };
```

TypeScript normally prevents this mistake. The cast exists only as a small
experiment: it proves the runtime validates the wire input even when a caller
bypasses the types. In a real app, remove the experiment and keep the error
helper.

## CPU work: warm workers and isolated workers

`scan_notes` is async because it can pause between files. Keyword extraction
is different: it is synchronous CPU work. Blocking the runtime event loop
would make every other call stutter, so it runs isolated.

There are two isolated shapes:

- `worker=isolated()` starts a fresh worker process for each call. Use it
  for one-off, crash-prone or strongly stateless work.
- `worker=warm(pool=1)` keeps a worker process alive for that function.
  Imports and module state survive between calls. Use it when startup cost
  matters, especially models.

Inkwell uses a warm worker even though keyword extraction is small. Guide 3
will put a model in the same pattern:

<!-- snippet: 02-real-work/backend/app.py -->
```python
@expose(worker=warm(pool=1))
def analyze_note(name: str) -> NoteAnalysis:
    """Extract simple keywords in a warm worker process."""
    text = read_text_note(name)
    words = [word.lower() for word in _WORDS.findall(text)]
    useful = [word for word in words if len(word) > 3 and word not in _STOPWORDS]
    keywords = [word for word, _count in Counter(useful).most_common(5)]
    longest_line = max((len(line) for line in text.splitlines()), default=0)
    return NoteAnalysis(name=name, keywords=keywords, longest_line=longest_line)
```

Calling it is ordinary typed TypeScript:

<!-- snippet: 02-real-work/src/App.tsx -->
```tsx
  const analyze = async () => {
    try {
      const result = await analyzeNote(selected);
      setAnalysis(
        result.keywords.length > 0
          ? `keywords: ${result.keywords.join(", ")} · longest line ${result.longest_line}`
          : "no keywords yet",
      );
    } catch (error) {
      setAnalysis(describeError(error));
    }
  };
```

Cancellation is stronger here than for inline sync Python. A per-call worker
is terminated, then killed after grace. On POSIX a warm worker first gets an
interrupt while user code runs; if it stays stuck, it is killed and the pool
respawns. Windows warm-worker cancellation is kill-only. Those details matter
for native libraries, so read [docs/cancellation.md](../cancellation.md)
before putting long native calls behind a button.

## Inspect what happened

Restart the dev loop with the inspector:

```sh
npx pylair dev --inspect
```

The terminal announces it before the windows open:

```text
pylair: inspector window will open (PYLAIR_INSPECT=1)
```

The inspector window opens beside the app. Scan, cancel a scan, analyze a
note and press **Try an error**. In the timeline you can correlate the call,
its outcome, its latency, stderr lines and dev reloads. The Calls view keeps
per-function counts and p50/p95 latency; failed rows expand to the same
error fields you saw in the UI.

Payload previews are off by default. If you need them while debugging, use
`--inspect-payloads` and treat them as sensitive local debugging data. The
full reference is [docs/inspector.md](../inspector.md).

## What you learned

> - Async generators become typed TypeScript streams.
> - `AbortController` is the front-end cancellation mechanism, and
>   `isCancelled` distinguishes a user cancel from a failure.
> - `PythonError.details`, `suggestions` and `traceback` expose structured
>   wire errors without parsing the message.
> - CPU-heavy synchronous work belongs in an isolated worker. Warm workers
>   keep expensive state loaded; per-call workers start fresh every time.
> - `pylair dev --inspect` is the fastest way to see what the bridge did.
>
> **Next:** [guide 3](03-local-model.md) uses the warm-worker pattern for a
> real local embedding model. The complete state for this guide is in
> [`examples/learn/02-real-work/`](../../examples/learn/02-real-work/).
