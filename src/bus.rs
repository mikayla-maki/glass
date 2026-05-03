use anyhow::Result;
use async_trait::async_trait;

// Transport-agnostic IDs. Today every transport is Discord-shaped (u64);
// when Signal/Telegram show up these become enums and the conversion happens
// in the platform impls (see SerenityBus).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AuthorId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConversationId(pub u64);

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
    // Show a transient "typing..." indicator. Discord auto-clears after
    // ~10s, so the bot loop ticks this every few seconds while Pi runs.
    async fn typing(&self, channel: ConversationId) -> Result<()>;
}

// Discord's typing indicator lasts ~10s; refresh well within that.
const TYPING_REFRESH: std::time::Duration = std::time::Duration::from_secs(7);

fn actionable(msg: &IncomingDm, owner: AuthorId) -> bool {
    if msg.author != owner {
        tracing::warn!(author = ?msg.author, "ignoring DM from non-owner");
        return false;
    }
    !msg.content.trim().is_empty()
}

// The bot loop. Pulls DMs, gates by owner, hands to engine, posts replies.
// If a new owner DM arrives while we're mid-turn, the in-flight handle is
// cancelled (drops the Pi child via kill_on_drop) and we restart with the
// new message. The cancelled user message stays in the logs so the next
// turn can still see it as context.
pub async fn run(
    bus: &dyn MessageBus,
    engine: &crate::turn::TurnEngine,
    owner: AuthorId,
) -> Result<()> {
    use tracing::{error, info};

    let mut pending: Option<IncomingDm> = None;

    loop {
        let msg = match pending.take() {
            Some(m) => m,
            None => match bus.next().await {
                Some(m) => m,
                None => return Ok(()),
            },
        };

        if !actionable(&msg, owner) {
            continue;
        }

        info!(chars = msg.content.len(), "DM received");
        let started = std::time::Instant::now();
        let channel = msg.channel;

        let handle_fut = engine.handle(&msg.content);
        tokio::pin!(handle_fut);

        let mut typing_tick = tokio::time::interval(TYPING_REFRESH);
        // First tick fires immediately so the indicator shows up before Pi
        // has produced anything.

        loop {
            tokio::select! {
                // Prefer the in-flight handle. Only check for a new message
                // if the handle is actually blocked (e.g. on Pi).
                biased;
                result = &mut handle_fut => {
                    match result {
                        Ok(reply) => {
                            info!(
                                chars = reply.len(),
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "reply sent"
                            );
                            if let Err(e) = bus.reply(channel, &reply).await {
                                error!("failed to send reply: {e}");
                            }
                        }
                        Err(e) => {
                            error!(
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "turn failed: {e:#}"
                            );
                            let _ = bus
                                .reply(channel, &format!("⚠️ turn failed: `{e}`"))
                                .await;
                        }
                    }
                    break;
                }
                _ = typing_tick.tick() => {
                    if let Err(e) = bus.typing(channel).await {
                        // Don't fail the turn over a missing typing indicator.
                        tracing::debug!("typing indicator failed: {e}");
                    }
                }
                next = bus.next() => match next {
                    Some(new_msg) => {
                        if !actionable(&new_msg, owner) { continue; }
                        info!(
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "DM received mid-turn, cancelling current"
                        );
                        pending = Some(new_msg);
                        break;
                    }
                    None => return Ok(()),
                },
            }
        }
    }
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
        typing_calls: std::sync::atomic::AtomicUsize,
        notify: Notify,
        closed: AtomicBool,
    }

    impl StubBus {
        pub fn new() -> Self {
            Self {
                inbox: Mutex::new(VecDeque::new()),
                replies: Mutex::new(Vec::new()),
                typing_calls: std::sync::atomic::AtomicUsize::new(0),
                notify: Notify::new(),
                closed: AtomicBool::new(false),
            }
        }

        pub fn typing_count(&self) -> usize {
            self.typing_calls.load(Ordering::Acquire)
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

        async fn typing(&self, _channel: ConversationId) -> Result<()> {
            self.typing_calls.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::StubBus;
    use super::*;
    use crate::agent::testing::FakeAgent;
    use crate::agent::AgentRuntime;
    use crate::compaction::testing::loose;
    use crate::config::Workspace;
    use crate::events::testing::E;
    use crate::events::Event;
    use std::path::Path;
    use std::sync::Arc;

    fn fresh_engine_with(
        runtime: Arc<dyn AgentRuntime>,
    ) -> (tempfile::TempDir, crate::turn::TurnEngine) {
        let (tmp, ws) = Workspace::tempdir();
        let engine = crate::turn::TurnEngine::new(ws, runtime, loose());
        (tmp, engine)
    }

    #[tokio::test]
    async fn run_loop_replies_to_owner_and_ignores_others() {
        let runtime = Arc::new(FakeAgent::new(&["echo: hi", "echo: still there?"], &[]));
        let (_tmp, engine) = fresh_engine_with(runtime);
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
        let runtime = Arc::new(FakeAgent::new(&["echo: real"], &[]));
        let (_tmp, engine) = fresh_engine_with(runtime);
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

    #[tokio::test]
    async fn new_dm_cancels_in_flight_handle_and_restarts() {
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // First call to run_turn pends forever (will be cancelled). Second
        // call returns a canned reply. Genuinely different from FakeAgent
        // (which always pops a queue), so kept inline.
        struct InterruptableRuntime {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl AgentRuntime for InterruptableRuntime {
            async fn run_turn(&self, _w: &Path, _h: &[Event], msg: &str) -> Result<String> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                Ok(format!("reply to {msg}"))
            }
            async fn summarize(&self, _w: &Path, _p: Option<&str>, _t: &[Event]) -> Result<String> {
                unreachable!("compaction-disabled in this test")
            }
        }

        let (_tmp, ws) = Workspace::tempdir();
        let runtime = Arc::new(InterruptableRuntime {
            calls: AtomicUsize::new(0),
        });
        let engine = Arc::new(crate::turn::TurnEngine::new(
            ws.clone(),
            runtime.clone(),
            loose(),
        ));
        let bus = Arc::new(StubBus::new());
        let owner = AuthorId(42);
        let channel = ConversationId(1);

        let bus_t = bus.clone();
        let engine_t = engine.clone();
        let task = tokio::spawn(async move { run(&*bus_t, &engine_t, owner).await });

        // First message: bot will start handling, hit run_turn, pend forever.
        bus.push(IncomingDm {
            author: owner,
            channel,
            content: "first".into(),
        })
        .await;

        // Wait until run_turn is reached, so we know the bot is mid-handle
        // when we push the second message.
        for _ in 0..1000 {
            if runtime.calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);

        // Second message: should cancel the first handle, restart with this.
        bus.push(IncomingDm {
            author: owner,
            channel,
            content: "second".into(),
        })
        .await;
        bus.close();

        task.await.unwrap().unwrap();

        // Both messages reached run_turn (first cancelled mid-flight, second completed).
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 2);

        // Audit log: both UserMessages, exactly one AgentMessage (for "second").
        let log = crate::events::testing::read_log(&ws.events_log());
        let users = log.iter().filter(|e| matches!(e, E::User(_))).count();
        let agents: Vec<_> = log.iter().filter(|e| matches!(e, E::Agent(_))).collect();
        assert_eq!(users, 2);
        assert_eq!(agents.len(), 1);
        assert!(matches!(agents[0], E::Agent(s) if s == "reply to second"));

        // One reply was sent over the bus.
        let replies = bus.replies().await;
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].1, "reply to second");
    }

    #[tokio::test(start_paused = true)]
    async fn typing_indicator_refreshes_during_long_run() {
        // Paused time lets us drive the typing-tick interval deterministically.
        // SlowRuntime parks run_turn on a sleep we control via tokio::time::advance.
        use async_trait::async_trait;
        use std::time::Duration;

        struct SlowRuntime;
        #[async_trait]
        impl AgentRuntime for SlowRuntime {
            async fn run_turn(&self, _w: &Path, _h: &[Event], _m: &str) -> Result<String> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok("done".into())
            }
            async fn summarize(&self, _w: &Path, _p: Option<&str>, _t: &[Event]) -> Result<String> {
                unreachable!()
            }
        }

        let (_tmp, ws) = Workspace::tempdir();
        let engine = Arc::new(crate::turn::TurnEngine::new(
            ws,
            Arc::new(SlowRuntime),
            loose(),
        ));
        let bus = Arc::new(StubBus::new());
        let owner = AuthorId(42);

        bus.push(IncomingDm {
            author: owner,
            channel: ConversationId(1),
            content: "hi".into(),
        })
        .await;

        let bus_t = bus.clone();
        let engine_t = engine.clone();
        let task = tokio::spawn(async move { run(&*bus_t, &engine_t, owner).await });

        // Advance past the 60s run_turn sleep in 7s steps. Yield between
        // advances so the bot loop polls the typing interval.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(7)).await;
        }
        tokio::task::yield_now().await;

        bus.close();
        task.await.unwrap().unwrap();

        // 60s window / TYPING_REFRESH (7s) = 8 refreshes, plus the immediate
        // first tick = 9 total.
        assert_eq!(bus.typing_count(), 9);
        assert_eq!(bus.replies().await.len(), 1);
    }

    #[test]
    fn chunk_message_preserves_per_line_prefixes_across_splits() {
        // Each tool line carries its own `-# ` Discord-subtext prefix. Forcing
        // a chunk boundary between lines must not strip prefixes from any
        // line in any chunk — same property would apply to `> ` quote prefixes.
        use std::fmt::Write as _;
        let mut s = String::new();
        for i in 0..25 {
            let _ = writeln!(s, "-# `Tool{i}(arg)`");
        }
        s.push_str("\nanswer body text");

        let chunks = chunk_message(&s, 200);
        assert!(chunks.len() > 1, "expected multiple chunks");
        for chunk in &chunks {
            for line in chunk.lines() {
                if line.contains("Tool") {
                    assert!(
                        line.starts_with("-# "),
                        "tool line lost its prefix in chunk: {line:?}"
                    );
                }
            }
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
