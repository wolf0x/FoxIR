## FoxIR v1.0.9 — Multi-Session Rendering Stability + Memory Soft-Archive

This release hardens the multi-session experience (cross-session rendering/history/tool-card fixes) and lands the deep-memory soft-archive & curator-revive layer. 13 commits since v1.0.8.

### Multi-Session UX / Rendering
- **No more split bubbles on session-switch**: a stream is no longer chopped into multiple assistant bubbles (and no stale mid-stream DOM is resurrected on switch-back)
- **History preserved on switch**: switching away/back no longer clears past user + assistant messages
- **Tool-card ordering restored**: `pre-text -> tool card -> conclusion text` (tool calls no longer render below the final answer)
- **Instant replies stay one bubble**: invocation-id splitting is now Expert-only; background replay no longer re-renders the same text twice
- **"Jump to latest" pill**: a translucent `...` button over the input box appears only when you scroll up; click returns to the latest turn

### Memory (deep facts, soft-archive)
- Soft-GC in curator + grouped / Top-N / archived MEMORY.md projection
- `remember` defaults to Agent pin; User only on explicit request
- Keyword-relevance rerank + touch only relevant ids on inject
- `archived` column + soft-archive/restore/stale-GC (no physical deletes, soft-delete red-line honored)
- **Curator revives re-captured archived facts** via subject-key upsert (no duplicate, no physical delete)

### Other
- WinRM tool embeds group-host HTTP 500/401 diagnostics
- Adopt tool-vs-context reuse policy

### Verification
- `cargo check --lib` clean; inline JS passes `node --check`; `cargo build --release` succeeds
