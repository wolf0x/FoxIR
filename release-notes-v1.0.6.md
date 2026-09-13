## FoxIR v1.0.6 — Expert Multi-Agent Orchestration, Dual-Mode Routing, and `ir_process` Returns Data Again

25 commits since v1.0.5. Absorbs upstream v1.0.5 and the shell Recycle-Bin change (`0914b81`).

### New Features

**Expert mode multi-agent orchestration**
- Manager–Executor–Auditor role separation: Manager plans each round, Executor runs the tools, Auditor verifies claims before a task may complete
- `TaskContract` state persisted to SQLite — a crashed or stopped run resumes instead of restarting
- Bidirectional sync between `TaskContract` and `update_plan`, so the Expert task tree reflects real execution
- Auditor reports are surfaced back to Manager/Executor as cross-round memory; a `Done` claim with zero verified findings is rejected and fed back as a synthetic audit note
- Expert multi-agent UI view: sub-agent status, token budget and plan updates streamed over WebSocket
- Two-layer capability routing prompts

### Behavioral Changes

- **Orchestration gate flipped Expert → Instant.** The Instant root now orchestrates workers; Expert runs serially (Manager → Executor → Auditor)
- `can_spawn=false` is enforced for the Expert executor (no nested sub-agents from an executor)
- Removed the parallel collect path from the managed runner, plus `parallel.rs`, `orch_selftest` and the `parallel_subtasks` field on `ManagerPlan`/`PendingPlan`; dead `orchestration_template` config deleted
- Orchestration delivery and construction are now gated on the rule-based `orchestration_prefilter` signal

### Bug Fixes

- **`ir_process` had never returned any data — since v1.0.0.** A stray `}` in the embedded PowerShell collection script made the whole script block fail to parse, so the child produced zero stdout and the tool answered `{"status":"ok","raw":"","classified":[]}`: success-shaped, empty inside, and indistinguishable from "no suspicious processes found" unless you looked at the key names. The parse failure also silenced itself (`$ErrorActionPreference='SilentlyContinue'` + stderr discarded). Now fixed and verified end to end. Additionally, the payload is no longer dumped whole into the context: the full list is written to `output/ir_process-<timestamp>.json` (`full_output_path`) and only non-safe processes are returned inline.
  *Note for archaeology: this code fix rides in commit `c9274d3`, whose message does not mention `ir_process`.*
- Expert rounds are STOP-interruptible and hang-proof (parallel collection, executor start)
- Manager planning, the Executor stream and the Auditor now all honor STOP
- Cancellation propagates to workers on STOP — no more orphaned sub-agent processes
- Repaired artifact-path auditing, `cmd_format` false positives, the anti-stagnation gate, and skill-injection truncation

### Configuration

- `[orchestration.*]` keys move to `[modes.instant.*]`. The legacy section still loads through a fallback and is normalized on validation; existing `config.toml` files keep working unchanged
- `max_tokens_per_run` is clamped to `max_total_tokens` when it exceeds it, with a warning

### Upstream changes included in this release

- `feat(shell)`: literal shell deletes in `shell_exec` are redirected to the OS Recycle Bin (recoverable deletion)

### Verification (measured on the build host, Windows 11 23H2)

- `cargo build --release` — success in 9m44s, warnings only; `FoxIR.exe` 38.72 MB
- `cargo test --release --no-fail-fast` — lib 279 passed / 1 failed, bin 304 passed / 1 failed, integration suites and doc-tests green. The single failure is the pre-existing, machine-dependent `forensics::live_tests::test_shimcache` (see Known Issue); `src/forensics/` is byte-identical to `v1.0.5`, so this is not a regression from this release
- `ir_process` smoke test — the embedded enumeration script parses with 0 syntax errors, exits 0, emits 254 520 bytes and 460 process records (`"pid":` fields), versus zero output before the fix

### Known Issue

- `ShimCache` parsing fails on Windows 11 23H2: `parse_shimcache` only accepts the `0x00000080` (Win10/11) and `0xBADC0FEE` (Win7) headers, but this build's `AppCompatCache` blob starts directly with per-entry data (first `u32` = `0x00000034`), so the parser returns `Unknown ShimCache signature: 0x00000034`. The `test_shimcache` live test unwraps that error and therefore fails on affected hosts. Tracked for a follow-up release; the unit-level parsers for other artifacts are unaffected.
