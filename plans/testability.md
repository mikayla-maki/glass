# Testability Architecture — Implementation Plan

This file contains the detailed code samples for Glass's testability architecture: boundary traits, production implementations, mock implementations, test fixtures, and test examples.

**Parent document:** [ARCHITECTURE.md](../ARCHITECTURE.md)

---

## Design Principle

> **Mock the boundary, not the logic.** A test that exercises tool dispatch should use the real `ToolExecutor`, the real path scoping, and the real capability filter. The only fakes are what's on the other side of the process or kernel boundary: Claude Code's subprocess output, Docker's exec output, Discord's message delivery, and the bytes on disk.

Glass has three external boundaries that are expensive, slow, or impossible to use in tests: the agent runtime (Claude Code subprocess), Discord, and the Docker sandbox. Each is abstracted behind a trait. **Everything else is real.** The production `InvocationContext`, `AuditEntry`, `ToolExecutor` dispatch logic, capability filtering, path scoping, context assembly — all of it runs identically in tests and production. The mocks replace only the I/O at the edge.

**Filesystem uses temp directories, not a trait.** File I/O uses `std::fs` directly in both production and tests. Tests create temp directories (`tempfile` crate) with pre-populated fixture files. This gives better test fidelity than an in-memory mock — path scoping, permissions, and directory listing all exercise real OS behavior. The cost is negligible (temp dir operations are fast), and the benefit is one fewer abstraction to maintain.

---

## Boundary Traits

All three traits live in `src/traits.rs`. They are deliberately minimal — just enough surface area to cover what the rest of the system needs, no more.

```rust
// src/traits.rs

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ── Agent Runtime ─────────────────────────────────────────────

/// The boundary between Glass and whatever agent runtime is on the other end.
/// Production: ClaudeCodeRuntime (claude-sdk-rs wrapping the Claude Code CLI).
/// Tests: MockAgentRuntime (returns canned InvocationResults).
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn run_invocation(
        &self,
        ctx: &InvocationContext,
    ) -> Result<InvocationResult, AgentError>;
}

// ── Discord ───────────────────────────────────────────────────

/// The boundary between Glass and Discord.
/// Production: SerenityDiscord (wraps serenity Http).
/// Tests: MockDiscordSink (captures sent messages for assertion).
#[async_trait]
pub trait DiscordSink: Send + Sync {
    /// Send a plain text message to a channel.
    async fn send_message(
        &self,
        channel_id: ChannelId,
        content: &str,
    ) -> Result<(), DiscordSinkError>;

    /// Send a message with action buttons (inbox review, domain approval).
    async fn send_review_message(
        &self,
        channel_id: ChannelId,
        content: &str,
        buttons: Vec<ReviewButton>,
    ) -> Result<(), DiscordSinkError>;
}

/// A simplified button representation that doesn't depend on serenity types.
/// The production DiscordSink converts these to serenity CreateButton internally.
#[derive(Debug, Clone)]
pub struct ReviewButton {
    pub custom_id: String,
    pub label: String,
    pub style: ButtonStyle,
}

// ── Sandbox ───────────────────────────────────────────────────

/// The boundary between Glass and the container runtime.
/// Production: DockerSandbox (docker CLI via tokio::process::Command).
/// Tests: MockSandbox (returns canned ExecResults).
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn create_container(
        &self,
        project_workspace: &Path,
    ) -> Result<(), SandboxError>;

    async fn exec(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<ExecResult, SandboxError>;

    async fn destroy(&self) -> Result<(), SandboxError>;
}

// No Filesystem trait — file I/O uses std::fs directly.
// Tests use temp directories (tempfile crate) for isolation.
```

---

## Production Implementations

Each trait has exactly one production implementation:

```rust
// agent/runtime.rs
impl AgentRuntime for ClaudeCodeRuntime { ... }  // claude-sdk-rs wrapping Claude Code CLI

// discord/sink.rs
pub struct SerenityDiscord { http: Arc<Http> }
impl DiscordSink for SerenityDiscord { ... }  // serenity Http calls

// sandbox/docker.rs
pub struct DockerSandbox { ... }
impl Sandbox for DockerSandbox { ... }  // docker CLI via Command
```

---

## How Traits Thread Through the System

