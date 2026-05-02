# Audit System & Inbox Review — Implementation Plan

Code samples and implementation details for `src/audit/` and `src/inbox/`.

---

## Audit File Writing

```rust
// audit/logger.rs

pub struct AuditLogger {
    audit_dir: PathBuf,
}

impl AuditLogger {
    pub fn new(harness_path: &Path) -> Self {
        let audit_dir = harness_path.join("audit");
        std::fs::create_dir_all(&audit_dir).ok();
        Self { audit_dir }
    }

    /// Write an audit entry to a timestamped JSON file.
    pub fn log(&self, entry: &AuditEntry) -> Result<(), AuditError> {
        let filename = format!(
            "{}_{}.json",
            entry.timestamp.format("%Y-%m-%dT%H-%M-%SZ"),
            sanitize_filename(&entry.trigger.detail),
        );
        let path = self.audit_dir.join(filename);

        let json = serde_json::to_string_pretty(entry)?;
        std::fs::write(&path, json)?;

        Ok(())
    }
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(50)
        .collect()
}
```

---

## Discord Audit Channel

For autonomous invocations (scheduled tasks), the bot asks the agent to produce a summary and posts it to `#audit-log`:

```rust
// audit/discord_log.rs

pub async fn post_audit_summary(
    discord: &dyn DiscordSink,
    audit_channel_id: ChannelId,
    trigger: &InvocationTrigger,
    project_name: &str,
    response_summary: &str,
    timestamp: DateTime<Utc>,
) -> Result<(), AuditError> {
    let (emoji, trigger_label) = match trigger {
        InvocationTrigger::ScheduledTask { task_id, .. } => {
            ("🕐", format!("Scheduled task: `{}`", task_id))
        }
        InvocationTrigger::UserMessage { .. } => return Ok(()), // Don't log user messages
    };

    let message = format!(
        "**{} {}** · #{} · {}\n{}",
        emoji,
        trigger_label,
        project_name,
        timestamp.format("%I:%M %p"),
        response_summary,
    );

    discord.send_message(audit_channel_id, &message).await
        .map_err(|e| AuditError::DiscordPost(e.to_string()))?;
    Ok(())
}
```

---

## Inbox Pipeline

```
Agent calls suggest_learning("Multi-step processes benefit from backward-planning")
  │
  ▼
Bot writes to harness/pending/from-{project}-{timestamp}.json
  │
  ▼
Bot posts to Discord with ✅ Approve · ❌ Reject · ✏️ Edit buttons
  │
  ├─ ✅ Approve → Bot writes to workspace/inbox/{timestamp}-{slug}.md
  ├─ ❌ Reject → Bot deletes from harness/pending/
  └─ ✏️ Edit → Bot opens a modal for the owner to edit, then writes edited version
```

This is the one boundary in the system that is semantic rather than architectural. The LLM decides what to abstract, and there's no firewall for "don't include specifics." The human review step is the mitigation.

---

## Pending Suggestion Format

```rust
// inbox/pending.rs

#[derive(Debug, Serialize, Deserialize)]
pub struct PendingSuggestion {
    pub id: String,  // UUID
    pub timestamp: DateTime<Utc>,
    pub source_project: String,
    pub content: String,
    pub status: SuggestionStatus,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionStatus {
    Pending,
    Approved,
    Rejected,
}

impl PendingSuggestion {
    /// Write this suggestion to the pending directory.
    pub fn save(&self, harness_path: &Path) -> Result<(), InboxError> {
        let pending_dir = harness_path.join("pending");
        std::fs::create_dir_all(&pending_dir)?;

        let filename = format!(
            "from-{}-{}.json",
            self.source_project,
            self.timestamp.format("%Y-%m-%dT%H-%M-%SZ"),
        );
        let path = pending_dir.join(filename);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }
}
```

---

## Approval Handler

```rust
// inbox/approval.rs

/// On approval, write the suggestion content to workspace/inbox/.
pub fn approve_suggestion(
    suggestion: &PendingSuggestion,
    workspace_root: &Path,
) -> Result<(), InboxError> {
    let inbox_dir = workspace_root.join("inbox");
    std::fs::create_dir_all(&inbox_dir)?;

    let slug = suggestion.content.chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .take(40)
        .collect::<String>()
        .trim()
        .replace(' ', "-")
        .to_lowercase();

    let filename = format!(
        "{}-{}.md",
        suggestion.timestamp.format("%Y-%m-%d"),
        slug,
    );

    let content = format!(
        "# {}\n\n*Source: #{} — {}*\n\n{}\n",
        suggestion.content.lines().next().unwrap_or("Learning"),
        suggestion.source_project,
        suggestion.timestamp.format("%Y-%m-%d %H:%M UTC"),
        suggestion.content,
    );

    std::fs::write(inbox_dir.join(filename), content)?;
    Ok(())
}

/// On rejection, delete the pending file.
pub fn reject_suggestion(
    suggestion: &PendingSuggestion,
    harness_path: &Path,
) -> Result<(), InboxError> {
    let pending_dir = harness_path.join("pending");
    let filename = format!(
        "from-{}-{}.json",
        suggestion.source_project,
        suggestion.timestamp.format("%Y-%m-%dT%H-%M-%SZ"),
    );
    std::fs::remove_file(pending_dir.join(filename)).ok();
    Ok(())
}
```

---

## Discord Review UI

```rust
// inbox/review.rs

/// Post a suggestion to Discord for human review.
pub async fn post_for_review(
    discord: &dyn DiscordSink,
    review_channel_id: ChannelId,  // #glass or a dedicated #inbox channel
    suggestion: &PendingSuggestion,
) -> Result<(), InboxError> {
    let content = format!(
        "**📬 Inbox review** (from #{})\n\
         *\"{}\"*",
        suggestion.source_project,
        suggestion.content,
    );

    let buttons = vec![
        ReviewButton {
            custom_id: format!("inbox_approve:{}", suggestion.id),
            label: "Approve".to_string(),
            style: ButtonStyle::Success,
        },
        ReviewButton {
            custom_id: format!("inbox_reject:{}", suggestion.id),
            label: "Reject".to_string(),
            style: ButtonStyle::Danger,
        },
        ReviewButton {
            custom_id: format!("inbox_edit:{}", suggestion.id),
            label: "Edit".to_string(),
            style: ButtonStyle::Secondary,
        },
    ];

    discord.send_review_message(review_channel_id, &content, buttons).await
        .map_err(|e| InboxError::Discord(e.to_string()))?;
    Ok(())
}
```

---

## Key Design Decisions

- **Audit files are JSON, not a database.** Grep-able, jq-parseable, git-trackable. No migration headaches.
- **Atomic writes via temp-file-then-rename** should be used if concurrent invocations become a concern.
- **User-initiated invocations are NOT posted to `#audit-log`** — only autonomous (scheduled) actions generate Discord summaries. All invocations get JSON files regardless.
- **The inbox is a trust boundary, not a technical boundary.** The agent decides what to suggest; the human decides what to keep. There's no code that can prevent the agent from including project-specific details in a suggestion — the review step is the mitigation.