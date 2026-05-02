# glass-spike — Findings

*Runtime validation spike for `claude-sdk-rs`. All four tests pass.*

---

## Summary

| # | Test | Result | Workaround needed? |
|---|------|--------|--------------------|
| 1 | MCP config passthrough | ✅ PASS | No |
| 2 | Blocking built-in tools | ✅ PASS | Yes — must use `disallowed_tools`, not `allowed_tools` |
| 3 | Tool call records in session results | ✅ PASS | Must use `StreamJson` format |
| 4 | System prompt length (>10K chars) | ✅ PASS | Must use `Client::new()`, not `builder().build()` |

**Bottom line:** `claude-sdk-rs` is viable for Glass. All four capabilities work, two require minor workarounds that don't affect the architecture.

---

## Test 1: MCP Config Passthrough — ✅ PASS

**What we tested:** Write a temporary MCP config JSON pointing to our `echo-mcp` binary. Pass it to Claude Code via `Config.mcp_config_path`. Verify Claude Code spawns the server and calls our custom tool.

**Result:** Works perfectly. Claude Code:
1. Read the MCP config tempfile
2. Spawned `echo-mcp` as a subprocess
3. Discovered the `echo` and `greet` tools via MCP `tools/list`
4. Called `echo` with the correct arguments
5. Received the result and incorporated it into the response

**Implication for Glass:** The per-invocation MCP parameterization design is validated. The bot writes a tempfile MCP config specifying `glass serve-mcp` with CLI args (`--project`, `--workspace-root`, `--capability-tier`, `--allowed-domains`), and Claude Code spawns it correctly. No changes to the architecture needed.

---

## Test 2: Blocking Built-in Tools — ✅ PASS (with `disallowed_tools`)

This test has two parts that together establish the correct approach.

### Part A: `allowed_tools` alone does NOT block built-ins

Setting `allowed_tools` to only `["mcp__glass-echo__echo"]` did NOT prevent Claude Code from using its built-in `Read` tool. Claude happily read a local file. The `--allowedTools` flag is additive — it permits additional MCP tools but does not restrict built-in tools.

**This means the original ARCHITECTURE.md assumption was wrong:**

> The agent cannot invoke a tool it was not given.

This holds for MCP tools (which must be registered) but NOT for Claude Code's own built-in tools, which are always available unless explicitly blocked.

### Part B: `disallowed_tools` DOES block built-ins

Setting `disallowed_tools` to all known Claude Code built-in tools successfully blocked them. Claude reported:

> *"I need to read the file ./Cargo.toml, but looking at the tools available to me, I only have access to: mcp\_\_glass-echo\_\_echo and mcp\_\_glass-echo\_\_greet. Neither of these tools allows me to read files. NO_READ_TOOL_AVAILABLE"*

No built-in tool `tool_use` invocations appeared in the raw JSON output.

### Complete list of Claude Code built-in tools to block

Discovered through the spike's raw JSON output (Part A lists all available tools):

```rust
const CLAUDE_BUILTIN_TOOLS: &[&str] = &[
    // File & code tools
    "Read", "Edit", "Write", "MultiEdit", "NotebookEdit",
    // Shell
    "Bash",
    // Search & navigation
    "Glob", "Grep", "LS",
    // Web
    "WebFetch", "WebSearch",
    // Task management
    "Task", "TaskOutput", "TaskStop", "TodoRead", "TodoWrite",
    // Agent flow
    "EnterPlanMode", "ExitPlanMode", "AskUserQuestion",
    // Skills
    "Skill",
];
```

### Implication for Glass

The architecture's "Defense in Depth at the Tool Boundary" needs updating. The correct three-layer model is:

1. **Explicit built-in blocklist** — `disallowed_tools` blocks all Claude Code built-in tools. The bot maintains the complete list above and passes it via `Config.disallowed_tools` for every invocation.
2. **MCP tool registration** — only Glass's tools are discoverable via the per-invocation MCP config.
3. **Runtime capability checks** within each MCP handler — even if a tool is registered, the handler validates capabilities before executing.

All three must pass.

**Maintenance note:** If Claude Code adds new built-in tools in future versions, they won't be blocked until added to the list. This is a maintenance burden but not a structural flaw — Glass already controls which Claude Code version is installed. The spike's Part A test can be re-run after upgrades to discover new built-in tools.

---

## Test 3: Tool Call Records in Session Results — ✅ PASS

**What we tested:** Ask Claude to call two MCP tools (`echo` and `greet`). After the session completes, inspect `ClaudeResponse.raw_json` for tool call records.

**Result:** Both tool calls extracted with full detail:

```
Found tool call: mcp__glass-echo__echo({"message":"audit-test"})
Found tool call: mcp__glass-echo__greet({"name":"Glass"})
```

### How it works

In `StreamJson` mode, `raw_json` is an array of message objects. Tool calls appear as `tool_use` content blocks inside `assistant` messages:

