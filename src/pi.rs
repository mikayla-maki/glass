use crate::agent::AgentRuntime;
use crate::events::Event;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

// 30 minutes. Hung Pi runs (network stalls, runaway agent loops) hold the
// turn lock open, blocking every queued DM. Bounded.
const PI_TIMEOUT: Duration = Duration::from_secs(30 * 60);

// Per-argument cap: each `key=value` rendering. Wide enough for a typical
// path or short bash command to fit unfolded.
const ARG_MAX_CHARS: usize = 40;

// Whole argument list cap. Truncates the joined `arg, arg, arg` string.
const ARGS_TOTAL_MAX_CHARS: usize = 80;

pub struct PiRuntime {
    pub command: String,
    pub model_arg: Option<String>,
    pub extra_args: Vec<String>,
}

impl PiRuntime {
    pub fn build_prompt(history: &[Event], user_message: &str) -> String {
        let mut s = String::new();

        let messages = match history.split_first() {
            Some((Event::Summary { body, timestamp }, rest)) => {
                let _ = write!(
                    s,
                    "# Earlier conversation (compacted on {})\n\n{}\n\n---\n\n",
                    timestamp.format("%Y-%m-%d"),
                    body.trim()
                );
                rest
            }
            _ => history,
        };

        if !messages.is_empty() {
            s.push_str("# Recent DM conversation (oldest -> newest)\n\n");
            for e in messages {
                append_message(&mut s, e);
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
            let _ = write!(
                s,
                "# Previous summary (from earlier compactions — fold this in)\n\n{}\n\n",
                prior.trim()
            );
        }
        s.push_str("# Conversation chunk to add into the summary\n\n");
        for e in transcript {
            append_message(&mut s, e);
        }
        s.push_str("\nProduce the new combined summary now. Output only the summary body.");
        s
    }

    async fn invoke(&self, workspace: &Path, prompt: &str, extra: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.command);
        cmd.current_dir(workspace);
        cmd.kill_on_drop(true);
        if let Some(model) = &self.model_arg {
            cmd.arg("--model").arg(model);
        }
        cmd.args(&self.extra_args);
        cmd.args(extra);
        cmd.arg("--mode").arg("json");
        cmd.arg("-p").arg(prompt);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Hold the child inside the timed future. Drop on timeout = kill_on_drop
        // = SIGKILL to Pi. Same applies if a higher-level future cancels us.
        let work = async {
            let mut child = cmd.spawn().with_context(|| {
                format!(
                    "failed to spawn `{}` — is Pi installed and on PATH?",
                    self.command
                )
            })?;
            let stdout = child.stdout.take().context("pi: missing stdout pipe")?;
            let stderr = child.stderr.take().context("pi: missing stderr pipe")?;

            // Drain stderr concurrently so a chatty Pi doesn't block on a full pipe.
            let stderr_task = tokio::spawn(async move {
                let mut buf = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut buf).await;
                buf
            });

            let mut tools = Vec::new();
            let mut texts = Vec::new();
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                process_pi_line(&line, &mut tools, &mut texts);
            }

            let status = child.wait().await?;
            let stderr_buf = stderr_task.await.unwrap_or_default();

            if !status.success() {
                anyhow::bail!("pi exited {}: {}", status, stderr_buf.trim());
            }
            finalize_render(tools, texts)
        };

        match tokio::time::timeout(PI_TIMEOUT, work).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!("pi timed out after {:?}", PI_TIMEOUT),
        }
    }
}

fn append_message(s: &mut String, e: &Event) {
    let (who, content) = match e {
        Event::UserMessage { content, .. } => ("owner", content),
        Event::AgentMessage { content, .. } => ("you", content),
        Event::Summary { .. } => return,
    };
    let _ = write!(
        s,
        "[{} — {who}]\n{}\n\n",
        e.timestamp().format("%Y-%m-%d %H:%M UTC"),
        content.trim()
    );
}