The traits are injected at construction time and flow downward. No module reaches out to a global or constructs its own I/O — it receives the trait object from its caller.

```
main.rs
  │  Constructs: ClaudeCodeRuntime, SerenityDiscord, DockerSandbox
  │  Wraps them in Arc<dyn Trait> where shared
  │  Filesystem: std::fs directly (paths from config)
  │
  ├─► GlassHandler (Discord event handler)
  │     holds: Arc<dyn AgentRuntime>, Arc<dyn DiscordSink>, workspace_path, harness_path
  │     on message → spawns invocation task
  │
  ├─► ToolExecutor (used by MCP server subprocess)
  │     holds: workspace_path, harness_path
  │     execute() accepts: &dyn Sandbox
  │
  ├─► AuditLogger
  │     holds: harness_path
  │
  ├─► InboxManager
  │     holds: harness_path, Arc<dyn DiscordSink>
  │
  └─► invoke_agent()
        accepts: &dyn AgentRuntime, &InvocationContext
        (MCP server subprocess handles tool execution independently)
```

The `InvocationContext`, `AuditEntry`, `ScheduledTask`, `ChannelConfig`, `SkillMetadata` — all the domain types — remain plain structs. They don't know or care about traits. The traits live only at the I/O boundary.

---

## Mock Implementations

These live in `tests/helpers/` and are designed to be ergonomic. The goal: writing a test that drives a full multi-turn agentic conversation should take ~15 lines, not 150.

### MockAgentRuntime

The most important mock. It holds a queue of canned invocation results and records every context it receives for later assertion.

```rust
// tests/helpers/mock_runtime.rs

use std::sync::{Arc, Mutex};
use crate::traits::AgentRuntime;

pub struct MockAgentRuntime {
    /// Queue of results to return, in order.
    results: Arc<Mutex<VecDeque<InvocationResult>>>,
    /// Every invocation context received, for assertions.
    invocations: Arc<Mutex<Vec<InvocationContext>>>,
}

impl MockAgentRuntime {
    pub fn new() -> Self {
        Self {
            results: Arc::new(Mutex::new(VecDeque::new())),
            invocations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Queue a simple text response.
    pub fn respond_with_text(self, text: &str) -> Self {
        self.results.lock().unwrap().push_back(InvocationResult {
            response_text: text.to_string(),
            tokens_used: 150,
            cost_usd: 0.001,
            session_id: format!("mock_{}", Uuid::new_v4()),
            tool_calls: Vec::new(),
        });
        self
    }

    /// Queue a result with tool calls recorded.
    pub fn respond_with_tools(self, text: &str, tool_calls: Vec<ToolCallRecord>) -> Self {
        self.results.lock().unwrap().push_back(InvocationResult {
            response_text: text.to_string(),
            tokens_used: 300,
            cost_usd: 0.003,
            session_id: format!("mock_{}", Uuid::new_v4()),
            tool_calls,
        });
        self
    }

    /// Queue a raw result.
    pub fn respond_with(self, result: InvocationResult) -> Self {
        self.results.lock().unwrap().push_back(result);
        self
    }

    // ── Assertion helpers ────────────────────────────────────

    pub fn invocation_count(&self) -> usize {
        self.invocations.lock().unwrap().len()
    }

    pub fn invocation(&self, index: usize) -> InvocationContext {
        self.invocations.lock().unwrap()[index].clone()
    }

    pub fn system_prompt(&self, index: usize) -> String {
        self.invocation(index).system_prompt.clone()
    }

    pub fn was_tool_allowed(&self, index: usize, tool_name: &str) -> bool {
        self.invocation(index).allowed_tools.iter().any(|t| t == tool_name)
    }
}

#[async_trait]
impl AgentRuntime for MockAgentRuntime {
    async fn run_invocation(
        &self,
        ctx: &InvocationContext,
    ) -> Result<InvocationResult, AgentError> {
        // Record the invocation context
        self.invocations.lock().unwrap().push(ctx.clone());

        // Pop the next canned result
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AgentError::RuntimeError(
                "MockAgentRuntime: no more canned results".to_string(),
            ))
    }
}
```

### MockDiscordSink

Captures every message the bot tries to send, so tests can assert on what was posted and where.

