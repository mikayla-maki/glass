# MCP Tool Server — Implementation Plan

**Module:** `src/mcp/`
**Responsibility:** Serve Glass tools to Claude Code via the Model Context Protocol. All tool execution, path scoping, and capability enforcement happens here. This is the primary security boundary.

## Architecture

Glass runs an MCP server as a subprocess that Claude Code spawns. The main Glass binary has a `serve-mcp` subcommand:

```
glass serve-mcp --project surgery-prep --workspace /path/to/workspace/surgery-prep \
    --allowed-paths '["..."]' --network-capability search
```

Claude Code's MCP config (generated per-invocation by `agent/runtime.rs`) tells it to launch this server. The MCP server communicates with Claude Code over stdin/stdout using the MCP protocol. All tool calls flow through it.

**Why a subprocess?** MCP's standard transport is stdin/stdout between a client (Claude Code) and a server process. Glass reuses its own binary with a different entry point. The MCP server subprocess shares all the production code (tool dispatch, path scoping, Docker exec) with the main binary — it's the same crate, same compiled code, different `main` path.

## Tool Dispatch

The MCP server exposes tools and handles calls. The tool set is determined by the `--network-capability` and other flags passed at launch — the main Glass process already decided which tools this invocation gets.

```rust
// mcp/tools.rs

/// Central tool executor. Dispatches MCP tool calls to the correct handler.
/// This is the same dispatch logic whether running in the MCP server subprocess
/// or called directly in tests.
pub struct ToolExecutor {
    workspace_root: PathBuf,
    harness_path: PathBuf,
    brave_api_key: Option<String>,
}

impl ToolExecutor {
    pub fn new(
        workspace_root: PathBuf,
        harness_path: PathBuf,
        brave_api_key: Option<String>,
    ) -> Self {
        Self { workspace_root, harness_path, brave_api_key }
    }

    /// Execute a tool call. Returns the string result or an error.
    /// Called by the MCP server when Claude Code invokes a tool.
    pub async fn execute(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        ctx: &McpContext,
        sandbox: &dyn Sandbox,
    ) -> Result<String, ToolError> {
        match tool_name {
            "shell" => self.exec_shell(input, ctx, sandbox).await,
            "read_file" => self.exec_read_file(input, ctx).await,
            "write_file" => self.exec_write_file(input, ctx).await,
            "list_files" => self.exec_list_files(input, ctx).await,
            "fetch_url" => self.exec_fetch_url(input, ctx).await,
            "web_search" => self.exec_web_search(input, ctx).await,
            "suggest_learning" => self.exec_suggest_learning(input, ctx).await,
            "query_projects" => self.exec_query_projects(input, ctx).await,
            "list_projects" => self.exec_list_projects(input, ctx).await,
            "create_project" => self.exec_create_project(input, ctx).await,
            "archive_project" => self.exec_archive_project(input, ctx).await,
            "rename_project" => self.exec_rename_project(input, ctx).await,
            _ => Err(ToolError::UnknownTool(tool_name.to_string())),
        }
    }
}

/// Context available to MCP tool handlers. Constructed from CLI args
/// passed to `glass serve-mcp`.
pub struct McpContext {
    pub project_name: String,
    pub workspace_path: PathBuf,
    pub allowed_paths: Vec<PathBuf>,
    pub network_capability: NetworkCapability,
    pub owner_present: bool,
}
```

## Tool Registration

Claude Code discovers available tools from the MCP server at startup. The MCP server registers only the tools appropriate for this invocation (already filtered by the main Glass process based on channel capabilities):

```rust
// mcp/server.rs

/// Register MCP tools based on the invocation's capabilities.
fn register_tools(capability: &NetworkCapability, is_root: bool, owner_present: bool) -> Vec<McpToolDef> {
    let mut tools = vec![
        shell_tool_def(),
        read_file_tool_def(),
        write_file_tool_def(),
        list_files_tool_def(),
        suggest_learning_tool_def(),
    ];

    // Network tools — gated by capability
    match capability {
        NetworkCapability::Open | NetworkCapability::Allowlist => {
            tools.push(fetch_url_tool_def());
            tools.push(web_search_tool_def());
        }
        NetworkCapability::Search => {
            tools.push(web_search_tool_def());
        }
        NetworkCapability::None => {}
    }

    // Root-only tools
    if is_root {
        tools.push(list_projects_tool_def());
        if owner_present {
            tools.push(create_project_tool_def());
            tools.push(archive_project_tool_def());
            tools.push(rename_project_tool_def());
        }
        if !owner_present {
            tools.push(query_projects_tool_def());
        }
    }

    tools
}
```

**Note:** When `query_projects` is invoked, the MCP handler returns a message like *"Queries dispatched. This session will end and a new session will resume with the results."* The bot detects `query_projects` in the completed session's tool call records and initiates the two-phase dispatch described in the Agent Runtime plan. No side-channel needed.

