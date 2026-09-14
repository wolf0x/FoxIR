## FoxIR v1.0.8 — Parallel Multi-Session, Cross-Session Isolation, and Memory-Layer Cleanup

24 commits since v1.0.7. This release delivers the full multi-session story: you can create up to 5 isolated sessions (Settings-hot), switch between them from the sidebar, and run several concurrently — each with its own conversation, STOP control, tool view, and TASKS/TODO. Also lands the memory-layer simplification (shallow memory removed, deep memory projected back to MEMORY.md) and the parallel-session isolation fixes.

### New Features

**Parallel multi-session (P1/P2 落地)**
- Multi-session navigation index + `/api/sessions` + history filtered per session; sidebar panel with per-session snapshot/switch/rename/delete
- `Max sessions = 5` (Settings hot-reload); per-session cancel wired into Instant runs; executor slot registry so concurrent runs don’t collide
- 片② demux/spawn: each Instant run is drained by its own spawned task; Expert stays inline. Same-session follow-ups queue behind the running task; interject messages render on execution-entry as a user bubble (no tool calls / session isolation)
- per-session STOP is isolated end-to-end; switching sessions no longer stops another session's run, and each session keeps its own START/STOP state

**Cross-session isolation (this fix)**
- `drain_session_stream` now injects the owning `session` into every forwarded WebSocket event; the frontend routes streaming by `msg.session` instead of a single global `taskSessionId`, so A and B running in parallel never cross-render (fixes "A's output appears in B")
- TASKS/TODO are now per-session (`todos-<session>.json`) — each session's list is fully independent; `/api/todos?session=` serves the right list and the sidebar refreshes it on switch/new
- A finishing background session no longer tears down the active session's live tool cards/text

**Memory-layer cleanup**
- Removed the shallow-memory concept and all its artifacts (config, code paths, `shallow_memories` leftovers); deep facts + SQLite auto-summary remain
- Deep memory is now projected to workspace `MEMORY.md` so you can review it in the original window; knowledge base is methodology-only (no auto-precipitation of session conclusions, threat-intel refs removed)

### Verification
- `cargo check --lib` clean; inline JS passes `node --check`; `cargo build --release` succeeds
