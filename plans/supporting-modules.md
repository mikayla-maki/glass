# Supporting Modules — Skills, Git & Config

Code samples and implementation details for the smaller supporting modules.

---

## Skill Discovery

**Module:** `src/skills/`
**Responsibility:** Scan SKILL.md files from workspace/skills/ and project skill directories, parse YAML frontmatter, and provide metadata for context assembly.

### Discovery Implementation

```rust
// skills/discovery.rs

/// Scan all skill directories and return metadata for progressive disclosure.
pub fn discover_skills(
    workspace_root: &Path,
    project: Option<&Project>,
) -> Vec<DiscoveredSkill> {
    let mut skills = Vec::new();

    // Global skills: workspace/skills/*/SKILL.md
    let global_skills_dir = workspace_root.join("skills");
    if global_skills_dir.exists() {
        for entry in walkdir::WalkDir::new(&global_skills_dir)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().is_dir() {
                let skill_md = entry.path().join("SKILL.md");
                if let Some(meta) = parse_skill_frontmatter(&skill_md) {
                    skills.push(DiscoveredSkill {
                        metadata: meta,
                        path: entry.path().strip_prefix(workspace_root)
                            .unwrap_or(entry.path()).to_path_buf(),
                        global: true,
                    });
                }
            }
        }
    }

    // Project-local skills: {project}/skills/*/SKILL.md
    if let Some(project) = project {
        let project_skills_dir = project.workspace_path.join("skills");
        if project_skills_dir.exists() {
            for entry in walkdir::WalkDir::new(&project_skills_dir)
                .min_depth(1)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if entry.path().is_dir() {
                    let skill_md = entry.path().join("SKILL.md");
                    if let Some(meta) = parse_skill_frontmatter(&skill_md) {
                        skills.push(DiscoveredSkill {
                            metadata: meta,
                            path: entry.path().strip_prefix(workspace_root)
                                .unwrap_or(entry.path()).to_path_buf(),
                            global: false,
                        });
                    }
                }
            }
        }
    }

    skills
}

/// Parse YAML frontmatter from a SKILL.md file.
/// Returns None if the file doesn't exist or can't be parsed.
fn parse_skill_frontmatter(path: &Path) -> Option<SkillMetadata> {
    let content = std::fs::read_to_string(path).ok()?;

    // YAML frontmatter is between --- delimiters
    if !content.starts_with("---") {
        return None;
    }

    let end = content[3..].find("---")?;
    let yaml_str = &content[3..3 + end];

    serde_yaml::from_str(yaml_str).ok()
}
```

**Context cost:** ~100 tokens per skill for metadata. With 20 skills, that's ~2K tokens added to every invocation — manageable.

---

## Git Integration

**Module:** `src/git/`
**Responsibility:** Auto-commit workspace changes after each agent invocation so every modification is tracked and reversible.

### Auto-Sync Implementation

```rust
// git/sync.rs

use tokio::process::Command;

/// After an agent invocation, commit any workspace changes.
/// Uses a descriptive commit message based on the invocation.
pub async fn auto_commit(
    workspace_root: &Path,
    project_name: &str,
    trigger_description: &str,
) -> Result<(), GitError> {
    // Check if there are any changes
    let status = Command::new("git")
        .current_dir(workspace_root)
        .args(["status", "--porcelain"])
        .output()
        .await?;

    let changes = String::from_utf8_lossy(&status.stdout);
    if changes.trim().is_empty() {
        return Ok(()); // Nothing to commit
    }

    // Stage all changes
    Command::new("git")
        .current_dir(workspace_root)
        .args(["add", "-A"])
        .output()
        .await?;

    // Commit with descriptive message
    let message = format!(
        "[glass] {} — {}",
        project_name,
        trigger_description,
    );

    Command::new("git")
        .current_dir(workspace_root)
        .args(["commit", "-m", &message, "--no-gpg-sign"])
        .output()
        .await?;

    Ok(())
}

/// Initialize the workspace as a git repo if it isn't one already.
pub async fn ensure_git_repo(workspace_root: &Path) -> Result<(), GitError> {
    if !workspace_root.join(".git").exists() {
        Command::new("git")
            .current_dir(workspace_root)
            .args(["init"])
            .output()
            .await?;

        // Create .gitignore for common artifacts
        let gitignore = "*.pyc\n__pycache__/\nnode_modules/\n.DS_Store\n";
        std::fs::write(workspace_root.join(".gitignore"), gitignore)?;

        Command::new("git")
            .current_dir(workspace_root)
            .args(["add", "-A"])
            .output()
            .await?;

        Command::new("git")
            .current_dir(workspace_root)
            .args(["commit", "-m", "[glass] Initial workspace setup", "--no-gpg-sign"])
            .output()
            .await?;
    }

    Ok(())
}
```

---

## Configuration

**Module:** `src/config.rs`
**Responsibility:** Load environment variables and validate configuration at startup.

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

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `DISCORD_BOT_TOKEN` | ✅ | — | Bot token from Discord Developer Portal |
| `DISCORD_GUILD_ID` | ✅ | — | The Discord server ID |
| `ANTHROPIC_API_KEY` | ✅ | — | API key for Claude |
| `BRAVE_SEARCH_API_KEY` | ❌ | — | For `web_search` tool |
| `WORKSPACE_PATH` | ❌ | `./glass-data/workspace` | Agent workspace root |
| `HARNESS_PATH` | ❌ | `./glass-data/harness` | Bot-only data (audit, pending, config) |
| `DOCKER_IMAGE` | ❌ | `glass-sandbox` | Sandbox container image name |
| `CLAUDE_MODEL` | ❌ | `claude-sonnet-4-20250514` | Model for agent invocations |
| `OWNER_USER_ID` | ❌ | — | Discord user ID for owner presence detection |