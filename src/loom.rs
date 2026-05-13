use crate::invocation_log::InvocationLog;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

// 30 minutes. Hung runs (network stalls, runaway agent loops) hold the turn
// open, blocking every queued DM. Bounded.
const LOOM_TIMEOUT: Duration = Duration::from_secs(30 * 60);

// Per-argument cap when rendering tool-call args. Wide enough for a typical
// path or short bash command to fit unfolded.
const ARG_MAX_CHARS: usize = 60;

// Whole argument list cap. Truncates the joined `arg, arg, arg` string.
const ARGS_TOTAL_MAX_CHARS: usize = 140;

// How long a tool-call batch may sit pending before we flush it to Discord
// even without a text chunk to mark the boundary. Keeps a long run of tool
// calls from sitting invisible until the next text — but is wide enough that
// a rapid burst of tool calls coalesces into one message rather than spamming
// Discord's ~5 msg/5s DM rate limit.
const TOOL_COALESCE: Duration = Duration::from_millis(1500);

// Streaming agent runner. `tx.send(s).await` posts `s` to Discord as a
// distinct DM. The optional `log` receives every raw stdout line from Loom
// (preamble + SessionUpdates) for post-hoc forensics; the caller manages
// the log's lifecycle so cancellation can still write a clean footer.
// The runner returns `Ok(())` on a clean end-of-turn, an `Err` on
// tool/model failures. Cancellation is `kill_on_drop = true` on the child
// process; dropping the future closes `tx` and the bus drains the receiver.
#[async_trait]
pub trait LoomRunner: Send + Sync {
    async fn run(
        &self,
        manifest: &Path,
        prompt: &str,
        tx: mpsc::Sender<String>,
        log: Option<&mut InvocationLog>,
    ) -> Result<()>;
}

pub struct LoomCli {
    pub command: String,
    /// Extra env vars set on every spawned loom subprocess. Used to deliver
    /// runtime-resolved values like `GLASS_ORCHESTRATOR_SOCK` to companion
    /// tools via Loom's `EnvSecretsStore` pipeline.
    pub extra_env: Vec<(String, String)>,
}

impl LoomCli {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            extra_env: Vec::new(),
        }
    }

    pub fn with_env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), val.into()));
        self
    }

    async fn invoke(
        &self,
        manifest: &Path,
        prompt: &str,
        tx: mpsc::Sender<String>,
        mut log: Option<&mut InvocationLog>,
    ) -> Result<()> {
        // Canonicalize so the path Loom sees is unambiguous regardless of
        // process cwd. Loom resolves manifest-relative paths (like
        // `[providers].identity = { path = "../providers/glass-identity" }`)
        // against the manifest's own directory; we don't need to set cwd.
        let manifest_abs = std::fs::canonicalize(manifest)
            .with_context(|| format!("manifest not found: {}", manifest.display()))?;

        let mut cmd = Command::new(&self.command);
        cmd.kill_on_drop(true);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd.arg("prompt").arg(&manifest_abs);
        cmd.arg(prompt);
        cmd.arg("--format").arg("jsonl");
        // Loom 0.1.4: prepends one `{preamble: {systemPrompt, events,
        // tools}}` JSON line to the jsonl stream, capturing exactly what
        // the harness is about to send to the model. We tee the entire
        // stream into the invocation log; the preamble line gives full
        // audit fidelity (system prompt + history + tool list) for free.
        if log.is_some() {
            cmd.arg("--emit-preamble");
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let work = async {
            let mut child = cmd.spawn().with_context(|| {
                format!(
                    "failed to spawn `{}` — is Loom installed and on PATH? \
                     (try ./scripts/run.sh, which puts node_modules/.bin first)",
                    self.command
                )
            })?;
            let stdout = child.stdout.take().context("loom: missing stdout pipe")?;
            let stderr = child.stderr.take().context("loom: missing stderr pipe")?;

            // Drain stderr concurrently so a chatty Loom (audit findings,
            // permission prompts) doesn't block on a full pipe.
            let stderr_task = tokio::spawn(async move {
                let mut buf = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut buf).await;
                buf
            });

            stream_events(stdout, &tx, log.as_deref_mut()).await?;

            let status = child.wait().await?;
            let stderr_buf = stderr_task.await.unwrap_or_default();

            if !status.success() {
                anyhow::bail!("loom exited {}: {}", status, stderr_buf.trim());
            }
            Ok(())
        };

        match tokio::time::timeout(LOOM_TIMEOUT, work).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!("loom timed out after {:?}", LOOM_TIMEOUT),
        }
    }
}