```rust
// tests/helpers/mock_discord.rs

pub struct MockDiscordSink {
    messages: Arc<Mutex<Vec<SentMessage>>>,
    review_messages: Arc<Mutex<Vec<SentReviewMessage>>>,
}

#[derive(Debug, Clone)]
pub struct SentMessage {
    pub channel_id: ChannelId,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct SentReviewMessage {
    pub channel_id: ChannelId,
    pub content: String,
    pub buttons: Vec<ReviewButton>,
}

impl MockDiscordSink {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(Vec::new())),
            review_messages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// All plain messages sent, in order.
    pub fn messages(&self) -> Vec<SentMessage> {
        self.messages.lock().unwrap().clone()
    }

    /// All review messages (with buttons) sent, in order.
    pub fn review_messages(&self) -> Vec<SentReviewMessage> {
        self.review_messages.lock().unwrap().clone()
    }

    /// Messages sent to a specific channel.
    pub fn messages_to(&self, channel_id: ChannelId) -> Vec<SentMessage> {
        self.messages.lock().unwrap()
            .iter()
            .filter(|m| m.channel_id == channel_id)
            .cloned()
            .collect()
    }

    /// Was anything sent to this channel?
    pub fn was_channel_messaged(&self, channel_id: ChannelId) -> bool {
        self.messages.lock().unwrap()
            .iter()
            .any(|m| m.channel_id == channel_id)
    }
}

#[async_trait]
impl DiscordSink for MockDiscordSink {
    async fn send_message(
        &self,
        channel_id: ChannelId,
        content: &str,
    ) -> Result<(), DiscordSinkError> {
        self.messages.lock().unwrap().push(SentMessage {
            channel_id,
            content: content.to_string(),
        });
        Ok(())
    }

    async fn send_review_message(
        &self,
        channel_id: ChannelId,
        content: &str,
        buttons: Vec<ReviewButton>,
    ) -> Result<(), DiscordSinkError> {
        self.review_messages.lock().unwrap().push(SentReviewMessage {
            channel_id,
            content: content.to_string(),
            buttons,
        });
        Ok(())
    }
}
```

### MockSandbox

Returns canned command outputs in order. If a test doesn't queue any outputs, it returns a default success with empty stdout.

```rust
// tests/helpers/mock_sandbox.rs

pub struct MockSandbox {
    exec_results: Arc<Mutex<VecDeque<ExecResult>>>,
    exec_history: Arc<Mutex<Vec<String>>>,
    created: Arc<Mutex<bool>>,
}

impl MockSandbox {
    pub fn new() -> Self {
        Self {
            exec_results: Arc::new(Mutex::new(VecDeque::new())),
            exec_history: Arc::new(Mutex::new(Vec::new())),
            created: Arc::new(Mutex::new(false)),
        }
    }

    // Note: MockSandbox uses interior mutability (Arc<Mutex<>>)
    // so all trait methods can take &self instead of &mut self.

    /// Queue a successful command output.
    pub fn on_exec_return(self, stdout: &str) -> Self {
        self.exec_results.lock().unwrap().push_back(ExecResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
        });
        self
    }

    /// Queue a failed command output.
    pub fn on_exec_fail(self, stderr: &str, exit_code: i32) -> Self {
        self.exec_results.lock().unwrap().push_back(ExecResult {
            stdout: String::new(),
            stderr: stderr.to_string(),
            exit_code,
        });
        self
    }

    /// Queue a raw ExecResult for full control.
    pub fn on_exec(self, result: ExecResult) -> Self {
        self.exec_results.lock().unwrap().push_back(result);
        self
    }

    /// All commands that were executed, in order.
    pub fn exec_history(&self) -> Vec<String> {
        self.exec_history.lock().unwrap().clone()
    }

    /// Was any command executed?
    pub fn was_exec_called(&self) -> bool {
        !self.exec_history.lock().unwrap().is_empty()
    }
}

#[async_trait]
impl Sandbox for MockSandbox {
    async fn create_container(&self, _workspace: &Path) -> Result<(), SandboxError> {
        *self.created.lock().unwrap() = true;
        Ok(())
    }

    async fn exec(&self, command: &str, _timeout: Duration) -> Result<ExecResult, SandboxError> {
        self.exec_history.lock().unwrap().push(command.to_string());

        let result = self.exec_results.lock().unwrap().pop_front()
            .unwrap_or(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            });

        Ok(result)
    }

    async fn destroy(&self) -> Result<(), SandboxError> {
        *self.created.lock().unwrap() = false;
        Ok(())
    }
}
```

