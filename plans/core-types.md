# Core Types & Data Model

*Code samples extracted from [ARCHITECTURE.md](../ARCHITECTURE.md) — these are the foundational types that flow through the system.*

---

## Project

```rust
// projects/registry.rs

/// A discovered project with its metadata.
pub struct Project {
    /// The project identifier — matches the Discord channel name and workspace directory.
    pub name: String,
    /// Absolute path to the project's workspace directory.
    pub workspace_path: PathBuf,
    /// The Discord channel ID for this project.
    pub channel_id: ChannelId,
    /// Whether this project is the root project.
    pub is_root: bool,
    /// Whether this project is archived.
    pub archived: bool,
}
```

---

## ChannelCapability

```rust
// capabilities/config.rs

/// Network capability tier for a project.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkCapability {
    None,
    Search,
    Allowlist,
    Open,
}

/// Full capability configuration for a project channel.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub network: NetworkCapability,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub integrations: Vec<String>,
}
```

---

## InvocationContext

```rust
// agent/invocation.rs

/// Everything the bot assembles before invoking Claude Code.
pub struct InvocationContext {
    /// Which project this invocation is for.
    pub project: Project,
    /// The trigger that caused this invocation.
    pub trigger: InvocationTrigger,
    /// Assembled system prompt (identity + skills + project context).
    pub system_prompt: String,
    /// The user message or task description to send as the prompt.
    pub prompt: String,
    /// Network capability for this invocation (determines which MCP tools are exposed).
    pub network_capability: NetworkCapability,
    /// Whether the owner is currently in the conversation (for root project).
    pub owner_present: bool,
    /// Workspace paths the agent is allowed to read/write (enforced by MCP server).
    pub allowed_paths: Vec<PathBuf>,
    /// Names of MCP tools to expose for this invocation (determined by capabilities).
    pub allowed_tools: Vec<String>,
    /// Session ID to resume, if continuing a conversation.
    pub resume_session_id: Option<String>,
    /// Maximum agentic turns for this invocation (safety valve).
    pub max_turns: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum InvocationTrigger {
    /// A user sent a message in a project channel.
    UserMessage {
        user_id: UserId,
        channel_id: ChannelId,
        message_content: String,
    },
    /// A scheduled task fired.
    ScheduledTask {
        task_id: String,
        cron_expression: String,
        description: String,
    },
}
```

**Note:** Glass does not define Claude API wire types (`Message`, `ContentBlock`, `ToolDefinition`, etc.). These are handled internally by `claude-sdk-rs` and Claude Code. Glass's types are its own domain types — `InvocationContext`, `InvocationResult`, `ToolCallRecord`, etc.

---

## InvocationResult

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

## Audit Types

```rust
// audit/types.rs

#[derive(Debug, Serialize)]
pub struct AuditEntry {
    pub id: String, // UUID
    pub timestamp: DateTime<Utc>,
    pub trigger: AuditTrigger,
    pub project: String,
    pub prompt_summary: String,
    pub full_context: Vec<claude::types::Message>,
    pub response: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens_used: u32,
    pub duration_ms: u64,
    pub status: InvocationStatus,
}

#[derive(Debug, Serialize)]
pub struct AuditTrigger {
    #[serde(rename = "type")]
    pub trigger_type: String, // "user_message" | "scheduled_task"
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct ToolCallRecord {
    pub tool: String,
    pub args: serde_json::Value,
    pub result: String,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationStatus {
    Success,
    Error,
    MaxTokens,
}
```

---

## Schedule Types

```rust
// scheduler/tasks.rs

#[derive(Debug, Deserialize)]
pub struct ScheduleFile {
    pub tasks: Vec<ScheduledTask>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub cron: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }
```

---

## Skill Metadata

```rust
// skills/discovery.rs

/// Metadata parsed from SKILL.md YAML frontmatter.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
}

/// A discovered skill with its location.
pub struct DiscoveredSkill {
    pub metadata: SkillMetadata,
    /// Path to the skill directory (relative to workspace root).
    pub path: PathBuf,
    /// Whether this is a global skill (workspace/skills/) or project-local.
    pub global: bool,
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

```rust
// main.rs or a dedicated errors.rs

#[derive(Debug, thiserror::Error)]
pub enum GlassError {
    #[error("Discord error: {0}")]
    Discord(#[from] serenity::Error),

    #[error("Agent runtime error: {0}")]
    Agent(#[from] AgentError),

    #[error("Sandbox error: {0}")]
    Sandbox(#[from] SandboxError),

    #[error("Tool error: {0}")]
    Tool(#[from] ToolError),

    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    #[error("Project error: {0}")]
    Project(#[from] ProjectError),

    #[error("Audit error: {0}")]
    Audit(#[from] AuditError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
```

---

## Configuration

```rust
// config.rs

#[derive(Debug, Clone)]
pub struct GlassConfig {
    // Required
    pub discord_bot_token: String,
    pub discord_guild_id: u64,
    pub anthropic_api_key: String,

    // Optional with defaults
    pub brave_search_api_key: Option<String>,
    pub workspace_path: PathBuf,
    pub harness_path: PathBuf,
    pub docker_image: String,
    pub claude_model: String,

    // Derived
    pub audit_channel_name: String,
    pub root_channel_name: String,
    pub owner_user_id: Option<u64>,
}

impl GlassConfig {
    /// Load configuration from environment variables.
    /// Panics with clear error messages if required vars are missing.
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok(); // Load .env file if present

        Self {
            discord_bot_token: require_env("DISCORD_BOT_TOKEN"),
            discord_guild_id: require_env("DISCORD_GUILD_ID").parse()
                .expect("DISCORD_GUILD_ID must be a valid u64"),
            anthropic_api_key: require_env("ANTHROPIC_API_KEY"),
            brave_search_api_key: std::env::var("BRAVE_SEARCH_API_KEY").ok(),
            workspace_path: PathBuf::from(
                std::env::var("WORKSPACE_PATH").unwrap_or_else(|_| "./glass-data/workspace".into())
            ),
            harness_path: PathBuf::from(
                std::env::var("HARNESS_PATH").unwrap_or_else(|_| "./glass-data/harness".into())
            ),
            docker_image: std::env::var("DOCKER_IMAGE")
                .unwrap_or_else(|_| "glass-sandbox".into()),
            claude_model: std::env::var("CLAUDE_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".into()),
            audit_channel_name: "audit-log".to_string(),
            root_channel_name: "glass".to_string(),
            owner_user_id: std::env::var("OWNER_USER_ID").ok()
                .and_then(|s| s.parse().ok()),
        }
    }
}

fn require_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("Required environment variable {} is not set", name))
}
```
