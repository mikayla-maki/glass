use crate::agent::AgentRuntime;
use crate::events::Event;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

// 30 minutes. Hung Pi runs (network stalls, runaway agent loops) hold the
// turn lock open, blocking every queued DM. Bounded.
const PI_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct PiRuntime {
    pub command: String,
    pub model_arg: Option<String>,
    pub extra_args: Vec<String>,
}

impl PiRuntime {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            model_arg: None,
            extra_args: vec![],
        }
    }

    pub fn with_model(mut self, model_arg: Option<String>) -> Self {
        self.model_arg = model_arg;
        self
    }

    pub fn with_extra_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    pub fn build_prompt(history: &[Event], user_message: &str) -> String {
        let mut s = String::new();

        let mut messages_start = 0;
        if let Some(Event::Summary { body, timestamp }) = history.first() {
            s.push_str(&format!(
                "# Earlier conversation (compacted on {})\n\n",
                timestamp.format("%Y-%m-%d")
            ));
            s.push_str(body.trim());
            s.push_str("\n\n---\n\n");
            messages_start = 1;
        }

        let messages = &history[messages_start..];
        if !messages.is_empty() {
            s.push_str("# Recent DM conversation (oldest -> newest)\n\n");
            for e in messages {
                match e {
                    Event::UserMessage { timestamp, content } => {
                        s.push_str(&format!(
                            "[{} — owner]\n{}\n\n",
                            timestamp.format("%Y-%m-%d %H:%M UTC"),
                            content.trim()
                        ));
                    }
                    Event::AgentMessage { timestamp, content } => {
                        s.push_str(&format!(
                            "[{} — you]\n{}\n\n",
                            timestamp.format("%Y-%m-%d %H:%M UTC"),
                            content.trim()
                        ));
                    }
                    Event::Summary { .. } => {} // unreachable in active window
                }
            }
            s.push_str("---\n\n");
        }

        s.push_str("# Owner just sent\n\n");
        s.push_str(user_message.trim());
        s.push_str(
            "\n\nRespond as Glass. Discord DM register — conversational, not a doc. \
             AGENTS.md tells you who you are and where memory lives.\n",
        );
        s
    }

    pub fn build_summary_prompt(prior_summary: Option<&str>, transcript: &[Event]) -> String {
        let mut s = String::new();
        s.push_str(
            "You are summarizing your own past conversation with the owner so it fits in your \
             context window going forward. Produce a thorough but compact summary capturing:\n\
             - Names, dates, places, projects, and other concrete facts mentioned\n\
             - Decisions made and conclusions reached\n\
             - Ongoing threads / open questions\n\
             - The owner's current concerns and emotional register\n\
             - Anything you committed to or said you'd remember\n\n\
             Write in your own voice, first person (\"I told her...\", \"we figured out...\"). \
             It will appear at the start of your context next turn as 'earlier conversation', \
             so write it for future-you, not for the owner.\n\n",
        );
        if let Some(prior) = prior_summary {
            s.push_str("# Previous summary (from earlier compactions — fold this in)\n\n");
            s.push_str(prior.trim());
            s.push_str("\n\n");
        }
        s.push_str("# Conversation chunk to add into the summary\n\n");
        for e in transcript {
            match e {
                Event::UserMessage { timestamp, content } => {
                    s.push_str(&format!(
                        "[{} — owner]\n{}\n\n",
                        timestamp.format("%Y-%m-%d %H:%M UTC"),
                        content.trim()
                    ));
                }
                Event::AgentMessage { timestamp, content } => {
                    s.push_str(&format!(
                        "[{} — you]\n{}\n\n",
                        timestamp.format("%Y-%m-%d %H:%M UTC"),
                        content.trim()
                    ));
                }
                Event::Summary { .. } => {} // caller filters these out
            }
        }
        s.push_str("\nProduce the new combined summary now. Output only the summary body.");
        s
    }

    async fn invoke(&self, workspace: &Path, prompt: &str) -> Result<String> {
        let mut cmd = Command::new(&self.command);
        cmd.current_dir(workspace);
        cmd.kill_on_drop(true);
        if let Some(model) = &self.model_arg {
            cmd.arg("--model").arg(model);
        }
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        cmd.arg("-p").arg(prompt);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn `{}` — is Pi installed and on PATH?",
                self.command
            )
        })?;

        // Drop on timeout sends SIGTERM via kill_on_drop.
        let out = match tokio::time::timeout(PI_TIMEOUT, child.wait_with_output()).await {
            Ok(r) => r?,
            Err(_) => anyhow::bail!("pi timed out after {:?}", PI_TIMEOUT),
        };

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("pi exited {}: {}", out.status, stderr.trim());
        }

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stdout.is_empty() {
            anyhow::bail!("pi returned an empty response");
        }
        Ok(stdout)
    }
}

