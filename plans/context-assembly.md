# Context Assembly — Implementation Plan

**Module:** `src/context/`
**Responsibility:** Build the system prompt for each invocation type. Claude Code handles conversation history and context window management internally.

---

## Context Shapes

There are three distinct context shapes (per the spec):

### 1. Regular Project Invocation

```
System prompt:
  ┌─ Agent identity (workspace/identity.md)
  ├─ Skill metadata (name + description from SKILL.md frontmatter)
  ├─ Project brief (project/brief.md)
  ├─ Project status (project/status.md)
  └─ Project workspace file listing

Prompt:
  └─ User message content

Scoping (enforced by MCP server):
  ├─ read/write → project workspace subdirectory only
  ├─ Network tools → governed by channel capabilities
  └─ suggest_learning → always available
```

### 2. Root Project — Owner Present

```
System prompt:
  ┌─ Agent identity (workspace/identity.md)
  ├─ All skill metadata
  └─ Root workspace file listing (includes project subdirectories)

Prompt:
  └─ User message content

Scoping (enforced by MCP server):
  ├─ read/write → full workspace (root + all projects)
  ├─ Network → open
  └─ Admin tools available (create/archive/rename project)
```

### 3. Root Project — Autonomous (Scheduled Task)

```
System prompt:
  ┌─ Agent identity (workspace/identity.md)
  ├─ Skill metadata
  ├─ Root workspace file listing
  └─ Project list with basic metadata

Prompt:
  └─ Task description

Scoping (enforced by MCP server):
  ├─ read/write → root-level workspace files only
  ├─ Network → open
  ├─ query_projects available (triggers two-phase dispatch)
  └─ Cannot read project workspaces directly
```

---

## System Prompt Assembly

```rust
// context/assembly.rs

pub fn assemble_system_prompt(
    identity: &str,              // Contents of identity.md
    skills: &[DiscoveredSkill],  // Skill metadata for progressive disclosure
    project_context: &ProjectContext, // Brief, status, file listing
) -> String {
    let mut prompt = String::new();

    // Identity section
    prompt.push_str("# Identity\n\n");
    prompt.push_str(identity);
    prompt.push_str("\n\n");

    // Skills section (metadata only — name + description)
    if !skills.is_empty() {
        prompt.push_str("# Available Skills\n\n");
        for skill in skills {
            prompt.push_str(&format!(
                "- **{}**: {}\n",
                skill.metadata.name,
                skill.metadata.description
            ));
        }
        prompt.push_str("\n");
    }

    // Project context section
    prompt.push_str("# Current Project\n\n");
    prompt.push_str(&format!("**Project:** {}\n\n", project_context.name));

    if let Some(brief) = &project_context.brief {
        prompt.push_str("## Brief\n\n");
        prompt.push_str(brief);
        prompt.push_str("\n\n");
    }

    if let Some(status) = &project_context.status {
        prompt.push_str("## Current Status\n\n");
        prompt.push_str(status);
        prompt.push_str("\n\n");
    }

    // Workspace file listing
    prompt.push_str("## Workspace Files\n\n```\n");
    prompt.push_str(&project_context.file_listing);
    prompt.push_str("\n```\n\n");

    prompt
}
```

---

## Conversation History

Claude Code manages conversation state internally via its session system. Glass does **not** track conversation history — it passes the system prompt and the current user message (or task description) to Claude Code, and Claude Code handles multi-turn context, compaction, and summarization.

For continuing conversations within a Discord channel, Glass passes the `resume_session_id` from the previous invocation's `InvocationResult` back into the next `InvocationContext`. Claude Code resumes the session with full context preserved.

**Session persistence:** Claude Code stores session data on the host filesystem (in its own data directory, not in Glass's workspace or harness). Sessions survive bot restarts. The agent also has workspace files (brief.md, status.md, notes) for persistent context that outlives any individual session.