### Temp Directory Fixtures

Instead of an in-memory filesystem mock, tests use real temp directories populated with fixture files. This exercises actual OS behavior (path resolution, permissions, directory listing) while remaining fast and isolated.

```rust
// tests/helpers/fixtures.rs (filesystem portion)

use tempfile::TempDir;
use std::fs;
use std::path::{Path, PathBuf};

/// Create a temp directory with a standard workspace layout for testing.
pub fn create_test_workspace() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Root workspace files
    write_fixture(root, "workspace/identity.md", "You are Glass, a helpful assistant.");

    // A test project
    write_fixture(root, "workspace/test-project/brief.md", "A test project for unit tests.");
    write_fixture(root, "workspace/test-project/status.md", "Status: testing.");

    // Harness directory
    fs::create_dir_all(root.join("harness/audit")).unwrap();
    fs::create_dir_all(root.join("harness/pending")).unwrap();

    dir
}

fn write_fixture(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}
```

The `TempDir` is automatically cleaned up when it goes out of scope. Tests that need custom layouts call `write_fixture` to add files before exercising the system under test.

---

## TestFixtures: Wiring It All Together

The `TestFixtures` struct assembles all the mocks and production types into a ready-to-use test harness. It holds onto the `Arc`s so tests can both drive the system and inspect the mocks afterward.

```rust
// tests/helpers/fixtures.rs

pub struct TestFixtures {
    pub runtime: Arc<MockAgentRuntime>,
    pub discord: Arc<MockDiscordSink>,
    pub sandbox: MockSandbox,
    pub temp_dir: TempDir,                 // Holds the temp directory — cleaned up on drop
    pub tool_executor: ToolExecutor,       // Real production struct
    pub capabilities: CapabilitiesFile,     // Real production struct
    pub projects: ProjectRegistry,         // Real production struct
    pub audit_logger: AuditLogger,         // Real production struct (writes to temp dir)
}

impl TestFixtures {
    /// Create a test harness with sensible defaults:
    /// - A root project workspace with identity.md
    /// - One test project ("test-project") with brief.md and status.md
    /// - Open network capability by default
    /// - No canned runtime responses (add them per-test)
    pub fn new() -> Self {
        let temp_dir = create_test_workspace();
        let root = temp_dir.path().to_path_buf();

        let runtime = Arc::new(MockAgentRuntime::new());
        let discord = Arc::new(MockDiscordSink::new());
        let sandbox = MockSandbox::new();

        let tool_executor = ToolExecutor::new(
            root.join("workspace"),
            root.join("harness"),
            Some("fake-brave-key".to_string()),
        );

        Self {
            runtime,
            discord,
            sandbox,
            temp_dir,
            tool_executor,
            capabilities: default_test_capabilities(),
            projects: default_test_project_registry(),
            audit_logger: AuditLogger::new(root.join("harness")),
        }
    }

    /// The root path of the temp directory (for reading/asserting on files).
    pub fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    /// Build an InvocationContext for the test project.
    pub fn context_for_project(&self, project_name: &str) -> InvocationContext {
        // Uses the real context assembly code with the real temp filesystem
        assemble_context(
            &self.projects.get(project_name).unwrap(),
            &InvocationTrigger::UserMessage {
                user_id: UserId::new(1),
                channel_id: ChannelId::new(1),
                message_content: "test".to_string(),
            },
            &self.capabilities,
        )
    }
}
```

---

## Test Examples

There are two distinct testing levels:

1. **Integration tests** use `MockAgentRuntime` to test the full invocation flow (context assembly → agent runtime → Discord response → audit). Claude Code's internal behavior is opaque — the mock returns a canned `InvocationResult`.
2. **MCP tool tests** use `ToolExecutor` directly with `MockSandbox` + temp directories to test tool dispatch, path scoping, and capability enforcement. These exercise the same code that runs inside the `glass serve-mcp` subprocess.

