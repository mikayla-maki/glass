//! Append-only log of every DM Glass exchanges with the operator — both
//! inbound (OPERATOR-to-Glass) and outbound (Glass-to-OPERATOR, whether via the
//! streaming DM agent or a `send_dm` tool call from any agent).
//!
//! Lives in `$GLASS_SYSTEM_DATA/dm-log.jsonl`, not the vault. This is a
//! system audit artifact for the operator, and the source the cron agent's
//! `dm-history` session layer (next PR) reads to surface recent context
//! into its system prompt.
//!
//! Format: one JSON object per line.
//!
//!   `{"at": "2026-05-13T16:00:00-08:00", "direction": "in" | "out",
//!     "content": "..."}`
//!
//! `direction` is the only orientation we record: `in` is from the operator,
//! `out` is from Glass (any agent context). The "which agent produced this"
//! distinction can be reconstructed from invocation logs (next PR) if
//! audit ever needs it; tools should not need agent-identity awareness to
//! write to this log.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    In,
    Out,
}

#[derive(Debug, Serialize)]
struct Entry<'a> {
    at: String,
    direction: Direction,
    content: &'a str,
}

#[derive(Clone)]
pub struct DmLog {
    path: PathBuf,
}

impl DmLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn append(&self, direction: Direction, content: &str) -> Result<()> {
        let entry = Entry {
            at: chrono::Local::now().to_rfc3339(),
            direction,
            content,
        };
        let mut line =
            serde_json::to_string(&entry).context("dm_log: failed to serialize entry")?;
        line.push('\n');

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("dm_log: failed to open {}", self.path.display()))?;
        f.write_all(line.as_bytes())
            .await
            .context("dm_log: failed to write entry")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn append_writes_one_jsonl_line_per_entry() {
        let dir = tempdir().unwrap();
        let log = DmLog::new(dir.path().join("dm-log.jsonl"));

        log.append(Direction::In, "hello").await.unwrap();
        log.append(Direction::Out, "hi back").await.unwrap();

        let raw = tokio::fs::read_to_string(log.path()).await.unwrap();
        let lines: Vec<_> = raw.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed[0]["direction"], "in");
        assert_eq!(parsed[0]["content"], "hello");
        assert_eq!(parsed[1]["direction"], "out");
        assert_eq!(parsed[1]["content"], "hi back");
        // `at` field is present and string-shaped.
        assert!(parsed[0]["at"].is_string());
    }
}
