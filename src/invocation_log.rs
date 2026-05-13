//! Per-invocation audit log.
//!
//! Every `loom prompt` invocation Glass dispatches \u2014 whether a DM turn or
//! a scheduled cron fire \u2014 gets its own JSONL file at
//! `$GLASS_SYSTEM_DATA/invocations/<timestamp>-<id>.jsonl`. The file
//! captures four kinds of lines:
//!
//!   1. Start header: who fired this and why.
//!   2. `preamble`: Loom 0.1.4's `--emit-preamble` line, with the assembled
//!      system prompt + history events + tool list the model is about to see.
//!   3. SessionUpdate events: every line Loom emits over the turn, passed
//!      through verbatim.
//!   4. End footer: outcome (ok | error | cancelled), exit status, duration.
//!
//! Together they're enough to fully reconstruct a turn after the fact:
//! what the model saw, what it did, what tools it called, and how it
//! ended. The orchestrator's `tracing` logs are still useful for
//! real-time operator visibility; invocation logs are for post-hoc
//! forensics ("what happened in the cron fire at 9am yesterday?").

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Metadata captured at the start of an invocation: the contextual
/// "why was this loom run kicked off." Different triggers populate
/// different optional fields.
#[derive(Debug, Clone)]
pub struct InvocationContext {
    pub trigger: Trigger,
    pub manifest: PathBuf,
    pub prompt: String,
    /// Cron entry id, only present when `trigger = Cron`.
    pub cron_id: Option<String>,
    /// Discord channel id, only present when `trigger = Dm`.
    pub channel: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Dm,
    Cron,
}

/// Final outcome of an invocation, recorded in the end footer.
#[derive(Debug, Clone)]
pub enum InvocationStatus {
    /// Loom exited cleanly.
    Ok,
    /// Loom exited non-zero, or our spawn / stream code errored.
    Err(String),
    /// The orchestrator dropped the runner future before completion
    /// (typically because a new owner DM arrived mid-turn and the bus
    /// cancelled the in-flight handle).
    Cancelled,
}

impl InvocationStatus {
    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Err(_) => "error",
            Self::Cancelled => "cancelled",
        }
    }
    fn error_message(&self) -> Option<&str> {
        match self {
            Self::Err(m) => Some(m.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize)]
struct StartLine<'a> {
    kind: &'static str,
    started_at: String,
    trigger: Trigger,
    manifest: String,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cron_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<u64>,
}

#[derive(Debug, Serialize)]
struct EndLine<'a> {
    kind: &'static str,
    ended_at: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    duration_ms: i64,
}

pub struct InvocationLog {
    file: fs::File,
    path: PathBuf,
    started_at: DateTime<Local>,
}

impl InvocationLog {
    /// Create the invocation log file under `dir`, write the start header,
    /// and return the open handle ready for streamed lines.
    ///
    /// File name layout: `YYYY-MM-DD_HHMMSS-<id>.jsonl`. Sortable in `ls`
    /// output, and the id disambiguates concurrent invocations (the
    /// dispatcher serializes them so collisions are theoretical, but the
    /// id is cheap and makes the path unique without relying on lock
    /// state).
    pub async fn create(dir: &Path, context: InvocationContext) -> Result<Self> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("invocation_log: creating dir {}", dir.display()))?;
        let started_at = Local::now();
        let id = short_id();
        let filename = format!("{}-{id}.jsonl", started_at.format("%Y-%m-%d_%H%M%S"));
        let path = dir.join(filename);
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
            .with_context(|| format!("invocation_log: creating {}", path.display()))?;

        let header = StartLine {
            kind: "start",
            started_at: started_at.to_rfc3339(),
            trigger: context.trigger,
            manifest: context.manifest.display().to_string(),
            prompt: &context.prompt,
            cron_id: context.cron_id.as_deref(),
            channel: context.channel,
        };
        let mut line = serde_json::to_string(&header)?;
        line.push('\n');
        file.write_all(line.as_bytes())
            .await
            .with_context(|| format!("invocation_log: writing header to {}", path.display()))?;

        Ok(Self {
            file,
            path,
            started_at,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one raw line (preamble or SessionUpdate) verbatim. The
    /// caller passes lines as-emitted by Loom's `--format jsonl
    /// --emit-preamble` output; we don't re-parse or re-serialize. Empty
    /// and whitespace-only lines are silently dropped — they'd never
    /// parse as JSON downstream and would just dilute the log.
    pub async fn write_line(&mut self, raw: &str) -> Result<()> {
        if raw.trim().is_empty() {
            return Ok(());
        }
        let line = raw.trim_end_matches('\n');
        self.file
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("invocation_log: writing line to {}", self.path.display()))?;
        self.file
            .write_all(b"\n")
            .await
            .with_context(|| format!("invocation_log: writing line to {}", self.path.display()))?;
        Ok(())
    }

    /// Write the end footer with the final outcome and close.
    pub async fn complete(mut self, status: InvocationStatus) -> Result<()> {
        let ended_at = Local::now();
        let duration_ms = (ended_at - self.started_at).num_milliseconds().max(0);
        let footer = EndLine {
            kind: "end",
            ended_at: ended_at.to_rfc3339(),
            status: status.label(),
            error: status.error_message(),
            duration_ms,
        };
        let mut line = serde_json::to_string(&footer)?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .await
            .with_context(|| {
                format!("invocation_log: writing footer to {}", self.path.display())
            })?;
        self.file
            .flush()
            .await
            .with_context(|| format!("invocation_log: flushing {}", self.path.display()))?;
        Ok(())
    }
}