#[async_trait]
impl LoomRunner for LoomCli {
    async fn run(
        &self,
        manifest: &Path,
        prompt: &str,
        tx: mpsc::Sender<String>,
        log: Option<&mut InvocationLog>,
    ) -> Result<()> {
        self.invoke(manifest, prompt, tx, log).await
    }
}

async fn stream_events(
    stdout: tokio::process::ChildStdout,
    tx: &mpsc::Sender<String>,
    mut log: Option<&mut InvocationLog>,
) -> Result<()> {
    let mut lines = BufReader::new(stdout).lines();
    let mut r = Renderer::new();

    loop {
        // While a tool batch is pending, race the next line against the
        // coalesce window; if the window expires first, flush the batch.
        let line = if r.has_pending_tools() {
            tokio::select! {
                line = lines.next_line() => line?,
                _ = tokio::time::sleep(TOOL_COALESCE) => {
                    r.flush_tools();
                    drain(&mut r, tx).await?;
                    continue;
                }
            }
        } else {
            lines.next_line().await?
        };

        let Some(line) = line else { break };
        // Tee the raw line into the invocation log if one's attached.
        // The renderer (below) only sees parsed text/tool chunks; the
        // log captures the full SessionUpdate + preamble stream so the
        // record is a faithful replay of what Loom emitted.
        if let Some(log) = log.as_deref_mut() {
            if let Err(e) = log.write_line(&line).await {
                tracing::warn!("invocation_log: failed to write line: {e:#}");
            }
        }
        // Trace-level transcript of every raw event Loom emitted. Opt in
        // with `RUST_LOG=glass::loom=trace` for real-time debugging.
        tracing::trace!(line = %line, "loom event");
        observe_stop_event(&line);
        r.handle_line(&line);
        drain(&mut r, tx).await?;
    }

    r.finish();
    drain(&mut r, tx).await?;
    Ok(())
}

/// Watch for `stop` events with non-`end_turn` reasons and surface them via
/// `tracing::warn`. Without this, a cron turn that fails inside Loom
/// (`max_turn_requests`, `error`, `refusal`, etc.) exits non-zero with an
/// empty stderr because the stop reason was emitted to stdout in the jsonl
/// stream — which the orchestrator drops for cron's silent-by-design output.
fn observe_stop_event(line: &str) {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    if v.get("sessionUpdate").and_then(|s| s.as_str()) != Some("stop") {
        return;
    }
    let reason = v
        .get("stopReason")
        .and_then(|s| s.as_str())
        .unwrap_or("(unknown)");
    if reason == "end_turn" {
        tracing::debug!(stop_reason = %reason, "loom turn ended cleanly");
    } else {
        tracing::warn!(stop_reason = %reason, "loom turn stopped without end_turn");
    }
}

async fn drain(r: &mut Renderer, tx: &mpsc::Sender<String>) -> Result<()> {
    for msg in r.take_pending() {
        tx.send(msg)
            .await
            .context("loom: bus dropped the receiver")?;
    }
    Ok(())
}

/// Stateful event renderer. Watches a SessionUpdate stream and produces a
/// sequence of Discord-ready messages preserving interleaving (text before
/// tool calls stays before, text after stays after). The async wrapper
/// (`stream_events`) adds timing-based coalescing on top of this; the core
/// transition logic is sync and unit-testable.
struct Renderer {
    text_buf: String,
    tool_batch: Vec<String>,
    last_was_thinking: bool,
    pending: Vec<String>,
}

impl Renderer {
    fn new() -> Self {
        Self {
            text_buf: String::new(),
            tool_batch: Vec::new(),
            last_was_thinking: false,
            pending: Vec::new(),
        }
    }

    fn has_pending_tools(&self) -> bool {
        !self.tool_batch.is_empty()
    }

    fn take_pending(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }

