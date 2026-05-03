// Two JSONL log files share this format:
// - history/events.jsonl  (audit, append-only forever)
// - history/current.jsonl (live conversation, rewritten on compaction)

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use tracing::warn;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    UserMessage {
        timestamp: DateTime<Utc>,
        content: String,
    },
    AgentMessage {
        timestamp: DateTime<Utc>,
        content: String,
    },
    Summary {
        timestamp: DateTime<Utc>,
        body: String,
    },
}

impl Event {
    pub fn user(content: impl Into<String>) -> Self {
        Self::UserMessage {
            timestamp: Utc::now(),
            content: content.into(),
        }
    }

    pub fn agent(content: impl Into<String>) -> Self {
        Self::AgentMessage {
            timestamp: Utc::now(),
            content: content.into(),
        }
    }

    pub fn summary(body: impl Into<String>) -> Self {
        Self::Summary {
            timestamp: Utc::now(),
            body: body.into(),
        }
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Event::UserMessage { timestamp, .. }
            | Event::AgentMessage { timestamp, .. }
            | Event::Summary { timestamp, .. } => *timestamp,
        }
    }

    // Rough chars/4 heuristic. Uses bytes (slightly conservative on non-ASCII,
    // which makes compaction trigger slightly earlier; safe).
    pub fn estimated_tokens(&self) -> usize {
        let chars = match self {
            Event::UserMessage { content, .. } | Event::AgentMessage { content, .. } => {
                content.len()
            }
            Event::Summary { body, .. } => body.len(),
        };
        chars / 4 + 8
    }
}

pub fn append(log: &Path, event: &Event) -> Result<()> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(log)?;
    let line = serde_json::to_string(event)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

pub fn load_log(log: &Path) -> Result<Vec<Event>> {
    if !log.exists() {
        return Ok(vec![]);
    }
    let f = std::fs::File::open(log)?;
    let reader = BufReader::new(f);
    let mut events = Vec::new();
    for (n, line) in reader.lines().enumerate() {
        let line = line?;
        match serde_json::from_str(&line) {
            Ok(e) => events.push(e),
            Err(e) => warn!(line_no = n + 1, "skipping malformed log line: {e}"),
        }
    }
    Ok(events)
}

// Tempfile + rename. Crash-atomic w.r.t. the process; doesn't fsync the
// parent dir, so machine power-loss could undo the rename on some FSes.
pub fn write_atomic(log: &Path, events: &[Event]) -> Result<()> {
    let parent = log.parent().context("log path has no parent")?;
    std::fs::create_dir_all(parent)?;

    let file_name = log
        .file_name()
        .context("log path has no file name")?
        .to_string_lossy();
    let tmp_path = parent.join(format!(".{file_name}.tmp"));

    {
        let mut f = std::fs::File::create(&tmp_path)?;
        for event in events {
            let line = serde_json::to_string(event)?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, log)?;
    Ok(())
}

// Test helpers; not gated #[cfg(test)] so integration tests can use them.
pub mod testing {
    use super::*;

    // Timestamps stripped for deterministic equality.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum E {
        User(String),
        Agent(String),
        Summary(String),
    }

    impl E {
        pub fn user(s: &str) -> Self {
            E::User(s.into())
        }
        pub fn agent(s: &str) -> Self {
            E::Agent(s.into())
        }
        pub fn summary(s: &str) -> Self {
            E::Summary(s.into())
        }
    }

    pub fn simplify(events: &[Event]) -> Vec<E> {
        events
            .iter()
            .map(|e| match e {
                Event::UserMessage { content, .. } => E::User(content.clone()),
                Event::AgentMessage { content, .. } => E::Agent(content.clone()),
                Event::Summary { body, .. } => E::Summary(body.clone()),
            })
            .collect()
    }

    pub fn read_log(path: &Path) -> Vec<E> {
        simplify(&load_log(path).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn append_then_load_preserves_event_order_and_content() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        append(tmp.path(), &Event::user("hi")).unwrap();
        append(tmp.path(), &Event::agent("hello")).unwrap();
        append(tmp.path(), &Event::summary("a summary")).unwrap();

        assert_eq!(
            read_log(tmp.path()),
            vec![E::user("hi"), E::agent("hello"), E::summary("a summary")]
        );
    }

    #[test]
    fn write_atomic_replaces_contents() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        append(tmp.path(), &Event::user("old 1")).unwrap();
        append(tmp.path(), &Event::user("old 2")).unwrap();

        let new_contents = vec![Event::summary("rolled up"), Event::user("kept")];
        write_atomic(tmp.path(), &new_contents).unwrap();

        assert_eq!(
            read_log(tmp.path()),
            vec![E::summary("rolled up"), E::user("kept")]
        );
    }

    #[test]
    fn load_log_on_missing_file_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("does_not_exist.jsonl");
        assert!(load_log(&path).unwrap().is_empty());
    }
}