#[async_trait]
impl AgentRuntime for PiRuntime {
    async fn run_turn(
        &self,
        workspace: &Path,
        history: &[Event],
        user_message: &str,
    ) -> Result<String> {
        let prompt = Self::build_prompt(history, user_message);
        self.invoke(workspace, &prompt).await
    }

    async fn summarize(
        &self,
        workspace: &Path,
        prior_summary: Option<&str>,
        transcript: &[Event],
    ) -> Result<String> {
        // Defensive filter — the caller in compaction.rs already strips
        // summaries, but better to be safe than to feed Pi a weird input.
        let cleaned: Vec<Event> = transcript
            .iter()
            .filter(|e| !e.is_summary())
            .cloned()
            .collect();
        let prompt = Self::build_summary_prompt(prior_summary, &cleaned);
        self.invoke(workspace, &prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(rfc: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc).unwrap().to_utc()
    }

    fn user(content: &str) -> Event {
        Event::UserMessage {
            timestamp: ts("2025-11-01T09:00:00Z"),
            content: content.into(),
        }
    }
    fn agent(content: &str) -> Event {
        Event::AgentMessage {
            timestamp: ts("2025-11-01T09:00:30Z"),
            content: content.into(),
        }
    }
    fn summary(body: &str) -> Event {
        Event::Summary {
            timestamp: ts("2025-10-15T14:30:00Z"),
            body: body.into(),
        }
    }

    /// Helper: index of the first occurrence of `needle` in `haystack`, or
    /// fail the test with a clear message.
    fn index_of(haystack: &str, needle: &str) -> usize {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("expected {needle:?} in prompt:\n{haystack}"))
    }

    #[test]
    fn run_prompt_no_history_has_just_the_owner_message_section() {
        let prompt = PiRuntime::build_prompt(&[], "hello");
        assert!(!prompt.contains("Recent DM conversation"));
        assert!(!prompt.contains("Earlier conversation"));
        assert!(prompt.contains("# Owner just sent"));
        assert!(prompt.contains("hello"));
    }

    #[test]
    fn run_prompt_orders_history_then_new_message() {
        let history = vec![user("earlier user msg"), agent("earlier agent reply")];
        let prompt = PiRuntime::build_prompt(&history, "current msg");

        let history_pos = index_of(&prompt, "earlier user msg");
        let current_pos = index_of(&prompt, "current msg");
        assert!(history_pos < current_pos);
        assert!(prompt.contains("Recent DM conversation"));
    }

    #[test]
    fn run_prompt_orders_summary_then_recent_then_new_message() {
        let history = vec![
            summary("earlier we discussed cats"),
            user("and dogs?"),
            agent("yes also dogs"),
        ];
        let prompt = PiRuntime::build_prompt(&history, "and turtles?");

        let summary_pos = index_of(&prompt, "earlier we discussed cats");
        let recent_pos = index_of(&prompt, "and dogs?");
        let new_pos = index_of(&prompt, "and turtles?");
        assert!(summary_pos < recent_pos);
        assert!(recent_pos < new_pos);
        assert!(prompt.contains("Earlier conversation"));
    }

    #[test]
    fn summary_prompt_with_no_prior_omits_previous_summary_section() {
        let transcript = vec![user("a"), agent("b")];
        let prompt = PiRuntime::build_summary_prompt(None, &transcript);
        assert!(!prompt.contains("Previous summary"));
        assert!(prompt.contains("a"));
        assert!(prompt.contains("b"));
    }

    #[test]
    fn summary_prompt_carries_prior_forward() {
        let transcript = vec![user("new user msg"), agent("new agent reply")];
        let prompt = PiRuntime::build_summary_prompt(Some("prior summary text"), &transcript);

        let prior_pos = index_of(&prompt, "prior summary text");
        let user_pos = index_of(&prompt, "new user msg");
        assert!(prior_pos < user_pos);
        assert!(prompt.contains("Previous summary"));
    }
}