### Test: Simple text conversation (integration)

```rust
#[tokio::test]
async fn test_simple_text_response() {
    let fixtures = TestFixtures::new();

    // Queue a canned response — Claude Code's internal loop is opaque
    fixtures.runtime.respond_with_text("Hello! I'm Glass.");

    let ctx = fixtures.context_for_project("test-project");
    let result = fixtures.runtime.run_invocation(&ctx).await.unwrap();

    // The response came through
    assert_eq!(result.response_text, "Hello! I'm Glass.");

    // The runtime was called exactly once
    assert_eq!(fixtures.runtime.invocation_count(), 1);

    // No tool calls recorded (Claude Code handled everything internally)
    assert!(result.tool_calls.is_empty());
}
```

### Test: Invocation context is assembled correctly (integration)

```rust
#[tokio::test]
async fn test_context_assembly_for_project() {
    let fixtures = TestFixtures::new();
    fixtures.runtime.respond_with_text("ok");

    let ctx = fixtures.context_for_project("test-project");
    let _ = fixtures.runtime.run_invocation(&ctx).await.unwrap();

    // Verify the context passed to the runtime
    let recorded_ctx = fixtures.runtime.invocation(0);
    assert_eq!(recorded_ctx.project.name, "test-project");
    assert!(recorded_ctx.system_prompt.contains("You are Glass"));
    assert!(recorded_ctx.system_prompt.contains("A test project for unit tests"));

    // Standard tools are in the allowed list
    assert!(recorded_ctx.allowed_tools.contains(&"shell".to_string()));
    assert!(recorded_ctx.allowed_tools.contains(&"read_file".to_string()));
    assert!(recorded_ctx.allowed_tools.contains(&"suggest_learning".to_string()));
}
```

### Test: Channel capabilities filter network tools (integration)

```rust
#[tokio::test]
async fn test_none_capability_excludes_network_tools() {
    let fixtures = TestFixtures::new();
    fixtures.runtime.respond_with_text("ok");

    // Build context for a project with network: none
    let mut ctx = fixtures.context_for_project("test-project");
    ctx.network_capability = NetworkCapability::None;
    ctx.allowed_tools = allowed_tools_for_project(
        &ChannelConfig { network: NetworkCapability::None, ..Default::default() },
        false,
        false,
    );

    let _ = fixtures.runtime.run_invocation(&ctx).await.unwrap();

    // Assert the runtime was NOT given network tools
    let recorded_ctx = fixtures.runtime.invocation(0);
    assert!(!recorded_ctx.allowed_tools.contains(&"fetch_url".to_string()));
    assert!(!recorded_ctx.allowed_tools.contains(&"web_search".to_string()));

    // But it WAS given standard tools
    assert!(recorded_ctx.allowed_tools.contains(&"shell".to_string()));
    assert!(recorded_ctx.allowed_tools.contains(&"read_file".to_string()));
    assert!(recorded_ctx.allowed_tools.contains(&"suggest_learning".to_string()));
}
```

### Test: MCP tool writes a file via write_file (tool-level)

```rust
#[tokio::test]
async fn test_write_file_via_mcp_tool_executor() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_fixture(root, "workspace/test-project/brief.md", "A test project.");

    let tool_executor = ToolExecutor::new(
        root.join("workspace"),
        root.join("harness"),
        None,
    );

    let mcp_ctx = McpContext {
        project_name: "test-project".to_string(),
        workspace_path: root.join("workspace/test-project"),
        allowed_paths: vec![root.join("workspace/test-project")],
        network_capability: NetworkCapability::None,
        owner_present: false,
    };

    // Execute write_file directly through the MCP tool executor
    let result = tool_executor.execute(
        "write_file",
        &json!({ "path": "notes.md", "content": "# Surgery Notes\n\nPre-op checklist." }),
        &mcp_ctx,
        &MockSandbox::new(),
    ).await;

    assert!(result.is_ok());

    // The file was written to the real temp filesystem at the scoped path
    let written = root.join("workspace/test-project/notes.md");
    assert!(written.exists());
    let content = std::fs::read_to_string(&written).unwrap();
    assert!(content.contains("Pre-op checklist"));
}
```

