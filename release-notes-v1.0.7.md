## FoxIR v1.0.7 — Sub-Agent Orchestration in Instant Mode, `ir_process` Inline Output, and ShimCache v3 Headerless Support

3 commits since v1.0.6. Focused release: makes Instant-mode sub-agent orchestration actually runnable/migrated from Expert, restores `ir_process` inline data (dropping the JSON dump), and adds parsing for the headerless AppCompatCache v3 blob that broke `ShimCache` on recent Windows 10/11.

### New Features

**Instant-mode sub-agent orchestration (Expert → Instant migration completed)**
- `can_spawn` now propagates so the Instant root can spawn workers; previously only the Expert executor path was gated
- Fixed the P0 serde blocker: `SubAgentSpec` (from `spawn_subagent`) now applies `#[serde(default)]` to `tools_allowlist` / `allow_write` / `allow_exec` / `skills`, so a schema-compliant call with only `role` + `prompt` no longer fails with `missing field`. Unknown allowlist names now fail loudly instead of silently producing an empty worker (F8)
- Added a per-Orchestrator concurrency ceiling so one round can’t spin up an unbounded number of workers (P2 cost/DoS guard)
- Added an orchestration-gate diagnostic log line (`[session:…] orchestration gate: prefilter=… -> orchard=…`) so you can see at a glance why a run did/didn’t fan out (F7)

**Typed sub-agent milestones + main-chat cards**
- Worker raw fragments (thinking/text/tool/progress) are no longer broadcast into the parent WebSocket stream — this collapses the wait-phase flood that could stall or drop the connection
- Structured `subagent_spawned` / `subagent_completed` / `subagent_failed` / `budget_update` milestones are now emitted into the parent stream and rendered as grouped cards in the main conversation (plus the existing Expert drawer)
- Frontend suppresses worker raw fragments by `invocation_id`/role, showing only the typed cards

### Behavioral Changes

- `ir_process` no longer writes `output/ir_process-<timestamp>.json`: the full classified process list is returned inline, consistent with the other `ir_*` tools (`ir_scan`, `ir_artifacts`, …). The old smart-truncation / file-dump path was removed
- Wait-phase behavior: a closed consumer channel no longer tears down a run while sub-agents are still in flight — `wait_subagent` is allowed to collect worker results before the loop aborts normally

### Bug Fixes

- **ShimCache parsing on headerless AppCompatCache v3** (fixes the v1.0.6 Known Issue): recent Windows 10/11 live registry blobs carry no `0x00000080` file header and start directly with the first v3 entry; `parse_shimcache` now recognizes the `0x30` / `0x34` entry-header leads and dispatches to a dedicated headerless parser (`0x73743031` "10ts" marker walk). The machine-dependent `test_shimcache` failure should no longer occur on affected hosts

### Verification

- `cargo test --release --no-fail-fast` — lib suite passes including the new `test_win11_headerless_parses_entry` and `subagent_spec_partial_json_uses_safe_defaults`; prior known-issue `test_shimcache` live test is resolved by the headerless parser
- `cargo build --release` — success; `FoxIR.exe` 38.6 MB

### Known Issue

- None blocking. Headless run still has no sub-agent UI events (a documented limitation for later work); Instant orchestration now has both backend and frontend milestone wiring.
