# Glass — Architecture

*From spec to system: a blueprint for building Glass.*

This document covers **implementation decisions** — the how and why of building Glass. For behavior, capabilities, security model, and design principles, see [SPEC-v2.md](SPEC-v2.md). Detailed code samples for each component live in the [`plans/`](plans/) directory.

---

## Table of Contents

1. [Overview](#overview)
2. [Crate & Module Structure](#crate--module-structure)
3. [Core Types](#core-types)
4. [Implementation Decisions](#implementation-decisions)
5. [Data Flow](#data-flow)
6. [Boundary Traits & Testability](#boundary-traits--testability)
7. [Error Handling](#error-handling)
8. [Build Order](#build-order)
9. [Open Questions](#open-questions)

---

## Overview

Glass is a single Rust binary with two entry points:

- **`glass`** (default) — the bot: Discord gateway, scheduler, context assembly, agent orchestration, audit logging, inbox review, sandbox management.
- **`glass serve-mcp`** — the MCP tool server, spawned by Claude Code as a subprocess. Shares code with the main binary but has its own entry path.

The bot invokes Claude Code (via `claude-sdk-rs`) as the agent runtime. Claude Code handles the agentic loop, context window management, and tool calling. Glass provides tools via MCP and enforces all security at the tool boundary.

### Key Architectural Invariants

These must hold at all times. Any violation is a bug:

1. **The agent never has direct network access.** Shell execution happens in `--network none` Docker containers. Web access is executed by the bot on the host via MCP tool handlers. Claude Code's built-in tools are disabled via `disallowed_tools` blocklist — all tool execution goes through Glass's MCP server. (Validated in [`glass-spike`](glass-spike/FINDINGS.md), Test 2.)
2. **Channel capabilities are enforced by the bot at MCP registration time.** The agent cannot invoke a tool it was not given.
3. **Project isolation is structural.** Other project data is not in the LLM context.
4. **The harness directory is invisible to the agent.** Never mounted, never referenced in any path the agent can access.
5. **The workspace is a git repo.** Every agent modification is trackable and reversible.
6. **Glass defines the system prompt.** The bot assembles it and passes it to Claude Code. Glass does not use CLAUDE.md files.

---

## Crate & Module Structure

Glass is a single Rust binary crate. No workspace, no sub-crates. Modules start as single files and grow into directories only when the code demands it.

```
glass/
├── Cargo.toml
├── .env.example
├── Dockerfile.sandbox
├── SPEC-v2.md
├── ARCHITECTURE.md
├── plans/                          # Detailed implementation plans per component
│
├── src/
│   ├── main.rs                     # Entry point: load config, start bot + scheduler
│   ├── config.rs                   # Environment & TOML config loading
│   ├── traits.rs                   # Boundary traits: AgentRuntime, DiscordSink, Sandbox
│   │
│   ├── discord/
│   │   ├── mod.rs                  # Re-exports, Discord client setup
│   │   ├── handler.rs              # EventHandler impl (message, interaction, ready)
│   │   ├── components.rs           # Button/interaction builders (inbox, domain approval)
│   │   └── channels.rs            # Channel ↔ project resolution
│   │
│   ├── agent/
│   │   ├── mod.rs
│   │   ├── runtime.rs              # ClaudeCodeRuntime: wraps claude-sdk-rs, implements AgentRuntime
│   │   ├── invocation.rs           # InvocationContext construction, MCP config generation
│   │   └── subagent.rs             # query_projects two-phase dispatch
│   │
│   ├── mcp/
│   │   ├── mod.rs
│   │   ├── server.rs               # MCP server entry point (`glass serve-mcp`)
│   │   ├── tools.rs                # MCP tool definitions and dispatch
│   │   └── scoping.rs              # Path scoping, capability enforcement at tool boundary
│   │
│   ├── context/
│   │   ├── mod.rs
│   │   ├── assembly.rs             # Build system prompt for each invocation type
│   │   └── identity.rs             # Load identity.md and skill metadata
│   │
│   ├── sandbox/
│   │   ├── mod.rs
│   │   ├── docker.rs               # Docker container lifecycle (create, exec, destroy)
│   │   └── volume.rs               # Workspace volume mounting logic
│   │
│   ├── capabilities.rs             # Parse channel_capabilities.toml, tool filtering, domain allowlist
│   ├── projects.rs                 # ProjectRegistry: scan workspace, metadata, lifecycle ops
│   ├── scheduler.rs                # Cron loop: parse schedule.json, fire tasks via mpsc
│   ├── audit.rs                    # Write timestamped JSON audit files, post to #audit-log
│   ├── inbox.rs                    # Pending queue, Discord review UI, approval pipeline
│   ├── skills.rs                   # Scan SKILL.md YAML frontmatter from workspace/skills/
│   └── git.rs                      # Auto-commit after agent invocations, ensure_git_repo
│
└── tests/
    ├── integration/                # Full invocation tests with mocks
    ├── helpers/
    │   ├── mock_runtime.rs         # MockAgentRuntime
    │   ├── mock_discord.rs         # MockDiscordSink
    │   ├── mock_sandbox.rs         # MockSandbox
    │   └── fixtures.rs             # TestFixtures: wires everything together
    └── fixtures/                   # On-disk fixture layouts (temp directories)
```

**Why this shape:**

- **Directories for the big four.** Discord, agent, MCP, and context have enough internal structure to justify directories from the start. Everything else starts as a single file.
- **Claude Code is a boundary, not an internal.** `agent/runtime.rs` wraps `claude-sdk-rs` behind the `AgentRuntime` trait. If we replace Claude Code, only this file changes.
- **Traits at the boundary, not everywhere.** Three traits cover the three external seams. Everything inside is concrete.
- **Test helpers are first-class code.** If it's hard to test, the design is wrong.

---

## Core Types

The core data flow is: the bot builds an **`InvocationContext`** (project, trigger, system prompt, allowed tools) and hands it to the **`AgentRuntime`**. The runtime returns an **`InvocationResult`** (response text, tool calls, tokens, cost). Post-invocation, the bot writes an **`AuditEntry`** combining the context and result with timing data.

Full struct definitions with doc comments are in [`plans/core-types.md`](plans/core-types.md).

| Type | Purpose |
|------|---------|
| `Project` | A discovered project — name, workspace path, channel ID, root/archived flags |
| `InvocationTrigger` | Enum: `UserMessage { user_id, channel_id, content }` or `ScheduledTask { task_id, cron, description }` |
| `NetworkCapability` | Enum: `None`, `Search`, `Allowlist`, `Open` |
| `ChannelConfig` | Network tier + allowed domains + integrations for a project |
| `ToolCallRecord` | Individual tool call — tool name, args, result, duration |
| `ScheduledTask` | A cron task — id, expression, description, enabled flag |
| `DiscoveredSkill` | Skill metadata (name + description) parsed from SKILL.md frontmatter |
| `GlassConfig` | All configuration — Discord tokens, API keys, paths, model name |
| `GlassError` | Unified error enum with `#[from]` conversions for each module's error type |

Glass does not define Claude API wire types. Those are handled internally by `claude-sdk-rs`.

---

## Implementation Decisions

These are the choices that aren't obvious from the spec — the things you'd need to know to build this.

### MCP Per-Invocation Parameterization

The MCP server process is launched by Claude Code, but it needs to know which project it's serving and what capabilities apply. The bot controls this by writing a **per-invocation MCP config tempfile** that specifies the `glass serve-mcp` command with CLI arguments: `--project`, `--workspace-root`, `--capability-tier`, `--allowed-domains`. (Validated in [`glass-spike`](glass-spike/FINDINGS.md), Test 1.)

Claude Code spawns the MCP server from a config the bot wrote, so the bot controls the security parameters without needing shared state between processes. The MCP server reads its arguments at startup and enforces them for the lifetime of the session.

Every invocation also sets:
- **`disallowed_tools`** — explicit blocklist of all Claude Code built-in tools (see [Defense in Depth](#defense-in-depth-at-the-tool-boundary)).
- **`stream_format: StreamJson`** — required to get tool call records in session results for audit logging and `query_projects` detection. (Validated in [`glass-spike`](glass-spike/FINDINGS.md), Test 3.)
- **`skip_permissions: true`** — required for non-interactive use; without it Claude Code hangs waiting for permission prompts.

### Path Scoping via Lexical Normalization

Every `read_file`/`write_file`/`list_files` call verifies the normalized path is under the allowed workspace root. We use **lexical normalization** (resolve `..` and `.` in the string) rather than `std::fs::canonicalize`, because canonicalize follows symlinks and fails on paths that don't exist yet. A `write_file` to a new path needs to be validated before the file exists.

### Defense in Depth at the Tool Boundary

Tools are filtered at three independent points:

1. **Built-in tool blocklist** — `disallowed_tools` explicitly blocks all Claude Code built-in tools. This is required because `allowed_tools` does **not** prevent Claude Code from using its own built-ins — it only controls which MCP tools are permitted. (Validated in [`glass-spike`](glass-spike/FINDINGS.md), Test 2.) The complete list discovered by the spike: `Read`, `Edit`, `Write`, `MultiEdit`, `NotebookEdit`, `Bash`, `Glob`, `Grep`, `LS`, `WebFetch`, `WebSearch`, `Task`, `TaskOutput`, `TaskStop`, `TodoRead`, `TodoWrite`, `EnterPlanMode`, `ExitPlanMode`, `AskUserQuestion`, `Skill`.
2. **MCP tool registration** — only Glass's tools are registered via the per-invocation MCP config. Claude Code can only call tools that appear in `tools/list`.
3. **Runtime capability checks** within each MCP handler — even if a tool is registered, the handler validates capabilities before executing.

All three must pass. This means a bug in registration logic doesn't automatically become a security hole, and the explicit blocklist ensures Claude Code's own tools cannot bypass Glass's security boundary.

**Maintenance note:** If future Claude Code versions add new built-in tools, they must be added to the blocklist. Glass already controls which Claude Code version is installed, so this is a versioned dependency, not an open-ended risk.

### Docker via CLI, Not Bollard

The sandbox uses Docker CLI via `tokio::process::Command` — not the `bollard` crate. We only need three operations (create, exec, destroy), and shelling out is simpler than maintaining an async Docker API client for that.

Container flags: `--network none`, `--cap-drop ALL`, `--read-only`, `--memory 512m`, `--cpus 1.0`, `--pids-limit 256`, non-root user. Image is Ubuntu 24.04 with python3, node, jq, ripgrep, git, and common text tools. Under 500MB.

### Conversation History

Glass fetches recent Discord messages from the project's channel and includes them in the context passed to Claude Code. This gives the agent conversational continuity without relying on Claude Code session persistence — each invocation is still a fresh Claude Code session, but it can see what was said recently.

The workspace memory mechanisms (`status.md`, `notes.md`, etc.) coexist with conversation history. Discord messages provide short-term conversational flow; workspace files provide long-term persistent memory that the agent explicitly maintains. The two serve different purposes and both are needed.

A `get_channel_history` MCP tool is available for the agent to fetch older messages beyond the initial window when it needs more context. This keeps the default context lean while allowing the agent to reach back when a conversation references something from earlier.

### Dual-Path File Access

The workspace is accessible via two independent paths, and **both must be scoped identically:**

- **MCP tools** (`read_file`, `write_file`, `list_files`) — execute on the host via the MCP server, scoped by lexical path normalization.
- **Shell commands** (`shell("cat notes.md")`, `shell("python script.py > output.txt")`) — execute inside the Docker container, scoped by what's mounted into the container.

Both paths touch the same underlying files. The critical invariant is that **the Docker mount and the MCP path scope must agree on what's accessible.** For a project invocation, Docker mounts only `workspace/surgery-prep/` and the MCP server path-scopes to the same directory. For root+owner, Docker mounts all of `workspace/` and the MCP server allows the full workspace. If these ever disagree, the agent can use `shell` to reach files that `read_file` would deny (or vice versa).

The bot configures both scopes from the same source (`InvocationContext.workspace_root`) so they can't drift independently.

### Scheduler Design

The cron loop runs as a `tokio::spawn` background task with a 30-second check interval. It **reloads schedule.json on each tick** because the agent may modify schedules during invocations. Tasks fire via an `mpsc` channel to decouple scheduling from invocation. Uses the `cron` crate for expression parsing.

### Audit Data from Session Results

Tool call records (tool name, args, result, duration) are extracted from the completed Claude Code session result via `claude-sdk-rs`. The bot builds the full `AuditEntry` by combining session metadata (`InvocationContext`) with the tool call records and timing data from the session result. No side-channel or tempfile needed — the data flows through the same path the bot already uses to get the agent's response.

### Skill Discovery: Progressive Disclosure

Only YAML frontmatter metadata (~100 tokens/skill) goes into the system prompt. The agent reads full skill files via `read_file` when it decides to use one. This keeps context cost flat as skills accumulate.

### Git Auto-Commit

Best-effort — failures are logged, not fatal. Commit message format: `[glass] {project} — {trigger}`. Runs `ensure_git_repo()` at startup.

### Channel ↔ Project Resolution

`ProjectRegistry` scans the workspace at startup. A directory is a project if it contains `brief.md`. Channel IDs are resolved by matching Discord channel names to project directory names. The registry maintains a `HashMap<ChannelId, String>` for fast lookup.

### Domain Approval Flow

When `fetch_url` is denied by the allowlist, the MCP server returns an error to Claude Code. Separately, the bot posts an Approve/Reject button message to Discord. On approval, the bot appends the domain to `channel_capabilities.toml`. This is deterministic bot code — no LLM in the approval pipeline.

---

## Data Flow

### User Message → Agent Response

```
User types in #surgery-prep
       │
       ▼
┌─────────────────────────────────────────────────────────────┐
│  Discord Gateway                                            │
│  Resolve channel → project, spawn invocation task           │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Context Assembly                                           │
│  Load identity.md, capabilities, skills, project brief/     │
│  status, workspace listing → build InvocationContext        │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Agent Runtime (ClaudeCodeRuntime via claude-sdk-rs)         │
│  Generate MCP config tempfile → spawn Claude Code session   │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Claude Code (agentic loop)                                 │
│                                                             │
│  Process prompt → need tool? ──Yes──► MCP call to Glass     │
│       │                                    │                │
│      No                                    ▼                │
│    (done!)                   Glass MCP Server executes:     │
│                              • shell → Docker sandbox       │
│                              • read/write/list → host fs    │
│                              • fetch_url → host reqwest     │
│                              • web_search → Brave API       │
│                              • suggest_learning → pending/  │
│                                    │                        │
│                              Tool result returned ──────►   │
│                              (loop continues)               │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│  Post-Invocation                                            │
│  1. Send response to Discord channel                        │
│  2. Destroy sandbox container                               │
│  3. Extract tool call records from session result → write audit JSON │
│  4. Auto-git commit workspace changes                       │
│  5. If autonomous: post summary to #audit-log               │
│  6. If inbox suggestions: post review messages to Discord   │
└─────────────────────────────────────────────────────────────┘
```

### Scheduled Task

Same flow as above, except:
- Trigger is `ScheduledTask` instead of `UserMessage`
- Context uses autonomous variant (task description as prompt)
- Post-invocation always posts summary to `#audit-log`

### query_projects: Two-Phase Dispatch

This is the canonical description of the two-phase pattern. Other sections reference this flow but do not repeat it.

When a root autonomous invocation needs information from multiple projects, it uses `query_projects` to coordinate without violating project isolation:

```
Root scheduled task fires (e.g., "morning-briefing")
       │
       ▼
Phase 1: Bot spawns root Claude Code session.
         Agent sees project list in system prompt, decides to call
         query_projects(projects: ["surgery-prep", "side-project"],
                        query: "What's your current status and any blockers?")
         MCP tool handler returns: "Queries dispatched. This session
         will end and a new session will resume with the results."
         Claude Code, with no further actionable step, wraps up.
       │
       ▼
Bot reads completed session's tool call records.
Sees query_projects was called → initiates subagent dispatch.
       │
       ▼
Subagent dispatch: for each queried project, bot spawns an
  independent Claude Code session with that project's context.
  Each session receives the uniform query string as its prompt.
  Each runs with that project's channel capabilities.
       │
       ▼
Phase 2: Bot spawns a NEW root session with:
  - Original system prompt + task description
  - Collected responses from each project
  - No tools registered (empty toolset)

  The agent synthesizes a cross-project response as plain text.
       │
       ▼
Bot posts synthesized response to #glass and #audit-log.
```

**Why two phases?** The tool swap between Phase 1 and Phase 2 is a **structural security property**. After subagent responses are collected, the agent holds cross-project data. Phase 2 registers **no tools** — no `write_file`, no `fetch_url`, no `shell`. Claude Code can only produce a text response. This is enforced by starting an entirely new Claude Code session with an empty toolset, not by prompting.

**Detection mechanism:** No side-channel needed. The bot already reads the completed session's tool call records (for audit logging). If `query_projects` appears in those records, the bot extracts the project list and query string and initiates the subagent dispatch. The session-completion boundary is the signal.

---

## Boundary Traits & Testability

Full code samples for traits, mocks, and fixtures are in [`plans/testability.md`](plans/testability.md).

### Principle

> **Mock the boundary, not the logic.** Tests use the real tool executor, real path scoping, real capability filter. The only fakes are what's on the other side of the process or kernel boundary.

### Three Boundary Traits (in `src/traits.rs`)

| Trait | Production Impl | Test Mock | What It Abstracts |
|-------|----------------|-----------|-------------------|
| `AgentRuntime` | `ClaudeCodeRuntime` | `MockAgentRuntime` | Claude Code subprocess |
| `DiscordSink` | `SerenityDiscord` | `MockDiscordSink` | Discord message delivery |
| `Sandbox` | `DockerSandbox` | `MockSandbox` | Docker container exec |

Traits are injected at construction time and flow downward. No module reaches out to a global or constructs its own I/O.

**No Filesystem trait.** File I/O uses the real filesystem in both production and tests. Tests use temp directories (`tempfile` crate) that are automatically cleaned up. This gives better test fidelity than an in-memory mock — path scoping, permissions, and directory listing all exercise real OS behavior. The cost is negligible (temp dir operations are fast), and the benefit is one fewer abstraction to maintain.

`TestFixtures` struct wires all mocks + temp directories into a ready-to-use harness. Goal: `cargo test` runs the full suite in seconds with zero external dependencies. Tests requiring Claude Code or Docker are `#[ignore]`.

---

## Error Handling

Glass uses `thiserror` for structured errors. Each module has its own error type; a top-level `GlassError` unifies them with `#[from]` conversions. Full error types are in [`plans/core-types.md`](plans/core-types.md).

| Error Type | Recovery Strategy |
|---|---|
| Claude Code binary not found | Fail with clear install instructions |
| Claude Code runtime/timeout | Report to Discord channel, log to audit |
| MCP tool execution error | Return error via MCP protocol; Claude Code sees it and can retry |
| Sandbox creation/timeout | Report to channel, skip invocation |
| Path traversal attempt | Reject tool call, log security event |
| Domain not allowed | Return error via MCP, optionally trigger domain request flow |
| Config parse error | Fail startup with clear message |
| Git commit failure | Log warning, continue (best-effort) |
| Discord API failure | Log error, retry once |

**Logging:** `tracing` crate with structured fields. ERROR for invocation failures and security violations, WARN for retries and best-effort failures, INFO for invocation lifecycle, DEBUG for full request/response details.

---

## Build Order

Build follows dependency chains. Each phase produces a working, testable artifact. Detailed phase plan with checklists is in [`plans/build-phases.md`](plans/build-phases.md).

**Phase 0 (spike): validate `claude-sdk-rs`** — ✅ **COMPLETE.** See [Runtime Validation Spike](#runtime-validation-spike) below and full results in [`glass-spike/FINDINGS.md`](glass-spike/FINDINGS.md).

**Phase 1+: config → Discord → MCP/sandbox → agent runtime → projects/context → capabilities → scheduling → audit → inbox → query_projects → domain/git → polish**

The critical path is config through agent runtime: once a message in Discord triggers a Claude Code invocation and a response appears, everything else layers on. Capabilities and scheduling are independent of each other and can be parallelized.

### Runtime Validation Spike — ✅ Complete

The spike lives in [`glass-spike/`](glass-spike/) with full results in [`glass-spike/FINDINGS.md`](glass-spike/FINDINGS.md). All four tests pass:

| # | Test | Result | Workaround |
|---|------|--------|------------|
| 1 | MCP config passthrough | ✅ PASS | None needed |
| 2 | Blocking built-in tools | ✅ PASS | Must use `disallowed_tools` (explicit blocklist), not `allowed_tools` — Claude Code's `--allowedTools` is additive and does not restrict built-ins |
| 3 | Tool call records in session results | ✅ PASS | Must use `StreamFormat::StreamJson`; tool calls appear as `tool_use` blocks in `raw_json` |
| 4 | System prompt length (>10K) | ✅ PASS | Use `Client::new(config)` directly — SDK's `validate()` rejects >10K chars, but Claude Code itself accepts them fine |

**Additional findings:**
- The published crate on crates.io (v1.0.2) is behind GitHub `main` and is missing `disallowed_tools`, `skip_permissions`, and `max_turns`. **Use the git dependency** until a new version is published.
- The crate's `mcp` feature flag doesn't compile (broken module paths). Glass doesn't need it — Glass provides its own MCP server binary.
- `skip_permissions: true` is required on every invocation or Claude Code hangs waiting for permission prompts.

These findings are already reflected in the [MCP Per-Invocation Parameterization](#mcp-per-invocation-parameterization) and [Defense in Depth](#defense-in-depth-at-the-tool-boundary) sections above.

### Key Dependencies

| Crate | Why this one over alternatives |
|-------|-------------------------------|
| `claude-sdk-rs` | Rust-native Claude Code wrapper; `AgentRuntime` trait allows replacement if insufficient. **Pin to git main** — published crate is missing `disallowed_tools` and `skip_permissions`. |
| `serenity` 0.12 | Mature, well-documented async Discord framework. No `poise` — messages-first, not slash commands |
| `reqwest` | Standard Rust HTTP client for host-side `fetch_url`/`web_search` |
| `serde_yml` | YAML parsing for SKILL.md frontmatter; `serde_yaml` is deprecated |
| `dotenvy` | `.env` loading; `dotenv` is unmaintained |
| `cron` | Lightweight cron expression parsing, does one thing |

**What's not here:** No `bollard` (Docker CLI is simpler). No `sqlx`/`diesel` (JSON files on disk). No `async-openai` (Claude Code handles the API).

---

## Open Questions

1. **Concurrent invocations.** Each invocation spawns its own Claude Code subprocess and Docker sandbox — should work, but the audit logger needs atomic writes (temp-file-then-rename).

2. **Container reuse vs. per-invocation.** Start with per-invocation (simpler, no state leakage). Pool if latency is a problem.

3. **Identity bootstrapping.** Ship a default `identity.md` or let the agent create its own on first run?

### Future Considerations (Post-MVP)

- Streaming responses to Discord
- Embedding-based memory / semantic search
- Session playback visualizer
- Integration tools (Linear, Google Calendar)
- Custom agentic loop (replace Claude Code via `AgentRuntime` trait)
- Container pooling, workspace snapshots, rate limiting

---

## Summary

One binary, flat modules, JSON files on disk, no databases. Every security property is structural — enforced by code, containers, and MCP tool boundaries rather than by instructions to the LLM.

The key decision is using Claude Code (via `claude-sdk-rs`) as the agent runtime behind an `AgentRuntime` trait. Glass provides tools via an MCP server that enforces all security. The trait boundary ensures Claude Code can be replaced if needed.

For the full behavior spec, security model, directory structure, and design principles, see [SPEC-v2.md](SPEC-v2.md). For detailed code samples, see the [`plans/`](plans/) directory.