use anyhow::Result;
use async_trait::async_trait;
use std::fmt;

// Transport-agnostic IDs. Today every transport is Discord-shaped (u64);
// when Signal/Telegram show up these become enums and the conversion happens
// in the platform impls (see SerenityBus).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AuthorId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConversationId(pub u64);

impl fmt::Display for AuthorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug)]
pub struct IncomingDm {
    pub author: AuthorId,
    pub channel: ConversationId,
    pub content: String,
}

#[async_trait]
pub trait MessageBus: Send + Sync {
    async fn next(&self) -> Option<IncomingDm>;
    async fn reply(&self, channel: ConversationId, content: &str) -> Result<()>;
}

// The bot loop: pull DMs, gate by owner, hand to engine, post replies.
// Returns when bus.next() returns None.
pub async fn run(
    bus: &dyn MessageBus,
    engine: &crate::turn::TurnEngine,
    owner: AuthorId,
) -> Result<()> {
    use tracing::{error, info, warn};

    while let Some(msg) = bus.next().await {
        if msg.author != owner {
            warn!(author = %msg.author, "ignoring DM from non-owner");
            continue;
        }
        if msg.content.trim().is_empty() {
            continue;
        }
        info!(chars = msg.content.len(), "DM received");
        let started = std::time::Instant::now();

        match engine.handle(&msg.content).await {
            Ok(reply) => {
                info!(
                    chars = reply.len(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "reply sent"
                );
                if let Err(e) = bus.reply(msg.channel, &reply).await {
                    error!("failed to send reply: {e}");
                }
            }
            Err(e) => {
                error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "turn failed: {e:#}"
                );
                let _ = bus
                    .reply(msg.channel, &format!("⚠️ turn failed: `{e}`"))
                    .await;
            }
        }
    }
    Ok(())
}

