# Pylair documentation

Specs and guides for building on Pylair, plus the design history.

## Specs and contracts

- [protocol-v1.md](protocol-v1.md): the host/Python wire protocol:
  framing, handshake, liveness, flow control, error codes.
- [cancellation.md](cancellation.md): what `cancel` guarantees per
  execution mode. A contract; changes land with the code.
- [security.md](security.md): threat model, capabilities, resource
  limits, process containment, known advisories, reporting.

## Guides

- [build.md](build.md): `pylair build`: bundled CPython, the locked
  environment, pruning, installers, signing.
- [codegen.md](codegen.md): Python signatures to generated TypeScript
  client stubs.
- [wrap.md](wrap.md): wrap mode: Gradio, Streamlit, FastAPI and other
  Python web apps in a desktop window.
- [wrap-mode-brief.md](wrap-mode-brief.md): the wrap-mode design brief.
- [performance.md](performance.md): bridge latency baseline: method,
  first numbers per OS, and how to run the three bench layers.

## History and ideas

- [decisions/](decisions/): numbered decision records (0000-0007).
- [design/](design/): design notes that outgrew a decision record.
- [ideas.md](ideas.md): parked ideas, not committed scope.

## Maintainer docs

- [maintainers/PROJECT_BRIEF.md](maintainers/PROJECT_BRIEF.md): goals,
  milestones and working agreements.
- [maintainers/HANDOFF.md](maintainers/HANDOFF.md): current release-train
  state and standing operational notes.