```json
{
  "type": "assistant",
  "message": {
    "content": [
      {
        "type": "tool_use",
        "name": "mcp__glass-echo__echo",
        "input": { "message": "audit-test" }
      }
    ]
  }
}
```

Each block contains:
- `name` — the full MCP tool name (e.g., `mcp__glass__query_projects`)
- `input` — the arguments as a JSON object

This is sufficient for:
- **Audit logging:** Record which tools were called with what arguments.
- **`query_projects` detection:** Scan the completed session's tool calls for `mcp__glass__query_projects`, extract the project list and query string from `input`.

### Requirement

Glass **must** use `StreamFormat::StreamJson` for all invocations. The `Json` format returns only the final text. The `Text` format returns nothing structured. Only `StreamJson` includes per-message records with tool call data.

---

## Test 4: System Prompt Length — ✅ PASS (with workaround)

**What we tested:** Construct a realistic Glass system prompt (~13.5K chars) containing identity, 30 skill metadata entries, project brief, status, tool descriptions, workspace listing, and conversation history. Test whether the SDK accepts it.

**Result:**

| Method | 10K chars | 13.5K chars |
|--------|-----------|-------------|
| `Config.validate()` | ✅ | ❌ rejects |
| `Config::builder().build()` | ✅ | ❌ rejects (calls validate) |
| `Client::new(config)` | ✅ | ✅ works |
| Claude Code itself | ✅ | ✅ works |

The 10K limit exists only in the SDK's `validate()` method. `Client::new()` does not call `validate()`. Claude Code itself accepts the prompt with no issues.

### Workaround

```rust
// DON'T — .build() calls validate() which rejects >10K
let client = Client::builder()
    .system_prompt(long_prompt)
    .build()?;  // Error

// DO — Client::new skips validation
let config = Config {
    system_prompt: Some(long_prompt),
    ..Default::default()
};
let client = Client::new(config);  // Works
```

Glass constructs `Config` directly for all invocations, never using `builder().build()`.

---

## Additional Finding: Published Crate vs. GitHub

The published `claude-sdk-rs` on crates.io (v1.0.2) is behind the GitHub `main` branch. The published version is missing fields Glass requires:

| Field | Needed for | In published 1.0.2? | In git main? |
|-------|-----------|---------------------|--------------|
| `disallowed_tools` | Blocking built-in tools (test 2) | ❌ | ✅ |
| `skip_permissions` | Non-interactive use | ❌ | ✅ |
| `max_turns` | Cost control | ❌ | ✅ |
| `security_level` | Disabling SDK input validation | ❌ | ✅ |
| `resume_session_id` | Session continuation | ❌ | ✅ |

**Requirement:** Use the git dependency until a new version is published:

```toml
claude-sdk-rs = { git = "https://github.com/bredmond1019/claude-sdk-rs", branch = "main" }
```

Also: the crate's `mcp` feature flag has broken module paths and doesn't compile. Glass doesn't need it (Glass provides its own MCP server binary), but it's a quality signal.

---

## Additional Finding: `skip_permissions` is Required

Without `skip_permissions: true` (which sends `--dangerously-skip-permissions`), Claude Code prompts for human approval on every tool call. In non-interactive mode this causes the process to hang. Glass must set this flag for all invocations.

---

## Changes Required in ARCHITECTURE.md

### 1. Update "Defense in Depth at the Tool Boundary"

Replace the two-layer model with three layers:

1. **Explicit built-in blocklist** via `disallowed_tools` — blocks all Claude Code built-in tools (`Read`, `Edit`, `Write`, `Bash`, etc.)
2. **MCP tool registration** — only Glass's tools are available via the MCP config
3. **Runtime capability checks** — MCP handlers validate capabilities before executing

Add the complete list of built-in tools to block as a reference.

### 2. Require `StreamJson` Format

All Claude Code invocations must use `StreamFormat::StreamJson` to get tool call records. Document this in the MCP Per-Invocation Parameterization section.

### 3. Bypass SDK Validation for System Prompts

Document that Glass uses `Client::new(config)` directly, not `Config::builder().build()`, due to the 10K system prompt limit in the SDK's validation.

### 4. Pin to Git Dependency

Use the git version of `claude-sdk-rs` until `disallowed_tools` and `skip_permissions` are in a published release.

### 5. Add `skip_permissions: true` to Invocation Config

Every invocation must set this or Claude Code will hang waiting for permission prompts.

---

## How to Re-run

```bash
cd glass-spike

# Build both binaries (glass-spike + echo-mcp)
cargo build

# Run all tests (requires Claude CLI + API key, costs ~$0.05)
cargo run --bin glass-spike -- all

# Run just the offline test (free, no API key)
cargo run --bin glass-spike -- 4

# Run individual live tests
cargo run --bin glass-spike -- 1   # MCP passthrough
cargo run --bin glass-spike -- 2   # Built-in tool blocking
cargo run --bin glass-spike -- 3   # Tool call records
```
