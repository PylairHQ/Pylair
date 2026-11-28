# 3. A local model: offline semantic search

**What you'll build:** Inkwell downloads a pinned embedding model with
verified resumable files, shows download progress in the UI, waits until the
model is ready, and ranks notes offline.

**Time:** about 25 minutes.

Start from your guide 2 app, or copy
[`examples/learn/03-local-model/`](../../examples/learn/03-local-model/) and
run `npm install` and `uv sync` inside it.

## Pick and pin the model

Inkwell uses
[`blobbybob/potion-mxbai-micro`](https://huggingface.co/blobbybob/potion-mxbai-micro),
a real Hugging Face static embedding model. Its weights are about 700 KB in
quantized form, and the files Inkwell downloads total roughly 1.2 MB. That is
small enough for a tutorial and fast enough for CPU search in a desktop app.
It is not a transformer-quality general model, but it is genuinely useful for
small local collections and keeps the feedback loop quick.

Never pin a moving branch in an app. Resolve the branch to a full commit sha:

```sh
npx pylair models resolve blobbybob/potion-mxbai-micro@main
```

This is the output from the pin used here:

```text
4b7425b7053e9976684c939bb1715f718d75b175
revision="4b7425b7053e9976684c939bb1715f718d75b175"  # blobbybob/potion-mxbai-micro@main, resolved 2026-11-28
```

## Declare the snapshot

Add `model2vec==0.10.0` to both dependency lists:

- `pyproject.toml`, so `uv sync` installs it for development.
- `requirements.txt`, so `pylair build` bundles the same library.

Then declare the model at module import time:

<!-- snippet: 03-local-model/backend/app.py -->
```python
MODEL_REPO = "blobbybob/potion-mxbai-micro"
MODEL_REVISION = "4b7425b7053e9976684c939bb1715f718d75b175"

# 700 KB of quantized static embeddings: small enough for a tutorial,
# fast enough for CPU search in a desktop app.
models.model_snapshot(
    "embedder",
    repo=MODEL_REPO,
    revision=MODEL_REVISION,
    allow=["config.json", "model.safetensors", "tokenizer.json"],
)
```

`model_snapshot` is declarative. Importing the module registers the component
but does not download during a codegen scan. Once the runtime starts, Pylair
fetches the pinned Hugging Face snapshot in the background. `allow` limits the
snapshot to the three files this app needs.

Use `models.fetch_snapshot()` when you need an explicit one-off fetch. For app
startup state, prefer the declarative form here because it wires readiness for
you. The full API and storage layout are in [docs/models.md](../models.md).

## Gate search until the model is ready

The search function has two important declarations:

- `worker=warm(pool=1)` keeps the process and loaded model resident.
- `requires="embedder"` makes the runtime wait for the model component before
  dispatching the call.

<!-- snippet: 03-local-model/backend/app.py -->
```python
_embedding_model = None


def load_embedding_model():
    """Load the pinned snapshot once inside this warm worker."""
    global _embedding_model
    if _embedding_model is None:
        from model2vec import StaticModel

        _embedding_model = StaticModel.from_pretrained(str(models.path("embedder")))
    return _embedding_model


@expose(worker=warm(pool=1), requires="embedder")
def search_notes(query: str) -> list[SearchHit]:
    """Rank notes by cosine similarity to the query embedding."""
    if not query.strip():
        raise ValueError("query must not be empty")
    model = load_embedding_model()
    query_vector = model.encode([query], normalize=True)[0]
    hits: list[SearchHit] = []
    for path in sorted(notes_dir().glob("*.md")):
        text = path.read_text(encoding="utf-8")
        note_vector = model.encode([text], normalize=True)[0]
        score = float(query_vector @ note_vector)
        hits.append(
            SearchHit(
                name=path.stem,
                title=title_of(path.stem, text),
                score=round(score, 4),
            )
        )
    return sorted(hits, key=lambda hit: hit.score, reverse=True)
```

`models.path("embedder")` is read-only resolution. Workers are allowed to
resolve a verified model, but never to fetch one; the main runtime owns
downloads. That rule keeps a warm worker from starting a second download or
racing the first one.

Inkwell passes `normalize=True`, so each vector has unit length and cosine
similarity is just a dot product. It encodes the notes on every search because
the corpus is tiny. A larger app would cache vectors in the warm worker and
invalidate them when files change.

## Show readiness in the UI

`requires=` is the server-side gate: a call cannot run too early. The UI
still needs to explain what is happening, so Inkwell also reads
`health().app.components.embedder`.

`health()` is a snapshot. `onApp` fires only when a component changes phase,
not on every progress tick, so progress bars still poll `health()` on a small
cadence:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
  const refreshEmbedder = async () => {
    const snapshot = await health();
    setEmbedder(snapshot.app?.components.embedder ?? null);
  };

  useEffect(() => {
    void refreshEmbedder();
    const timer = window.setInterval(() => void refreshEmbedder(), 700);
    let unlisten: (() => void) | undefined;
    void onApp((change) => {
      if (change.name === "embedder") void refreshEmbedder();
    }).then((off) => {
      unlisten = off;
    });
    return () => {
      window.clearInterval(timer);
      unlisten?.();
    };
  }, []);
```

The status text uses `phase`, `progress` and `detail`:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
function modelText(component: HealthAppComponent | null): string {
  if (!component) return "embedder: starting";
  if (component.phase === "ready") return "embedder: ready";
  if (component.phase === "failed") return `embedder failed: ${component.detail ?? "unknown error"}`;
  const percent = component.progress == null ? "" : ` ${Math.round(component.progress * 100)}%`;
  return `embedder: downloading${percent}${component.detail ? ` (${component.detail})` : ""}`;
}
```

The button is disabled until the component is ready. This is a UX gate in
addition to the runtime's `requires=` gate, not a replacement for it:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
          <button onClick={search} disabled={!modelReady}>
            Search
          </button>
```

The search call itself is small because the worker does the model work:

<!-- snippet: 03-local-model/src/App.tsx -->
```tsx
  const search = async () => {
    try {
      setHits(await searchNotes(query));
      setStatus("search finished");
    } catch (error) {
      setStatus(describeError(error));
    }
  };
```

## Run it

```sh
uv sync
npx pylair dev
```

The first run downloads about 1.2 MB into the app data directory. The model
status moves from downloading to ready, then **Search** becomes enabled. On a
warm checkout, a direct harness run of the same Python code produced:

```text
hits [SearchHit(name='projects', title='Projects', score=0.4553), SearchHit(name='ideas', title='Ideas', score=0.3696), SearchHit(name='welcome', title='Welcome to Inkwell', score=0.3365)]
```

Your ordering can change as you edit the notes. That is the point: the app is
searching your local files, not a hard-coded result.

## Disk, upgrades and private models

Snapshots live under the app data directory in `models/embedder/<revision>`,
with a `current` pointer naming the verified revision to serve. An upgrade
downloads beside the old verified revision and flips `current` atomically, so
peak disk use is old plus new and readers never see a half-written tree.
`HF_TOKEN` is adopted as a bearer token for private Hugging Face repos when
you do not pass an explicit Authorization header, and `HF_ENDPOINT` selects a
mirror. The exact behavior, including resume and verification failure cases,
is in [docs/models.md](../models.md).

## What you learned

> - `pylair models resolve` turns a moving branch into a full revision sha.
> - `models.model_snapshot` declares a pinned multi-file model and starts its
>   verified download at runtime startup.
> - `requires=` closes the first-call race server-side.
> - `health().app` and `onApp` let the UI explain readiness without guessing.
> - `models.path()` in a warm worker resolves the verified snapshot read-only,
>   and the loaded model can stay resident between calls.
>
> **Where next:** this is the end of the first learning-path installment. The
> complete state is in
> [`examples/learn/03-local-model/`](../../examples/learn/03-local-model/).
> Packaging, updates and bundled binaries continue in the next installment;
> meanwhile [docs/build.md](../build.md) is the reference for shipping.