/// 8-char base32-ish id. Not cryptographic \u2014 just disambiguates concurrent
/// invocations and makes paths grep-friendly.
fn short_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut n = nanos;
    let mut out = String::with_capacity(8);
    for _ in 0..8 {
        out.push(ALPHABET[(n & 0x1f) as usize] as char);
        n >>= 5;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_dm(prompt: &str) -> InvocationContext {
        InvocationContext {
            trigger: Trigger::Dm,
            manifest: PathBuf::from("./manifests/glass.toml"),
            prompt: prompt.to_string(),
            cron_id: None,
            channel: Some(123),
        }
    }

    fn ctx_cron(id: &str, prompt: &str) -> InvocationContext {
        InvocationContext {
            trigger: Trigger::Cron,
            manifest: PathBuf::from("./manifests/cron.toml"),
            prompt: prompt.to_string(),
            cron_id: Some(id.to_string()),
            channel: None,
        }
    }

    #[tokio::test]
    async fn round_trip_ok_turn_captures_start_lines_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut log = InvocationLog::create(dir.path(), ctx_dm("hi"))
            .await
            .unwrap();

        log.write_line(r#"{"preamble":{"systemPrompt":"…","events":[],"tools":[]}}"#)
            .await
            .unwrap();
        log.write_line(
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}"#,
        )
        .await
        .unwrap();
        log.write_line(r#"{"sessionUpdate":"stop","stopReason":"end_turn"}"#)
            .await
            .unwrap();
        log.complete(InvocationStatus::Ok).await.unwrap();

        // Walk the file and assert structure.
        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        let raw = std::fs::read_to_string(&entries[0]).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 5);

        let start: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(start["kind"], "start");
        assert_eq!(start["trigger"], "dm");
        assert_eq!(start["prompt"], "hi");
        assert_eq!(start["channel"], 123);
        assert!(start.get("cron_id").is_none() || start["cron_id"].is_null());

        let preamble: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(preamble.get("preamble").is_some());

        let chunk: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(chunk["sessionUpdate"], "agent_message_chunk");

        let stop: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(stop["sessionUpdate"], "stop");
        assert_eq!(stop["stopReason"], "end_turn");

        let end: serde_json::Value = serde_json::from_str(lines[4]).unwrap();
        assert_eq!(end["kind"], "end");
        assert_eq!(end["status"], "ok");
        assert!(end.get("error").is_none() || end["error"].is_null());
        assert!(end["duration_ms"].is_i64());
    }

    #[tokio::test]
    async fn error_status_includes_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = InvocationLog::create(dir.path(), ctx_cron("abc", "wake up"))
            .await
            .unwrap();
        log.complete(InvocationStatus::Err("model API blew up".into()))
            .await
            .unwrap();

        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let raw = std::fs::read_to_string(&entries[0]).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        // Just start + end since we wrote no events.
        assert_eq!(lines.len(), 2);
        let start: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(start["trigger"], "cron");
        assert_eq!(start["cron_id"], "abc");
        assert_eq!(start["prompt"], "wake up");

        let end: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(end["status"], "error");
        assert_eq!(end["error"], "model API blew up");
    }

    #[tokio::test]
    async fn cancelled_status_writes_clean_footer() {
        let dir = tempfile::TempDir::new().unwrap();
        let log = InvocationLog::create(dir.path(), ctx_dm("hi"))
            .await
            .unwrap();
        log.complete(InvocationStatus::Cancelled).await.unwrap();

        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let raw = std::fs::read_to_string(&entries[0]).unwrap();
        let end_line = raw.lines().last().unwrap();
        let end: serde_json::Value = serde_json::from_str(end_line).unwrap();
        assert_eq!(end["status"], "cancelled");
        assert!(end.get("error").is_none() || end["error"].is_null());
    }

    #[tokio::test]
    async fn write_line_strips_trailing_newline() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut log = InvocationLog::create(dir.path(), ctx_dm("hi"))
            .await
            .unwrap();
        log.write_line("{\"a\":1}\n").await.unwrap();
        log.write_line("{\"b\":2}").await.unwrap();
        log.complete(InvocationStatus::Ok).await.unwrap();

        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let raw = std::fs::read_to_string(&entries[0]).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        // start, two events, end
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1], "{\"a\":1}");
        assert_eq!(lines[2], "{\"b\":2}");
    }

    #[tokio::test]
    async fn empty_lines_are_silently_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut log = InvocationLog::create(dir.path(), ctx_dm("hi"))
            .await
            .unwrap();
        log.write_line("").await.unwrap();
        log.write_line("   \n").await.unwrap();
        log.write_line("{\"a\":1}").await.unwrap();
        log.complete(InvocationStatus::Ok).await.unwrap();

        let entries: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let raw = std::fs::read_to_string(&entries[0]).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        // start, one real event, end (the two empties dropped, the
        // whitespace-only line dropped after trim_end of \n).
        // Actually current impl only strips trailing \n; "   " survives.
        // Verify behavior either way against the expectation.
        let body: Vec<&str> = lines
            .iter()
            .filter(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v.get("kind").is_none()
            })
            .copied()
            .collect();
        assert!(body.contains(&"{\"a\":1}"));
        assert!(!body.iter().any(|l| l.trim().is_empty()));
    }
}