### Test: Path traversal is rejected (tool-level)

```rust
#[tokio::test]
async fn test_path_traversal_blocked() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_fixture(root, "workspace/test-project/brief.md", "Test.");
    write_fixture(root, "workspace/finances/secrets.md", "SSN: 123-45-6789");

    let tool_executor = ToolExecutor::new(
        root.join("workspace"),
        root.join("harness"),
        None,
    );

    let mcp_ctx = McpContext {
        project_name: "test-project".to_string(),
        workspace_path: root.join("workspace/test-project"),
        allowed_paths: vec![root.join("workspace/test-project")],
        network_capability: NetworkCapability::None,
        owner_present: false,
    };

    // Attempt to read a file outside the project via path traversal
    let result = tool_executor.execute(
        "read_file",
        &json!({ "path": "../finances/secrets.md" }),
        &mcp_ctx,
        &MockSandbox::new(),
    ).await;

    // The MCP tool executor rejects the traversal
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ToolError::PathTraversal { .. }));
}
```

### Test: Shell tool delegates to sandbox (tool-level)

```rust
#[tokio::test]
async fn test_shell_tool_uses_sandbox() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let tool_executor = ToolExecutor::new(
        root.join("workspace"),
        root.join("harness"),
        None,
    );

    let sandbox = MockSandbox::new()
        .on_exec_return("brief.md\nstatus.md\n");

    let mcp_ctx = McpContext {
        project_name: "test-project".to_string(),
        workspace_path: root.join("workspace/test-project"),
        allowed_paths: vec![root.join("workspace/test-project")],
        network_capability: NetworkCapability::None,
        owner_present: false,
    };

    let result = tool_executor.execute(
        "shell",
        &json!({ "command": "ls" }),
        &mcp_ctx,
        &sandbox,
    ).await.unwrap();

    assert!(result.contains("brief.md"));
    assert_eq!(sandbox.exec_history(), vec!["ls"]);
}
```

### Test: Audit logging captures invocation result (integration)

```rust
#[tokio::test]
async fn test_audit_log_written_after_invocation() {
    let fixtures = TestFixtures::new();
    fixtures.runtime.respond_with_text("Done.");

    let ctx = fixtures.context_for_project("test-project");
    let result = fixtures.runtime.run_invocation(&ctx).await.unwrap();

    // Build and write audit entry (this is what the handler does post-invocation)
    let entry = build_audit_entry(&result, &ctx.trigger, &ctx.project.name, 1500);
    fixtures.audit_logger.log(&entry).unwrap();

    // An audit file was written to the temp directory
    let audit_dir = fixtures.root().join("harness/audit");
    let audit_files: Vec<_> = std::fs::read_dir(&audit_dir).unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(audit_files.len(), 1);

    // The audit file is valid JSON containing the response
    let content = std::fs::read_to_string(audit_files[0].path()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["status"], "success");
    assert_eq!(parsed["response"], "Done.");
}
```

### Test: Response posted to correct Discord channel (integration)

```rust
#[tokio::test]
async fn test_response_posted_to_correct_channel() {
    let fixtures = TestFixtures::new();
    fixtures.runtime.respond_with_text("Here's your update!");

    let project_channel = ChannelId::new(12345);
    let audit_channel = ChannelId::new(99999);

    // Simulate the handler flow: invoke agent, post response
    let ctx = fixtures.context_for_project("test-project");
    let result = fixtures.runtime.run_invocation(&ctx).await.unwrap();

    fixtures.discord.send_message(project_channel, &result.response_text).await.unwrap();

    // Verify the right channel got the message
    assert_eq!(fixtures.discord.messages_to(project_channel).len(), 1);
    assert!(fixtures.discord.messages_to(project_channel)[0].content.contains("update"));

    // Audit channel was NOT messaged (this was a user-initiated invocation)
    assert!(!fixtures.discord.was_channel_messaged(audit_channel));
}
```

### Test: Inbox suggestion triggers Discord review (tool + integration)