// Walks Pi's `--mode json` event stream and produces a single Discord-ready
// string: tool calls as backticked headers, then the assistant's final text.
// Skips thinking blocks, user echoes, and tool result outputs.
pub fn render_pi_output(stdout: &str) -> Result<String> {
    let mut tools = Vec::new();
    let mut texts = Vec::new();
    for line in stdout.lines() {
        process_pi_line(line, &mut tools, &mut texts);
    }
    finalize_render(tools, texts)
}

fn process_pi_line(line: &str, tools: &mut Vec<String>, texts: &mut Vec<String>) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("skipping malformed pi event: {e}");
            return;
        }
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("message_end") {
        return;
    }
    let Some(msg) = v.get("message") else { return };
    if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return;
    }
    let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
        return;
    };
    for item in content {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("toolCall") => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let args = item.get("arguments").unwrap_or(&serde_json::Value::Null);
                tools.push(format_tool_call(name, args));
            }
            Some("thinking") => {
                // Surface the fact of thinking (without leaking the content)
                // so the owner sees the agent's loop rhythm.
                tools.push("-# Thinking".to_string());
            }
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() {
                        texts.push(trimmed.to_string());
                    }
                }
            }
            _ => {}
        }
    }
}

fn finalize_render(tools: Vec<String>, texts: Vec<String>) -> Result<String> {
    let mut out = String::new();
    for l in &tools {
        out.push_str(l);
        out.push('\n');
    }
    if !texts.is_empty() {
        out.push_str(&texts.join("\n\n"));
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        anyhow::bail!("pi produced no renderable output");
    }
    Ok(trimmed.to_string())
}

fn format_tool_call(name: &str, args: &serde_json::Value) -> String {
    // Discord subtext prefix: renders smaller and dimmer, visually demoting
    // tool calls to metadata next to the assistant's actual answer text.
    // Per-line prefix, so the chunker (line-based) preserves it across splits.
    format!("-# {}({})", pascal_case(name), render_args(args))
}

