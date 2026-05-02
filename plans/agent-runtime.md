# Agent Runtime — Implementation Plan

> Extracted from ARCHITECTURE.md. See that file for how this module fits into the overall system.

**Module:** `src/agent/`
**Responsibility:** Invoke Claude Code via `claude-sdk-rs`, manage the invocation lifecycle, and handle the `query_projects` two-phase pattern.

---

## Design Decision: Claude Code as the Agent Runtime

Glass delegates the agentic loop to Claude Code rather than owning it directly. This gives Glass:

1. **Context window management for free.** Claude Code handles compaction, summarization, and prompt caching internally.
2. **Session continuity.** Claude Code manages conversation state across turns within an invocation.
3. **Evolving capabilities.** As Claude Code adds features (streaming improvements, new tool patterns, skills), Glass inherits them automatically.
4. **Replaceability.** The `AgentRuntime` trait abstracts the boundary. If Claude Code becomes limiting, Glass can swap in a custom loop — the MCP server and security enforcement are independent.

**SDK fallback chain:** First choice is `claude-sdk-rs` (Rust, wraps Claude Code CLI). If it's missing features, fall back to the official Claude Agent SDK (TypeScript/Python via subprocess). Last resort: raw Claude Code CLI via `tokio::process::Command`.

---

## Runtime Implementation

`ClaudeCodeRuntime` implements the `AgentRuntime` trait (defined in `src/traits.rs`). The Discord handler and scheduler accept `&dyn AgentRuntime`, never the concrete struct. This is the primary seam for test mocks.

```rust
// agent/runtime.rs

use claude_sdk_rs::{Client, Config, StreamFormat};

pub struct ClaudeCodeRuntime {
    model: String,
    mcp_binary_path: PathBuf,  // Path to the `glass` binary for `glass serve-mcp`
    timeout_secs: u64,
}

impl ClaudeCodeRuntime {
    pub fn new(model: String, mcp_binary_path: PathBuf, timeout_secs: u64) -> Self {
        Self { model, mcp_binary_path, timeout_secs }
    }

    /// Generate a temporary MCP config file for this invocation.
    /// The config tells Claude Code to spawn `glass serve-mcp` as its tool server.
    fn write_mcp_config(
        &self,
        ctx: &InvocationContext,
    ) -> Result<tempfile::NamedTempFile, AgentError> {
        let mcp_config = serde_json::json!({
            "mcpServers": {
                "glass-tools": {
                    "command": self.mcp_binary_path.to_str().unwrap(),
                    "args": [
                        "serve-mcp",
                        "--project", &ctx.project.name,
                        "--workspace", ctx.project.workspace_path.to_str().unwrap(),
                        "--allowed-paths", serde_json::to_string(&ctx.allowed_paths)?,
                        "--network-capability", format!("{:?}", ctx.network_capability),
                    ]
                }
            }
        });

        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(&mut file, &mcp_config)?;
        Ok(file)
    }
}

#[async_trait]
impl AgentRuntime for ClaudeCodeRuntime {
    async fn run_invocation(
        &self,
        ctx: &InvocationContext,
    ) -> Result<InvocationResult, AgentError> {
        // Generate MCP config for this invocation
        let mcp_config_file = self.write_mcp_config(ctx)?;

        // Build claude-sdk-rs config
        let config = Config::builder()
            .model(&self.model)
            .system_prompt(&ctx.system_prompt)
            .mcp_config(mcp_config_file.path())
            .allowed_tools(ctx.allowed_tools.clone())
            .max_turns(ctx.max_turns.unwrap_or(25))
            .timeout_secs(self.timeout_secs)
            .stream_format(StreamFormat::Json)  // Get metadata back
            .skip_permissions(true)             // No interactive prompts
            .security_level(claude_sdk_rs::SecurityLevel::Disabled) // Glass handles security
            .build()
            .map_err(|e| AgentError::ConfigError(e.to_string()))?;

        let client = Client::new(config);

        // Optionally resume an existing session
        let query = if let Some(session_id) = &ctx.resume_session_id {
            client.query(&ctx.prompt)
                // Session resumption would be configured here
        } else {
            client.query(&ctx.prompt)
        };

        // Send and get full response with metadata
        let response = query.send_full().await
            .map_err(|e| AgentError::RuntimeError(e.to_string()))?;

        // Extract metadata for audit logging
        let (tokens_used, cost_usd, session_id) = match &response.metadata {
            Some(meta) => (
                meta.tokens_used.as_ref().map(|t| t.input + t.output).unwrap_or(0) as u32,
                meta.cost_usd.unwrap_or(0.0),
                meta.session_id.clone(),
            ),
            None => (0, 0.0, String::new()),
        };

        Ok(InvocationResult {
            response_text: response.content,
            tokens_used,
            cost_usd,
            session_id,
            // Tool calls are captured by the MCP server and reported separately
            tool_calls: Vec::new(), // Populated by MCP server audit log
        })
    }
}
```