```rust
#[tokio::test]
async fn test_suggest_learning_posts_review_to_discord() {
    let fixtures = TestFixtures::new();

    let mcp_ctx = McpContext {
        project_name: "test-project".to_string(),
        workspace_path: PathBuf::from("workspace/test-project"),
        allowed_paths: vec![PathBuf::from("workspace/test-project")],
        network_capability: NetworkCapability::None,
        owner_present: false,
    };

    // Execute suggest_learning through the MCP tool executor
    let _ = fixtures.tool_executor.execute(
        "suggest_learning",
        &json!({ "content": "Backward-planning works well for deadline-driven tasks." }),
        &mcp_ctx,
        &fixtures.sandbox,
    ).await.unwrap();

    // A pending suggestion was written to the temp directory
    let pending_dir = fixtures.root().join("harness/pending");
    let pending_files: Vec<_> = std::fs::read_dir(&pending_dir).unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(pending_files.len(), 1);

    // Post the review message (this is what the handler does after invocation)
    let suggestion = load_pending_suggestion(&pending_files[0].path()).unwrap();
    post_for_review(fixtures.discord.as_ref(), ChannelId::new(1), &suggestion).await.unwrap();

    // Discord got a review message with buttons
    let reviews = fixtures.discord.review_messages();
    assert_eq!(reviews.len(), 1);
    assert!(reviews[0].content.contains("Backward-planning"));
    assert!(reviews[0].buttons.iter().any(|b| b.label == "Approve"));
    assert!(reviews[0].buttons.iter().any(|b| b.label == "Reject"));
}
```

---

## What This Architecture Buys You

| Scenario | Without traits | With traits + temp dirs |
|---|---|---|
| Test invocation flow | Need Claude Code CLI, Docker, API keys | `MockAgentRuntime::new().respond_with_text("hi")` |
| Test capability filtering | Need real files in real directories | `allowed_tools_for_project(&config, false, false)` |
| Test MCP tool dispatch | Need Docker for shell | `ToolExecutor` + `MockSandbox` + temp dir |
| Test audit logging | Write to real dir, read back, parse | Same — temp dir, inspect with `std::fs` |
| Test Discord output | Can't — need a live Discord server | `discord.messages_to(channel_id)` |
| Test path traversal | Need real directory hierarchy | `ToolExecutor` with temp dir (real OS path behavior) |
| Run in CI | Docker + Claude Code CLI + API keys + Discord bot | `cargo test` — zero external dependencies |

The goal is that `cargo test` runs the full test suite in seconds, with no network, no Docker, no Claude Code CLI, and no filesystem side effects. The only tests that require external services are explicitly marked `#[ignore]` and test the production implementations of the traits themselves.

---

## Unit Tests (Non-Mock)

Some modules have no external I/O and are tested directly without mocks:

| Module | What to test |
|--------|-------------|
| `capabilities/config.rs` | Parse various TOML configurations, default fallback |
| `capabilities/filter.rs` | Tool filtering for each capability tier |
| `capabilities/allowlist.rs` | Domain matching (exact, subdomain, edge cases) |
| `context/assembly.rs` | System prompt generation for each invocation type (temp dir with fixture files) |
| `mcp/scoping.rs` | Path traversal rejection (`../`, symlinks, absolute paths). Lexical normalization. |
| `mcp/tools.rs` | Tool dispatch with `MockSandbox` + temp dir |
| `scheduler/tasks.rs` | Cron expression parsing and fire-time calculation |
| `skills/discovery.rs` | YAML frontmatter parsing (temp dir with fixture SKILL.md files) |

---

## Test Coverage Targets

| Area | Target | Rationale |
|------|--------|-----------|
| Capabilities parsing & filtering | 100% | Security-critical |
| Path traversal checks (lexical normalization) | 100% | Security-critical |
| Context assembly (all 3 types) | 90%+ | Core correctness |
| MCP tool dispatch & scoping | 90%+ | Correctness + security |
| Audit logging | 90%+ | Compliance |
| Inbox pipeline | 90%+ | Trust boundary |
| Scheduler cron logic | 90%+ | Timing correctness |
| Discord handler (end-to-end with mocks) | 80%+ | Testable without live server |
| Agent runtime (production impl) | Manual / CI | Requires Claude Code CLI — `#[ignore]` tests |
| Docker sandbox (production impl) | Manual / CI | Requires Docker daemon — `#[ignore]` tests |
