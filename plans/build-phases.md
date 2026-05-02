# Glass — Build Phases

*Detailed phase plan extracted from the architecture document. See [ARCHITECTURE.md](../ARCHITECTURE.md) for the system design.*

---

## Phase 0: Skeleton, Config & Trait Boundaries (Days 1–3)

**Goal:** Compiling binary with config loading, all three boundary traits defined, and test mock infrastructure ready.

- [ ] `cargo new glass`, set up `Cargo.toml` with all deps
- [ ] `config.rs` — load `.env`, validate required vars
- [ ] `main.rs` — load config, tracing subscriber, startup banner
- [ ] `glass-data/workspace/` and `glass-data/harness/` directory structure
- [ ] `.env.example`, `Dockerfile.sandbox`
- [ ] `src/traits.rs` — three boundary traits (`AgentRuntime`, `DiscordSink`, `Sandbox`)
- [ ] All three mock implementations in `tests/helpers/`
- [ ] `TestFixtures` struct wiring mocks + temp directories for filesystem
- [ ] Smoke tests: binary starts, mock infrastructure works

## Phase 1: Discord Bot — Alive (Days 3–5)

**Goal:** Bot connects to Discord, echoes messages. Handler logic testable via `MockDiscordSink`.

- [ ] `SerenityDiscord` impl of `DiscordSink`
- [ ] `EventHandler` with `message` and `ready`
- [ ] Channel ↔ project resolution
- [ ] Discord server setup: `#glass`, `#audit-log`, test project channel
- [ ] Mock-based handler tests

## Phase 2: MCP Tool Server & Docker Sandbox (Days 5–9)

**Goal:** `glass serve-mcp` exposes tools over MCP. Docker sandbox works. Security enforcement at boundary.

- [ ] MCP server entry point with stdin/stdout transport
- [ ] `ToolExecutor` with dispatch to all tool handlers
- [ ] Path scoping (`normalize_path`, `resolve_scoped_path`)
- [ ] File tools (direct `std::fs`), shell tool via `&dyn Sandbox`
- [ ] `suggest_learning` tool
- [ ] `DockerSandbox` implementing `Sandbox` trait
- [ ] Tool registration filtering by capability
- [ ] Path traversal tests, tool dispatch tests, sandbox integration tests (`#[ignore]`)

## Phase 3: Agent Runtime — claude-sdk-rs Integration (Days 9–12)

**Goal:** `ClaudeCodeRuntime` works end-to-end. Talk to Glass in Discord, get responses.

- [ ] `ClaudeCodeRuntime` implementing `AgentRuntime`
- [ ] Per-invocation MCP config generation
- [ ] Session resumption, error handling
- [ ] Wire into Discord handler: message → context → runtime → reply
- [ ] Mock-based tests for full invocation path

## Phase 4: Projects & Context Assembly (Days 12–16)

**Goal:** Multi-project support with correct isolation and context shapes.

- [ ] `ProjectRegistry` — discovery, channel resolution
- [ ] `assemble_system_prompt()` for all three invocation types
- [ ] Identity and skill metadata loading
- [ ] Integration tests for context correctness per type

## Phase 5: Channel Capabilities (Days 16–19)

**Goal:** Per-project network capabilities enforced at MCP boundary.

- [ ] Parse `channel_capabilities.toml`
- [ ] Tool filtering by capability tier
- [ ] Domain allowlist validation
- [ ] Defense-in-depth runtime checks in tool handlers
- [ ] Comprehensive capability tests

## Phase 6: Scheduling (Days 19–21)

**Goal:** Cron loop triggers agent invocations with correct autonomous context.

- [ ] Background cron loop with 30s interval
- [ ] Cron parsing, `schedule.json` loading (root + per-project)
- [ ] Tests for firing logic, disabled tasks, file reload

## Phase 7: Audit System (Days 21–23)

**Goal:** Every invocation logged. Autonomous actions summarized in Discord.

- [ ] `AuditLogger` — timestamped JSON files
- [ ] Discord audit channel posting for scheduled tasks
- [ ] Wire into post-invocation flow
- [ ] Tests for audit structure, conditional Discord posting

## Phase 8: Inbox System (Days 23–25)

**Goal:** `suggest_learning` → pending → Discord review → `workspace/inbox/` pipeline.

- [ ] Pending suggestion read/write in `harness/pending/`
- [ ] Discord buttons for approve/reject/edit
- [ ] Approval writes to `workspace/inbox/`, rejection deletes from pending
- [ ] Wire interaction handler for button clicks

## Phase 9: query_projects Two-Phase Dispatch (Days 25–27)

**Goal:** Root autonomous tasks can query across projects and synthesize responses.

- [ ] `dispatch_project_queries()` in `agent/subagent.rs`
- [ ] Phase 1 detection via session tool call records, subagent dispatch, Phase 2 synthesis (empty toolset)
- [ ] Tests for full two-phase flow with mocks

## Phase 10: Domain Approval & Git Sync (Days 27–29)

**Goal:** Domain allowlist requests via Discord buttons. Auto-git-commit. MVP feature-complete.

- [ ] Domain request detection and Discord approval flow
- [ ] `auto_commit()` and `ensure_git_repo()`
- [ ] End-to-end tests for both flows

## Phase 11: Polish & Hardening (Days 29–33)

**Goal:** Error handling, edge cases, documentation, deployment.

- [ ] Audit all error paths — no panics in production
- [ ] Graceful shutdown, sandbox/subprocess cleanup on crash
- [ ] Security review against spec's Security Model Summary
- [ ] Deployment docs, `docker-compose.yml`, README