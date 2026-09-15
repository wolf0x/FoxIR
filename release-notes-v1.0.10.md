## FoxIR v1.0.10 — P0 Security Hardening + Multi-Session Correctness + v3 Conversational Policy

This release hardens the frontend against XSS, fixes several cross-session correctness bugs in the multi-session work, and softens the agent's tool-vs-context expression to feel more natural.

### Security (P0)
- **Markdown XSS closed**: inlined DOMPurify 3.2.4; every `renderMd` output (marked + fallback) is now sanitized before entering `innerHTML`
- **`esc()` unified**: single escaper now covers `& < > " '`; removed the duplicate DOM-roundtrip `esc` and `escHtml` (closes attribute-injection via `'` in file/skill names)
- **Password hygiene**: no hardcoded default `123`; WebSocket password moved from `localStorage` to `sessionStorage`

### Multi-Session Correctness (P1)
- **Queued background runs no longer silently drop**: `bgBuffer` gains `events:[]` in the queued-run path
- **`taskSessionId` hijack fixed**: `queued_run` only claims it for the active session
- **Background errors no longer tear down the active view**: `error` branch now gated by `_doneSid` like `done`
- **No cross-session log bleed**: placeholder/on-the-fly snapshots use an empty `logHist`
- **`clear` targets the right session**: `clear`/`newSession` now carry `session`; server routes clear by that field (no more clearing another session)
- **Message queue is session-aware**: queued messages only auto-send on the *owning* session's completion; `doSend` takes a target session and only drives active-view UI
- **Inject bar is per-session**: pending interjects keyed by session and refreshed on switch

### Conversational UX (v3 policy)
- Replaced the tool-vs-context expression rule that told the model to say "这是实时状态，我重新查一下" with natural phrasings ("刚看了一眼，现在是……"); bans customer-service tone and per-sentence name-dropping while keeping the decision (tool vs context), freshness, and high-risk-confirm rules
- `docs/tool-vs-context-policy.md` synced to v3 (温暖日常版)

### UI (from prior)
- Hidden-style scrollbars for ACTIVITIES / TASKS; jump-to-latest pill now a chevron-down arrow

### Verification
- `cargo check` clean; all inline `<script>` blocks pass `new Function`; `cargo build --release` succeeds