    fn flush_text(&mut self) {
        let trimmed = self.text_buf.trim().to_string();
        self.text_buf.clear();
        if !trimmed.is_empty() {
            self.pending.push(trimmed);
        }
    }

    fn flush_tools(&mut self) {
        if !self.tool_batch.is_empty() {
            let msg = std::mem::take(&mut self.tool_batch).join("\n");
            self.pending.push(msg);
        }
    }

    fn finish(&mut self) {
        self.flush_text();
        self.flush_tools();
    }

    fn handle_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("skipping malformed loom event: {e}");
                return;
            }
        };
        match v.get("sessionUpdate").and_then(|s| s.as_str()) {
            Some("tool_call") => {
                // Text → tool boundary: flush the text so it shows up
                // *before* the tool subtext, not after.
                self.flush_text();
                let title = v.get("title").and_then(|s| s.as_str()).unwrap_or("tool");
                let raw_input = v.get("rawInput");
                self.tool_batch.push(render_tool_line(title, raw_input));
                self.last_was_thinking = false;
            }
            Some("agent_thought_chunk") if !self.last_was_thinking => {
                self.flush_text();
                self.tool_batch.push("-# Thinking".to_string());
                self.last_was_thinking = true;
            }
            Some("agent_thought_chunk") => {}
            Some("agent_message_chunk") => {
                // Tool → text boundary: flush the tool batch so it shows
                // up *before* this paragraph of the agent's reply.
                self.flush_tools();
                if let Some(t) = v.pointer("/content/text").and_then(|t| t.as_str()) {
                    self.text_buf.push_str(t);
                }
                self.last_was_thinking = false;
            }
            _ => {
                // user_message_chunk, tool_call_update, plan, usage_update,
                // available_commands_update, current_mode_update, stop —
                // not surfaced to the owner.
            }
        }
    }
}

fn render_tool_line(title: &str, raw_input: Option<&serde_json::Value>) -> String {
    let title = sanitize_one_line(title.trim());
    let args = raw_input.map(render_args).unwrap_or_default();
    format!("-# {title}({args})")
}

// Render args as `k=v, k=v` with per-arg and total caps. Single-arg tools
// drop the key entirely (`read_file(a.txt)` not `read_file(path=a.txt)`)
// since the kind is implied by the tool name. Strings render unquoted;
// other JSON values stringify via serde. Control chars become spaces so a
// tool-call line stays on one line.
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
    sanitize_one_line(&raw)
}

