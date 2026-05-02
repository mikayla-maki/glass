# Channel Capabilities — Implementation Plan

**Module:** `src/capabilities/`
**Responsibility:** Load, parse, and enforce per-project network capability tiers and domain allowlists.

---

## Configuration File Format

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

[channels.side-project]
network = "allowlist"
allowed_domains = ["github.com", "docs.rs", "crates.io"]
integrations = ["linear"]
```

---

## Parsing

```rust
// capabilities/config.rs

#[derive(Debug, Deserialize)]
pub struct CapabilitiesFile {
    pub default: ChannelConfig,
    #[serde(default)]
    pub channels: HashMap<String, ChannelConfig>,
}

impl CapabilitiesFile {
    pub fn load(harness_path: &Path) -> Result<Self, ConfigError> {
        let path = harness_path.join("channel_capabilities.toml");
        let content = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&content)?)
    }

    pub fn get_project(&self, project_name: &str) -> &ChannelConfig {
        self.channels.get(project_name).unwrap_or(&self.default)
    }
}
```

---

## Tool Filtering

The capabilities module determines which tool **names** are allowed for a given invocation. This produces the `allowed_tools: Vec<String>` field in `InvocationContext`, which is passed to both `claude-sdk-rs` (to restrict Claude Code's tool access) and to the MCP server CLI args (to control which tools are registered). This is defense in depth — two independent enforcement points.

```rust
// capabilities/filter.rs

/// Given a project's capabilities, return the list of tool names
/// that should be allowed for this invocation. This list feeds into
/// InvocationContext.allowed_tools and the MCP server's tool registration.
pub fn allowed_tools_for_project(
    capability: &ChannelConfig,
    is_root: bool,
    owner_present: bool,
) -> Vec<String> {
    let mut tools = vec![
        "shell".to_string(),
        "read_file".to_string(),
        "write_file".to_string(),
        "list_files".to_string(),
        "suggest_learning".to_string(),
    ];

    // Network-gated tools
    match capability.network {
        NetworkCapability::Open | NetworkCapability::Allowlist => {
            tools.push("fetch_url".to_string());
            tools.push("web_search".to_string());
        }
        NetworkCapability::Search => {
            tools.push("web_search".to_string());
        }
        NetworkCapability::None => {}
    }

    // Root-only tools
    if is_root {
        tools.push("list_projects".to_string());
        if owner_present {
            tools.push("create_project".to_string());
            tools.push("archive_project".to_string());
            tools.push("rename_project".to_string());
        }
        if !owner_present {
            tools.push("query_projects".to_string());
        }
    }

    tools
}
```

---

## Domain Approval Flow

When the agent calls `fetch_url` with an allowlisted project and the domain isn't approved:

1. The tool executor returns a domain-not-allowed error to the agent.
2. The agent may then be prompted (by system prompt guidance) to explain why it needs the domain.
3. If the agent has been instructed about the domain request flow, it can mention needing the domain in its response.
4. **Alternatively**, the bot can detect domain-not-allowed errors and proactively post a domain request to Discord:

```
🌐 Domain request (from #surgery-prep)
Agent wants to access: mayoclinic.org
Reason: "Looking up post-operative care guidelines"

✅ Approve · ❌ Reject
```

On approval, the bot appends the domain to `channel_capabilities.toml` and the agent can use it in future invocations.

---

## Test Targets

- `none` → no network tools registered in MCP server
- `search` → `web_search` only, `fetch_url` rejected
- `allowlist` → `fetch_url` with domain validation, unapproved domains rejected
- `open` → unrestricted `fetch_url`
- Default fallback when project not in config
- Domain matching edge cases (subdomain, trailing dot, etc.)