// snake_case / kebab-case → PascalCase. Bare names get their first letter
// uppercased. Empty input → empty output.
fn pascal_case(s: &str) -> String {
    let mut out = String::new();
    let mut up_next = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            up_next = true;
        } else if up_next {
            out.extend(c.to_uppercase());
            up_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

// Render args as `k=v, k=v` with per-arg and total caps. Single-arg tools
// drop the key entirely (`Read(a.txt)` not `Read(path=a.txt)`) since the
// kind is implied by the tool name. Strings unquoted; other JSON values
// stringify via serde. Control chars in values become spaces so a tool-
// call line stays one line.
fn render_args(args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else {
        return String::new();
    };
    match obj.len() {
        0 => String::new(),
        1 => {
            let (_, v) = obj.iter().next().unwrap();
            truncate_chars(&value_to_string(v), ARG_MAX_CHARS)
        }
        _ => {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| truncate_chars(&format!("{k}={}", value_to_string(v)), ARG_MAX_CHARS))
                .collect();
            truncate_chars(&parts.join(", "), ARGS_TOTAL_MAX_CHARS)
        }
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    let raw = match v {
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    };
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// Truncate to at most `max` chars (Unicode scalar values, not bytes). When
// truncated, ends with literal "..." and the total is exactly `max` chars.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max < 3 {
        return ".".repeat(max);
    }
    let take = max - 3;
    let mut out: String = s.chars().take(take).collect();
    out.push_str("...");
    out
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
        self.invoke(workspace, &prompt, &[]).await
    }

    async fn summarize(
        &self,
        workspace: &Path,
        prior_summary: Option<&str>,
        transcript: &[Event],
    ) -> Result<String> {
        let prompt = Self::build_summary_prompt(prior_summary, transcript);
        // Summarize is a pure transformation; no tool loop wanted.
        self.invoke(workspace, &prompt, &["--no-tools"]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    fn msg_end_assistant(content: serde_json::Value) -> String {
        json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": content }
        })
        .to_string()
    }

    fn msg_end_other(role: &str, text: &str) -> String {
        json!({
            "type": "message_end",
            "message": {
                "role": role,
                "content": [{ "type": "text", "text": text }]
            }
        })
        .to_string()
    }

    #[test]
    fn pascal_case_basic() {
        assert_eq!(pascal_case("bash"), "Bash");
        assert_eq!(pascal_case("read"), "Read");
        assert_eq!(pascal_case("read_file"), "ReadFile");
        assert_eq!(pascal_case("my-custom-tool"), "MyCustomTool");
        assert_eq!(pascal_case("a_b_c"), "ABC");
        assert_eq!(pascal_case(""), "");
    }

    #[test]
    fn truncate_chars_at_or_under_limit_unchanged() {
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn truncate_chars_over_limit_appends_ellipsis_and_hits_exact_length() {
        let out = truncate_chars("hello world this is long", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("..."));
        assert_eq!(out, "hello w...");
    }

    #[test]
    fn render_text_only() {
        let stream = msg_end_assistant(json!([{"type":"text","text":"hello world"}]));
        assert_eq!(render_pi_output(&stream).unwrap(), "hello world");
    }

    #[test]
    fn render_multi_arg_tool_uses_key_value_in_pi_order() {
        // serde_json with preserve_order keeps insertion order.
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "edit", "arguments": {
                "path": "a.txt", "old": "foo", "new": "bar"
            }}
        ]));
        assert_eq!(
            render_pi_output(&stream).unwrap(),
            "-# Edit(path=a.txt, old=foo, new=bar)"
        );
    }

    #[test]
    fn render_single_arg_tool_drops_the_key() {
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "read", "arguments": {"path": "a.txt"}}
        ]));
        assert_eq!(render_pi_output(&stream).unwrap(), "-# Read(a.txt)");
    }

    #[test]
    fn render_pascal_cases_snake_case_tool_names() {
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "read_file", "arguments": {"path": "a.txt"}}
        ]));
        assert_eq!(render_pi_output(&stream).unwrap(), "-# ReadFile(a.txt)");
    }

    #[test]
    fn render_caps_long_value_per_argument() {
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "bash", "arguments": {
                "command": "this is a very long shell command that goes on and on"
            }}
        ]));
        let out = render_pi_output(&stream).unwrap();
        // Single arg drops the key. 40-char cap = 37 char prefix + "...".
        assert_eq!(out, "-# Bash(this is a very long shell command tha...)");
        let inside = out
            .strip_prefix("-# Bash(")
            .unwrap()
            .strip_suffix(")")
            .unwrap();
        assert_eq!(inside.chars().count(), ARG_MAX_CHARS);
    }

    #[test]
    fn render_caps_total_args_at_eighty_chars() {
        // 20 args of `X=1` (3 chars) joined by `, ` = 20*3 + 19*2 = 98 chars.
        // Total cap pulls that down to exactly 80 chars (77 prefix + "...").
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "many", "arguments": {
                "a": "1", "b": "1", "c": "1", "d": "1", "e": "1",
                "f": "1", "g": "1", "h": "1", "i": "1", "j": "1",
                "k": "1", "l": "1", "m": "1", "n": "1", "o": "1",
                "p": "1", "q": "1", "r": "1", "s": "1", "t": "1"
            }}
        ]));
        let out = render_pi_output(&stream).unwrap();
        let body = out
            .strip_prefix("-# Many(")
            .unwrap()
            .strip_suffix(")")
            .unwrap();
        assert_eq!(body.chars().count(), ARGS_TOTAL_MAX_CHARS);
        assert!(body.ends_with("..."));
    }

    #[test]
    fn render_zero_args_produces_empty_parens() {
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "now", "arguments": {}}
        ]));
        assert_eq!(render_pi_output(&stream).unwrap(), "-# Now()");
    }

    #[test]
    fn render_non_string_values_stringify() {
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "limit", "arguments": {
                "count": 42, "verbose": true
            }}
        ]));
        assert_eq!(
            render_pi_output(&stream).unwrap(),
            "-# Limit(count=42, verbose=true)"
        );
    }

    #[test]
    fn render_control_chars_in_values_become_spaces() {
        let stream = msg_end_assistant(json!([
            {"type": "toolCall", "name": "bash", "arguments": {
                "command": "a\nb\tc"
            }}
        ]));
        // \n and \t both become single spaces; output stays one line.
        assert_eq!(render_pi_output(&stream).unwrap(), "-# Bash(a b c)");
    }

    #[test]
    fn render_tool_call_then_final_text() {
        let stream = [
            msg_end_assistant(json!([
                {"type": "toolCall", "name": "read", "arguments": {"path": "a.txt"}}
            ])),
            msg_end_other("toolResult", "alpha\n"),
            msg_end_assistant(json!([{"type": "text", "text": "It says alpha."}])),
        ]
        .join("\n");

        assert_eq!(
            render_pi_output(&stream).unwrap(),
            "-# Read(a.txt)\nIt says alpha."
        );
    }

    #[test]
    fn render_thinking_block_renders_as_subtext_user_messages_skipped() {
        // Thinking content is NOT rendered — just the fact of thinking, so
        // the owner sees the loop rhythm without leaking reasoning.
        let stream = [
            msg_end_other("user", "what's in a.txt?"),
            msg_end_assistant(json!([
                {"type": "thinking", "thinking": "I should read it"},
                {"type": "toolCall", "name": "read", "arguments": {"path": "a.txt"}}
            ])),
            msg_end_assistant(json!([{"type": "text", "text": "alpha"}])),
        ]
        .join("\n");

        assert_eq!(
            render_pi_output(&stream).unwrap(),
            "-# Thinking\n-# Read(a.txt)\nalpha"
        );
    }

    #[test]
    fn render_ignores_malformed_lines_and_empty_lines() {
        let good = msg_end_assistant(json!([{"type": "text", "text": "ok"}]));
        let stream = format!("not json\n\n{good}\n");
        assert_eq!(render_pi_output(&stream).unwrap(), "ok");
    }

    #[test]
    fn render_empty_stream_errors() {
        assert!(render_pi_output("").is_err());
        assert!(render_pi_output(r#"{"type":"agent_start"}"#).is_err());
    }

    #[test]
    fn render_concatenates_multiple_text_chunks_with_blank_line() {
        let stream = [
            msg_end_assistant(json!([{"type": "text", "text": "first"}])),
            msg_end_assistant(json!([{"type": "text", "text": "second"}])),
        ]
        .join("\n");
        assert_eq!(render_pi_output(&stream).unwrap(), "first\n\nsecond");
    }

    #[test]
    fn render_full_recorded_session_shape() {
        // Mirrors the "read a.txt, write d.txt, bash append" turn shape.
        let stream = [
            msg_end_other("user", "do the thing"),
            msg_end_assistant(json!([
                {"type": "toolCall", "name": "read", "arguments": {"path": "a.txt"}},
                {"type": "toolCall", "name": "write", "arguments": {
                    "path": "d.txt", "content": "delta"
                }}
            ])),
            msg_end_other("toolResult", "alpha\n"),
            msg_end_other("toolResult", "Successfully wrote 5 bytes to d.txt"),
            msg_end_assistant(json!([
                {"type": "toolCall", "name": "bash", "arguments": {
                    "command": "printf 'world\\n' >> a.txt && cat a.txt"
                }}
            ])),
            msg_end_other("toolResult", "alpha\nworld\n"),
            msg_end_assistant(json!([
                {"type": "text", "text": "Done. Appended `world` to a.txt and created d.txt."}
            ])),
        ]
        .join("\n");

        assert_eq!(
            render_pi_output(&stream).unwrap(),
            "-# Read(a.txt)\n\
             -# Write(path=d.txt, content=delta)\n\
             -# Bash(printf 'world\\n' >> a.txt && cat a.txt)\n\
             Done. Appended `world` to a.txt and created d.txt."
        );
    }
}