## Path Scoping

The `read_file`, `write_file`, and `list_files` tools enforce workspace scoping. This uses **lexical path normalization** (not `canonicalize`) to correctly handle paths to files that don't exist yet:

```rust
// mcp/scoping.rs

/// Normalize a path by resolving `.` and `..` components lexically,
/// without touching the filesystem. This is safe for paths that don't
/// exist yet (e.g., write_file to a new file).
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => { components.pop(); }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Resolve a relative path to an absolute path within the allowed workspace,
/// rejecting any path traversal attempts.
fn resolve_scoped_path(
    relative_path: &str,
    allowed_root: &Path,
) -> Result<PathBuf, ToolError> {
    let requested = allowed_root.join(relative_path);
    let normalized = normalize_path(&requested);

    // Ensure the normalized path is within the allowed root
    if !normalized.starts_with(allowed_root) {
        return Err(ToolError::PathTraversal {
            requested: relative_path.to_string(),
            allowed_root: allowed_root.display().to_string(),
        });
    }

    Ok(normalized)
}
```

## Web Tool Implementation

```rust
// mcp/tools.rs

impl ToolExecutor {
    pub async fn exec_fetch_url(
        &self,
        input: &serde_json::Value,
        ctx: &McpContext,
    ) -> Result<String, ToolError> {
        let url = input["url"].as_str()
            .ok_or(ToolError::MissingParam("url"))?;

        // Defense in depth — MCP server should only be started with
        // fetch_url registered if capability allows it, but double-check.
        match ctx.network_capability {
            NetworkCapability::None => {
                return Err(ToolError::CapabilityDenied("fetch_url not available"));
            }
            NetworkCapability::Search => {
                return Err(ToolError::CapabilityDenied("fetch_url not available (search only)"));
            }
            NetworkCapability::Allowlist => {
                // Validate URL against approved domains
                let parsed = url::Url::parse(url)
                    .map_err(|_| ToolError::InvalidUrl(url.to_string()))?;
                let host = parsed.host_str()
                    .ok_or(ToolError::InvalidUrl(url.to_string()))?;

                let caps = load_capabilities(&self.harness_path)?;
                let project_config = caps.get_project(&ctx.project_name);

                if !project_config.allowed_domains.iter().any(|d| host.ends_with(d)) {
                    return Err(ToolError::DomainNotAllowed {
                        domain: host.to_string(),
                        project: ctx.project_name.clone(),
                    });
                }
            }
            NetworkCapability::Open => {
                // No restrictions
            }
        }

        // Execute the fetch on the host
        let response = reqwest::get(url).await
            .map_err(|e| ToolError::FetchFailed(e.to_string()))?;

        let body = response.text().await
            .map_err(|e| ToolError::FetchFailed(e.to_string()))?;

        // Truncate very large responses to avoid context explosion
        let max_chars = 100_000;
        if body.len() > max_chars {
            Ok(format!("{}\n\n[Truncated: response was {} chars, showing first {}]",
                &body[..max_chars], body.len(), max_chars))
        } else {
            Ok(body)
        }
    }

    pub async fn exec_web_search(
        &self,
        input: &serde_json::Value,
        ctx: &McpContext,
    ) -> Result<String, ToolError> {
        let query = input["query"].as_str()
            .ok_or(ToolError::MissingParam("query"))?;

        let api_key = self.brave_api_key.as_ref()
            .ok_or(ToolError::ConfigError("BRAVE_SEARCH_API_KEY not set".into()))?;

        let client = reqwest::Client::new();
        let response = client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", api_key.as_str())
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", "10")])
            .send()
            .await
            .map_err(|e| ToolError::FetchFailed(e.to_string()))?;

        let body: serde_json::Value = response.json().await
            .map_err(|e| ToolError::FetchFailed(e.to_string()))?;

        // Format results into readable text
        let mut output = String::new();
        if let Some(results) = body["web"]["results"].as_array() {
            for (i, result) in results.iter().enumerate() {
                let title = result["title"].as_str().unwrap_or("Untitled");
                let url = result["url"].as_str().unwrap_or("");
                let description = result["description"].as_str().unwrap_or("");

                output.push_str(&format!(
                    "{}. **{}**\n   {}\n   {}\n\n",
                    i + 1, title, url, description
                ));
            }
        }

        if output.is_empty() {
            Ok("No results found.".to_string())
        } else {
            Ok(output)
        }
    }
}
```

## Audit Logging

Tool call records (tool name, args, result, duration) are extracted from the completed Claude Code session result via `claude-sdk-rs`. The bot builds the full `AuditEntry` by combining session metadata (`InvocationContext`) with the tool call records and timing data from the session result. No MCP-side audit logging or tempfile is needed — the data flows through the same path the bot already uses to get the agent's response.