fn sanitize_one_line(s: &str) -> String {
    s.chars()
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

// Test helpers; not gated #[cfg(test)] so integration tests can use them.
pub mod testing {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // Per call: drains the next slice from the queue, sends each message
    // over `tx`, and returns `Ok(())`. Empty queue → panic on call.
    pub struct MockLoomRunner {
        turns: Mutex<VecDeque<Vec<String>>>,
        calls: Mutex<Vec<(PathBuf, String)>>,
    }

    impl MockLoomRunner {
        pub fn new(turns: &[&[&str]]) -> Self {
            let q = turns
                .iter()
                .map(|t| t.iter().map(|s| s.to_string()).collect())
                .collect();
            Self {
                turns: Mutex::new(q),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<(PathBuf, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LoomRunner for MockLoomRunner {
        async fn run(
            &self,
            manifest: &Path,
            prompt: &str,
            tx: mpsc::Sender<String>,
            _log: Option<&mut InvocationLog>,
        ) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((manifest.to_path_buf(), prompt.to_string()));
            let msgs = self
                .turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockLoomRunner: out of canned turns");
            for m in msgs {
                tx.send(m).await.ok();
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(stdout: &str) -> Vec<String> {
        let mut r = Renderer::new();
        for line in stdout.lines() {
            r.handle_line(line);
        }
        r.finish();
        r.take_pending()
    }

    #[test]
    fn text_only_one_message() {
        let stdout = concat!(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello "}}"#,
            "\n",
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world."}}"#,
            "\n",
            r#"{"sessionUpdate":"stop","stopReason":"end_turn"}"#,
            "\n",
        );
        assert_eq!(render(stdout), vec!["Hello world."]);
    }

    #[test]
    fn text_then_tools_then_text_interleaves() {
        // Reproduces the v0.3.0 bug: all tool lines were rendered first,
        // then all text. The expected order is text → tools → text.
        let stdout = concat!(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Sure, "}}"#,
            "\n",
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"looking now."}}"#,
            "\n",
            r#"{"sessionUpdate":"tool_call","toolCallId":"a","title":"bash","rawInput":{"command":"ls -la"}}"#,
            "\n",
            r#"{"sessionUpdate":"tool_call","toolCallId":"b","title":"read_file","rawInput":{"path":"_glass/soul.md"}}"#,
            "\n",
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Found it."}}"#,
            "\n",
        );
        assert_eq!(
            render(stdout),
            vec![
                "Sure, looking now.".to_string(),
                "-# bash(ls -la)\n-# read_file(_glass/soul.md)".to_string(),
                "Found it.".to_string(),
            ]
        );
    }

    #[test]
    fn consecutive_tools_with_no_text_coalesce_into_one_message() {
        let stdout = concat!(
            r#"{"sessionUpdate":"tool_call","toolCallId":"a","title":"bash","rawInput":{"command":"ls"}}"#,
            "\n",
            r#"{"sessionUpdate":"tool_call","toolCallId":"b","title":"bash","rawInput":{"command":"pwd"}}"#,
            "\n",
            r#"{"sessionUpdate":"tool_call","toolCallId":"c","title":"write_file","rawInput":{"path":"a.md","content":"hi"}}"#,
            "\n",
        );
        assert_eq!(
            render(stdout),
            vec!["-# bash(ls)\n-# bash(pwd)\n-# write_file(path=a.md, content=hi)"]
        );
    }

    #[test]
    fn tool_call_with_no_raw_input_renders_empty_parens() {
        let stdout = r#"{"sessionUpdate":"tool_call","toolCallId":"a","title":"bash"}"#;
        assert_eq!(render(stdout), vec!["-# bash()"]);
    }

    #[test]
    fn tool_args_truncate_long_values_and_strip_control_chars() {
        let long = "x".repeat(120);
        let stdout = format!(
            r#"{{"sessionUpdate":"tool_call","toolCallId":"a","title":"bash","rawInput":{{"command":"echo {long}\nrun"}}}}"#
        );
        let out = render(&stdout);
        assert_eq!(out.len(), 1);
        let line = &out[0];
        // Control chars become spaces, line stays single-line.
        assert!(
            !line.contains('\n'),
            "line should not contain a newline: {line:?}"
        );
        // Truncation produces a trailing ellipsis and respects the per-arg cap.
        assert!(
            line.ends_with("...)"),
            "expected truncation suffix in {line:?}"
        );
        let inside = line.trim_start_matches("-# bash(").trim_end_matches(')');
        assert_eq!(inside.chars().count(), ARG_MAX_CHARS);
    }

    #[test]
    fn tool_args_non_string_values_stringify() {
        let stdout = r#"{"sessionUpdate":"tool_call","toolCallId":"a","title":"find","rawInput":{"limit":42,"recursive":true}}"#;
        let out = render(stdout);
        assert_eq!(out.len(), 1);
        // Order of object keys is preserved from the JSON source.
        assert!(
            out[0] == "-# find(limit=42, recursive=true)"
                || out[0] == "-# find(recursive=true, limit=42)",
            "unexpected render: {:?}",
            out[0]
        );
    }

    #[test]
    fn thought_chunks_dedupe_to_one_thinking_line() {
        let stdout = concat!(
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"a"}}"#,
            "\n",
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"b"}}"#,
            "\n",
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}"#,
            "\n",
        );
        assert_eq!(
            render(stdout),
            vec!["-# Thinking".to_string(), "ok".to_string()]
        );
    }

    #[test]
    fn malformed_and_empty_lines_are_skipped() {
        let stdout = "\n\nnot json\n{\"sessionUpdate\":\"agent_message_chunk\",\
                      \"content\":{\"type\":\"text\",\"text\":\"hi\"}}\n";
        assert_eq!(render(stdout), vec!["hi"]);
    }

    #[test]
    fn empty_stream_produces_no_messages() {
        assert_eq!(render(""), Vec::<String>::new());
    }
}