// Split on line boundaries when possible, on UTF-8 char boundaries when
// forced. Used by SerenityBus to respect Discord's 2000-byte message cap
// without producing replacement chars on multi-byte content.
pub fn chunk_message(s: &str, max: usize) -> Vec<String> {
    if s.len() <= max {
        return vec![s.to_string()];
    }
    let mut out = vec![];
    let mut cur = String::new();
    for line in s.lines() {
        if cur.len() + line.len() + 1 > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if line.len() > max {
            let mut rest = line;
            while !rest.is_empty() {
                let cut = if rest.len() <= max {
                    rest.len()
                } else {
                    rest.floor_char_boundary(max)
                };
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// Test helpers; not gated #[cfg(test)] so integration tests can use them.
pub mod testing {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{Mutex, Notify};

    pub struct StubBus {
        inbox: Mutex<VecDeque<IncomingDm>>,
        replies: Mutex<Vec<(ConversationId, String)>>,
        notify: Notify,
        closed: AtomicBool,
    }

    impl StubBus {
        pub fn new() -> Self {
            Self {
                inbox: Mutex::new(VecDeque::new()),
                replies: Mutex::new(Vec::new()),
                notify: Notify::new(),
                closed: AtomicBool::new(false),
            }
        }

        pub async fn push(&self, msg: IncomingDm) {
            self.inbox.lock().await.push_back(msg);
            self.notify.notify_one();
        }

        pub fn close(&self) {
            self.closed.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }

        pub async fn replies(&self) -> Vec<(ConversationId, String)> {
            self.replies.lock().await.clone()
        }
    }

    impl Default for StubBus {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl MessageBus for StubBus {
        async fn next(&self) -> Option<IncomingDm> {
            loop {
                if let Some(msg) = self.inbox.lock().await.pop_front() {
                    return Some(msg);
                }
                if self.closed.load(Ordering::Acquire) {
                    return None;
                }
                self.notify.notified().await;
            }
        }

        async fn reply(&self, channel: ConversationId, content: &str) -> Result<()> {
            self.replies
                .lock()
                .await
                .push((channel, content.to_string()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::StubBus;
    use super::*;
    use crate::agent::AgentRuntime;
    use crate::compaction::CompactionConfig;
    use crate::config::Workspace;
    use crate::events::Event;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    // Echoes the user message. Compaction is disabled via a huge context
    // window, so summarize() panics if reached.
    struct EchoRuntime;

    #[async_trait]
    impl AgentRuntime for EchoRuntime {
        async fn run_turn(&self, _ws: &Path, _h: &[Event], msg: &str) -> Result<String> {
            Ok(format!("echo: {msg}"))
        }
        async fn summarize(
            &self,
            _ws: &Path,
            _prior: Option<&str>,
            _transcript: &[Event],
        ) -> Result<String> {
            unreachable!("bus tests use a huge context window; summarize shouldn't fire")
        }
    }

    fn fresh_engine() -> (TempDir, crate::turn::TurnEngine) {
        let tmp = TempDir::new().unwrap();
        let ws = Workspace {
            root: tmp.path().to_path_buf(),
        };
        ws.ensure_layout().unwrap();
        let cfg = CompactionConfig {
            context_window_tokens: 10_000_000,
            threshold_pct: 0.7,
            keep_recent_tokens: 1000,
        };
        let engine = crate::turn::TurnEngine::new(ws, Arc::new(EchoRuntime), cfg);
        (tmp, engine)
    }

    #[tokio::test]
    async fn run_loop_replies_to_owner_and_ignores_others() {
        let (_tmp, engine) = fresh_engine();
        let bus = StubBus::new();
        let owner = AuthorId(42);
        let intruder = AuthorId(99);
        let owner_channel = ConversationId(200);

        bus.push(IncomingDm {
            author: intruder,
            channel: ConversationId(100),
            content: "intruder".into(),
        })
        .await;
        bus.push(IncomingDm {
            author: owner,
            channel: owner_channel,
            content: "hi".into(),
        })
        .await;
        bus.push(IncomingDm {
            author: owner,
            channel: owner_channel,
            content: "still there?".into(),
        })
        .await;
        bus.close();

        run(&bus, &engine, owner).await.unwrap();

        let replies = bus.replies().await;
        assert_eq!(replies.len(), 2, "intruder message should be ignored");
        assert_eq!(replies[0].0, owner_channel);
        assert_eq!(replies[0].1, "echo: hi");
        assert_eq!(replies[1].1, "echo: still there?");
    }

    #[tokio::test]
    async fn run_loop_skips_empty_messages() {
        let (_tmp, engine) = fresh_engine();
        let bus = StubBus::new();
        let owner = AuthorId(42);

        bus.push(IncomingDm {
            author: owner,
            channel: ConversationId(1),
            content: "   ".into(),
        })
        .await;
        bus.push(IncomingDm {
            author: owner,
            channel: ConversationId(1),
            content: "real".into(),
        })
        .await;
        bus.close();

        run(&bus, &engine, owner).await.unwrap();
        let replies = bus.replies().await;
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].1, "echo: real");
    }

    #[test]
    fn chunk_message_within_limit() {
        assert_eq!(chunk_message("hello", 100), vec!["hello".to_string()]);
    }

    #[test]
    fn chunk_message_splits_on_line_boundary() {
        let v = chunk_message("aaa\nbbb\nccc", 5);
        assert!(v.len() > 1);
        for chunk in &v {
            assert!(chunk.len() <= 5, "chunk {chunk:?} > 5");
        }
    }

    #[test]
    fn chunk_message_hard_splits_long_lines() {
        let s = "x".repeat(300);
        let v = chunk_message(&s, 100);
        assert_eq!(v.len(), 3);
        for chunk in &v {
            assert!(chunk.len() <= 100);
        }
    }

    #[test]
    fn chunk_message_preserves_multibyte_utf8() {
        // 4-byte chars (😀) repeated. Naive byte-split would land mid-codepoint
        // and `from_utf8_lossy` would inject U+FFFD; check we don't.
        let s = "😀".repeat(100); // 400 bytes
        let v = chunk_message(&s, 50);
        assert!(v.len() > 1);
        for chunk in &v {
            assert!(chunk.len() <= 50, "chunk len {} > 50", chunk.len());
            assert!(
                !chunk.contains('\u{FFFD}'),
                "chunk contains replacement char: {chunk:?}"
            );
        }
        // Round-trip: concatenated chunks equal original.
        assert_eq!(v.concat(), s);
    }
}