**Note on `SecurityLevel::Disabled`:** Claude-sdk-rs includes input validation that rejects certain characters in prompts (e.g., `../` for path traversal). Glass disables this because Glass handles all security at the MCP tool boundary — the prompt content is assembled by Glass itself and is trusted. The SDK's validation would incorrectly reject legitimate system prompts containing path examples or code snippets.

**Note on system prompt length:** The SDK validates system prompts to a 10,000 character max. Glass's assembled prompts (identity + skills + project context) may exceed this. If so, the workaround is to construct the `Config` struct directly (bypassing validation) or to split context between the system prompt and the initial user message.

---

## Invocation Result

```rust
// agent/invocation.rs

pub struct InvocationResult {
    /// The final text response from the agent.
    pub response_text: String,
    /// Total tokens used across all API calls in this invocation.
    pub tokens_used: u32,
    /// Estimated cost in USD.
    pub cost_usd: f64,
    /// Claude Code session ID (for potential session resumption).
    pub session_id: String,
    /// Tool calls made during the invocation (captured by MCP server).
    pub tool_calls: Vec<ToolCallRecord>,
}
```

---

## `query_projects` — The Two-Phase Pattern

When the root project (autonomous mode) calls `query_projects`, Glass handles it as two separate Claude Code sessions. The bot detects the call from the completed session's tool call records — no side-channel needed:

1. **Phase 1: Root invocation.** Claude Code runs with full root tools including `query_projects`. When the MCP server receives a `query_projects` call, it returns: *"Queries dispatched. This session will end and a new session will resume with the results."* Claude Code, with no further actionable step, wraps up the session.
2. **Bot detects `query_projects` in session results.** The bot reads the completed session's tool call records (which it already does for audit logging). If `query_projects` appears, the bot extracts the project list and query string and initiates subagent dispatch.
3. **Subagent dispatch.** For each specified project, the bot spawns an independent Claude Code session with that project's context and the query as the prompt.
4. **Phase 2: Synthesis.** The bot starts a new Claude Code session with: the original root context, the collected subagent responses as the prompt, and an **empty toolset** — no tools registered at all. Claude Code can only produce a text response.

```rust
// agent/subagent.rs

pub async fn dispatch_project_queries(
    runtime: &dyn AgentRuntime,
    query: &str,
    project_names: &[String],
    projects: &ProjectRegistry,
) -> Result<Vec<SubagentResponse>, AgentError> {
    let mut responses = Vec::new();

    // Run subagents sequentially to limit concurrent API/CLI usage.
    // Can be parallelized later if latency is a concern.
    for name in project_names {
        let project = projects.get(name)?;
        let sub_ctx = build_subagent_context(&project, query)?;

        let result = runtime.run_invocation(&sub_ctx).await?;

        responses.push(SubagentResponse {
            project_name: name.clone(),
            response: result.response_text,
        });
    }

    Ok(responses)
}
```

---

## Error Types

```rust
// agent/runtime.rs

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Claude Code not found. Install with: npm install -g @anthropic-ai/claude-code")]
    BinaryNotFound,

    #[error("Claude Code runtime error: {0}")]
    RuntimeError(String),

    #[error("Agent configuration error: {0}")]
    ConfigError(String),

    #[error("Agent timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },

    #[error("Project not found: {0}")]
    ProjectNotFound(String),
}
```
