**[中文](README.md)** | **[English](#)**

# FoxIR

An AI-assisted platform for local IT systems engineers — focused on system analysis, log investigation, incident response, and remote operations. **FoxIR is the successor to the RustAgent project.** It runs fully locally as a single binary with a WebSocket gateway, multi-model support, two-tier memory, dynamic SOP replay, a bounded context budget, 40+ built-in tools, permission gates, task scheduling, and remote channels (WinRM / Linux SSH). Built for Windows, works out of the box.

## Positioning

FoxIR is designed for local IT systems engineers to tackle the three most time-consuming jobs of daily operations: **system state analysis**, **log investigation & attribution**, and **security incident response** — extended to **remote Windows (WinRM)** and **remote Linux (SSH)** for troubleshooting and incident response.

Traditionally, an engineer switches between many tools — Event Viewer for logs, PowerShell for processes, the registry editor for config, netstat for connections — and manually correlates each clue. FoxIR unifies these capabilities into one AI agent: the engineer describes a problem in natural language, and the agent orchestrates the toolchain, collects system state, searches relevant logs, correlates anomalies, and finally produces a structured conclusion and remediation advice.

**Typical scenarios:**

- **Log investigation**: "List errors and warnings in the last 24h system log, sorted by timeline" — the agent calls event-log tools, filters by severity, sorts by time, correlates processes and service state
- **System analysis**: "Which anomalous processes are consuming resources? Check their launch origin" — the agent chains process enumeration + resource analysis + Autoruns persistence detection into a full process-chain report
- **Security triage**: "Check whether this machine has a persistence backdoor" — the agent chains registry audit, scheduled-task enumeration, service enumeration, and Autoruns detection into a full persistence-attack-surface report
- **Remote response**: "Use WinRM to reach 192.168.x.x for attribution" / "SSH to an internal host and check for a miner process" — the agent runs commands and forensics on remote hosts through the built-in WinRM / Linux SSH channels
- **Fault diagnosis**: "Service X failed to start, find the cause" — the agent checks service state, correlates event logs, examines dependencies and config, and locates the root cause

**Why fully local**: the logs, process info, and registry data an IT engineer handles often contain sensitive intranet topology and credentials. FoxIR's AI dialog engine, tool execution, and data storage all stay local; API keys are AES-256-GCM encrypted; only LLM inference requests go to the cloud model — raw system data never leaves the machine.

A single Rust artifact (`FoxIR.exe`, ~30MB+) bundles the AI dialog engine, tool-execution layer, WebSocket gateway, and Web Dashboard — no extra runtime or external service dependencies. Inspired by Google ADK's Agent → LlmAgent → EventStream architecture, it realizes a complete Agentic Loop in the Rust ecosystem.

## High-Level Architecture

```
┌──────────────────────────────────────────────────┐
│                    Dashboard SPA                 │
│  (Chat / Dashboard / Skills / Tools / Knowledge ) │
│  (CRON / MCP / Settings · Context Budget view)   │
└───────────────────────┬──────────────────────────┘
                        │ WebSocket / HTTP
┌───────────────────────┴──────────────────────────┐
│               Axum 0.8 Server                     │
│        (REST API + WS Gateway + SSE)             │
├──────────────────────────────────────────────────┤
│  Runner → LlmAgent (Agentic Loop)                │
│    ├── Agent trait → EventStream                 │
│    ├── Bounded Context Budget (value-ranked)     │
│    ├── Session recall / continuation             │
│    ├── Checkpoint crash recovery + JSON repair   │
│    └── Re-prompt detection & self-healing        │
├──────────────────────────────────────────────────┤
│  Tool Layer                                      │
│    ├── 40+ built-in tools (Windows / Linux IR)   │
│    ├── WinRM (remote Windows) / Linux SSH        │
│    ├── MCP Client (stdio + SSE)                  │
│    ├── Skill Manager (hot injection + contracts) │
│    └── External Tools (workspace/tools/)         │
├──────────────────────────────────────────────────┤
│  Memory & Learning                               │
│    ├── Deep memory: deep_facts (single source)   │
│    ├── Auto memory memory.db (SQLite+FTS5+BM25)  │
│    ├── Knowledge (routing.json + experience.md)  │
│    ├── SOP (dynamic multi-step flows)            │
│    └── Unified artifact value V(a,t)=Q×R×U       │
├──────────────────────────────────────────────────┤
│  Infrastructure                                  │
│    ├── Permission (category gates + intent)      │
│    ├── Intent Policy (Block / Audit / Pass)      │
│    ├── Scheduler (CRON + interval) + Heartbeat   │
│    ├── Security (AES-256-GCM + recycle-bin-safe) │
│    └── First-run embedded workspace extraction   │
└──────────────────────────────────────────────────┘
```

## Core Capabilities

### Permission System

FoxIR implements a two-layer security model of **category-based gates** + **intent policy**:

- **Five permission categories**: read / write / delete / modify / execute; every tool call declares its required category
- **Async user authorization**: high-privilege requests push an authorization prompt to the Dashboard over WebSocket; the agent loop waits without blocking
- **Command intent policy engine**: semantically parses shell_exec / winrm / linux_ssh commands (verb + targets) into three verdicts:
  - **Block** (absolute): irreversible ops (disk format, security-log clear, encoded commands) — hard-intercepted regardless of auth state
  - **Audit** (log and proceed): legitimate but high-risk ops (file delete, process kill, service stop)
  - **Pass** (silent): normal read-only ops
- **Cross-category bypass detection**: when shell_exec is pre-approved (execute:true) but a command's intent maps to a denied category (e.g. delete:false), access is escalated for confirmation — preventing the LLM from bypassing file_delete permissions through shell_exec
- **Recycle-bin-safe deletes**: `file_delete` and simple literal-path shell deletes (`Remove-Item` / `del`) move to the OS Recycle Bin by default instead of hard-deleting data
- **Strong permission-denial feedback**: denials return a strongly-worded message forbidding alternative-tool workarounds

### Memory System (Two-Tier)

**Deep Memory**: persistent SQLite layer managed by the `deep_memory` tool (remember / recall / list / forget / update). User-stated facts are pinned (never auto-forgotten); the rest is ranked by the unified value function V(a,t)=Q×R×U, annealed over time, and packed into the context budget.

**Auto memory (memory.db)**: every turn is persisted automatically; recent summaries are injected as [Memory Context] / [Memory Recall]; CJK bigram tokenization + BM25 full-text search; daily auto-summaries.

**MEMORY.md**: a read-only projection of deep memory auto-generated from `deep_facts`; not edited directly — manage facts with the `deep_memory` tool.

**Knowledge base**: `knowledge/routing.json` routes a request to the right document; `experience.md` accumulates all distilled experience (facts / lessons / decisions / tips — no longer split into many files); additional methodology / playbook / process documents can be attached and pre-retrieved per-turn.

### Scheduling

- **CRON expressions**: standard 5-field, timezone-aware
- **Interval syntax**: `every 5m`, `every 2h`, etc.
- **JSON persistence**: `cron_tasks.json`, survives restarts
- **Isolated execution**: due tasks run in their own sessions without blocking the main one
- **Heartbeat**: togglable from Settings; periodic memory maintenance and health checks, notifying only on anomalies

### Tools

40+ built-in tools span daily system checks to deep security analysis:

**File ops** (5): FileRead / FileWrite / FileDelete (to Recycle Bin) / FileModify / FileList

**Execution channels**: ShellExec (PowerShell/CMD, intent-policy guarded), LinuxSSH (remote Linux/bash), WinRM (remote Windows/PowerShell, NTLM/Basic + HTTP/5985 & HTTPS/5986)

**Incident-response toolset** (Windows IR + Linux IR): process analysis, network connections, registry audit, service enumeration, scheduled tasks, VSS/USN, event logs, EVTX parsing, memory forensics, persistence detection, attack-path, timeline, pcap traffic analysis, EML sample analysis, scanning, case management, report generation, etc.

**Malware analysis**: YARA (boreal) + PE static scanning (goblin / iced-x86)

**Remote Linux IR**: themed tools for auth, backdoors, brute-force, miners, persistence, files, rootkits, lateral movement, web, etc.

**Memory & knowledge**: deep_memory, memory_md (MEMORY.md projection/fallback), knowledge_search / knowledge_ingest

**Others**: browser_cdp (browser automation), MCP client (stdio+SSE), cron_manage, todo_update (task ledger), evidence (evidence ledger), external tools (workspace/tools/)

**SOP (dynamic flows)**: successful multi-step operations (e.g. "deploy an app", "IR stage") are distilled into structured, reusable flows (phases + verification steps); on similar tasks they are replayed and optimized by tag matching with almost no extra tokens.

**Bounded Context Budget**: block-level value-rank assembly that fits System / Tools / Memory / Knowledge / SOP / History into a budget via the unified artifact value V(a,t)=Q(a)×R(a,t)×U(a), keeping the most valuable information in the window; the Dashboard shows a live budget view and measured usage.

**Expert mode**: Manager–Executor–Auditor role separation, TaskContract persistence (SQLite, crash-recoverable), auto-generated HTML audit report into `workspace/Expert/`, and Blackboard sharing to the main session. Suited to 15+ round full IR investigations.

### Session & Continuation

- Every turn is persisted to memory.db; session recall (`build_session_recall_block`) rebuilds a [session review] for continuation after a disconnect
- Queued interjections enter the execution queue after a user STOP instead of being lost
- Checkpoint crash recovery: replay history and resume after a crash

### Web Dashboard

- **Chat**: multi-model conversations, interject, /skill commands, send/replay
- **Dashboard**: Context Budget view + usage stats
- **Skills / Tools / Knowledge / CRON / MCP / Settings**: skill management, tool listing, knowledge base, task scheduling, MCP servers, settings

## Tech Stack

| Component | Choice |
|-----------|--------|
| Runtime | Tokio (full features) |
| HTTP/WS | Axum 0.8 |
| LLM protocol | OpenAI-compatible streaming |
| Database | SQLite (rusqlite bundled) + FTS5 |
| Remote | winrm-rs (WinRM), ssh (Linux SSH) |
| Recycle bin | trash (safe delete) |
| MCP | rmcp (stdio + SSE) |
| Browser | chromiumoxide (CDP) |
| Crypto | aes-gcm (AES-256-GCM) |
| YARA | boreal (rule scanning) |
| PE parsing | goblin + iced-x86 (disassembly) |
| Serialization | serde + serde_json + serde_yaml + toml |
| Log analysis | regex + evtx |
| Traffic analysis | pcap-parser (offline pcap/pcapng) |
| Logging | tracing + tracing-subscriber (env-filter) |

## Configuration

Runtime workspace: `%USERPROFILE%\.RustAgent\workspace\` (data dir carried over from the legacy RustAgent) — note: only the default on-disk path retains the old name.

```
workspace/
├── config.toml          # main config (Server / Agent / Model)
├── models.json          # model config (encrypted API keys)
├── mcp_servers.json     # MCP server config
├── cron_tasks.json      # scheduled task definitions
├── .password            # Dashboard access password
├── memory/
│   └── memory.db        # SQLite auto memory (+ deep_facts deep memory)
├── knowledge/           # routing.json + experience.md + methodology docs
├── skills/              # skills directory
├── tools/               # external tools directory
├── Expert/              # Expert-mode HTML audit reports
├── logs/                # JSONL conversation logs
├── static/              # Dashboard static assets
└── output/              # tool outputs (screenshots / reports)
```

## Build & Run

```bash
# build release (LTO + strip)
cargo build --release

# artifact
target/release/FoxIR.exe

# first run auto-creates the workspace structure and extracts embedded files
.\target\release\FoxIR.exe
```

Release profile: `opt-level = 3`, `lto = true`, `strip = true` for a minimal binary and best runtime performance.

## Project Layout

```
src/
├── main.rs              # entry: workspace init, DI, server startup
├── server.rs            # Axum HTTP/WS server, REST API, SSE, WS channels
├── config.rs            # TOML config loading
├── runner.rs            # session management, agent scheduling
├── context_arbiter.rs   # bounded Context Budget arbiter (value-ranked)
├── deep_memory.rs       # deep memory (SQLite deep_facts)
├── memory.rs            # MemoryStore (memory.db + FTS5 + BM25)
├── sop.rs               # dynamic SOP (distill / match / replay / stats)
├── knowledge.rs         # Knowledge (routing.json routing + experience)
├── value.rs             # unified artifact value V=Q×R×U
├── interject.rs         # interject queue (queue / inject / re-queue after STOP)
├── turn_decision.rs     # turn decision
├── permission.rs        # permission checker (gates + async auth + bypass detect)
├── policy/              # command intent policy engine (Windows + Linux)
├── agent/               # Agent trait, LlmAgent, AgentEvent
├── tool/                # 40+ built-in tools (file/shell/ir*/malware*/winrm/evidence/...)
├── managed/             # Expert mode (TaskContract / Blackboard)
├── skill/               # SkillManager (hot injection + step contracts)
├── model/               # Llm trait + OpenAI-compatible client
├── scheduler.rs         # CRON scheduler
├── heartbeat.rs         # heartbeat health checks
├── checkpoint.rs        # conversation checkpoints (crash recovery)
├── crypto.rs            # AES-256-GCM crypto
├── event_log/ forensics/ security/ web/   # event logs / forensics / security / static serving
```

## License

MIT
