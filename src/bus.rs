use crate::dispatcher::Dispatcher;
use crate::dm_log::{self, DmLog};
use crate::invocation_log::{InvocationContext, InvocationLog, InvocationStatus, Trigger};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use std::path::Path;
use tokio::sync::mpsc;

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
    /// When Discord recorded the message as sent. The bus formats this into
    /// the prompt so the agent can see jumps in time (e.g. a queued message
    /// from before an orchestrator restart) rather than always reading
    /// inbound DMs as "just arrived."
    pub timestamp: DateTime<Local>,
}

/// Format an incoming DM as the prompt Loom sees: a timestamp prefix in
/// local time, then the content. Compact and easy for the model to parse;
/// pairs with the system-prompt `current time is` line so the agent always
/// has both "when did this arrive" and "when is now."
pub fn format_prompt(msg: &IncomingDm) -> String {
    format!(
        "[{}] {}",
        msg.timestamp.format("%Y-%m-%d %H:%M"),
        msg.content
    )
}

#[async_trait]
pub trait MessageBus: Send + Sync {
    async fn next(&self) -> Option<IncomingDm>;
    async fn reply(&self, channel: ConversationId, content: &str) -> Result<()>;
    // Show a transient "typing..." indicator. Discord auto-clears after
    // ~10s, so the bot loop ticks this every few seconds while Loom runs.
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

// The bot loop. Pulls DMs, gates by owner, hands to the loom runner, posts
// every reply the runner streams. If a new owner DM arrives while we're
// mid-turn, the in-flight handle is cancelled (drops the loom child via
// `kill_on_drop`) and we restart with the new message. Any messages already
// posted to Discord stay there — partial output on cancellation is an honest
// record of what happened. Loom owns session storage, so the cancelled
// user message is dropped at the orchestrator level.
pub async fn run(
    bus: &dyn MessageBus,
    dispatcher: &Dispatcher,
    dm_log: &DmLog,
    invocations_dir: &Path,
    manifest: &Path,
    owner: AuthorId,
) -> Result<()> {
    use tracing::{error, info, warn};

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
        if let Err(e) = dm_log.append(dm_log::Direction::In, &msg.content).await {
            warn!("dm_log: failed to log inbound: {e:#}");
        }
        let started = std::time::Instant::now();
        let channel = msg.channel;
        let prompt = format_prompt(&msg);

        // Open the invocation log before dispatch so the start header is
        // written even if dispatch errors out fast. Caller-owned so
        // cancellation can still write a clean `cancelled` footer below.
        let mut invocation_log = open_invocation_log(
            invocations_dir,
            InvocationContext {
                trigger: Trigger::Dm,
                manifest: manifest.to_path_buf(),
                prompt: prompt.clone(),
                cron_id: None,
                channel: Some(channel.0),
            },
        )
        .await;

        let mut run_result: Option<Result<()>> = None;
        let mut reply_count = 0usize;
        let mut cancelled = false;
        // Tracks bus.next() returning None mid-turn. We stop polling for new
        // inbound DMs (this arm is disabled below) but keep draining the
        // reply channel so partial output reaches Discord; the outer loop
        // discovers the close on its next iteration and returns Ok cleanly.
        let mut bus_closed = false;

        // Scope the runner future so its borrow of `invocation_log` drops
        // before we move the log into `finalize_invocation_log`. The state
        // variables above are declared outside this block so the post-loop
        // status handling can read them.
        {
            let (tx, mut rx) = mpsc::channel::<String>(16);
            let run_fut = dispatcher.dispatch(manifest, &prompt, tx, invocation_log.as_mut());
            tokio::pin!(run_fut);

            let mut typing_tick = tokio::time::interval(TYPING_REFRESH);

            loop {
                tokio::select! {
                    // Once the runner has completed, the sender is dropped and
                    // `rx.recv()` will return None after the buffered messages
                    // have drained. The `if` guards on the other arms make sure
                    // we don't keep ticking typing or polling for new DMs once
                    // the turn is finished — we just drain the receiver and
                    // exit.
                    biased;
                    result = &mut run_fut, if run_result.is_none() => {
                        run_result = Some(result);
                    }
                    msg = rx.recv() => match msg {
                        Some(reply) => {
                            reply_count += 1;
                            if let Err(e) = dm_log
                                .append(dm_log::Direction::Out, &reply)
                                .await
                            {
                                warn!("dm_log: failed to log outbound: {e:#}");
                            }
                            if let Err(e) = bus.reply(channel, &reply).await {
                                error!("failed to send reply: {e}");
                            }
                        }
                        None => break,
                    },
                    _ = typing_tick.tick(), if run_result.is_none() => {
                        if let Err(e) = bus.typing(channel).await {
                            // Don't fail the turn over a missing typing indicator.
                            tracing::debug!("typing indicator failed: {e}");
                        }
                    }
                    next = bus.next(), if run_result.is_none() && !bus_closed => match next {
                        Some(new_msg) => {
                            if !actionable(&new_msg, owner) { continue; }
                            info!(
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "DM received mid-turn, cancelling current"
                            );
                            pending = Some(new_msg);
                            cancelled = true;
                            break;
                        }
                        None => {
                            bus_closed = true;
                        }
                    },
                }
            }
            // End of `run_fut`/`rx`/`typing_tick` scope; the runner future
            // and its borrow of `invocation_log` are dropped here.
        }

        let status = if cancelled {
            InvocationStatus::Cancelled
        } else {
            match &run_result {
                Some(Ok(())) => {
                    info!(
                        replies = reply_count,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "turn complete"
                    );
                    InvocationStatus::Ok
                }
                Some(Err(e)) => {
                    error!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "turn failed: {e:#}"
                    );
                    let err_msg = format!("⚠️ turn failed: `{e}`");
                    if let Err(e) = dm_log.append(dm_log::Direction::Out, &err_msg).await {
                        warn!("dm_log: failed to log error reply: {e:#}");
                    }
                    let _ = bus.reply(channel, &err_msg).await;
                    InvocationStatus::Err(format!("{e:#}"))
                }
                None => {
                    // Should be unreachable: the only way out of the inner
                    // loop without cancellation is via the `None` branch on
                    // `rx.recv()`, which only fires after `run_result` is
                    // set. Log and move on if it ever happens.
                    error!("turn ended with no recorded result");
                    InvocationStatus::Err("turn ended with no recorded result".into())
                }
            }
        };
        finalize_invocation_log(invocation_log, status).await;
    }
}

/// Open an invocation log for a turn. Failures are non-fatal: we warn and
/// continue without a log so an unwritable directory doesn't take Glass
/// down. The cost of a missed audit record is preferable to crashing.
async fn open_invocation_log(dir: &Path, ctx: InvocationContext) -> Option<InvocationLog> {
    match InvocationLog::create(dir, ctx).await {
        Ok(log) => Some(log),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), "invocation_log: failed to open: {e:#}");
            None
        }
    }
}

/// Complete an invocation log with a final status. Failures are warn-only
/// so a log-write hiccup never overwrites the turn's actual outcome.
async fn finalize_invocation_log(log: Option<InvocationLog>, status: InvocationStatus) {
    if let Some(log) = log {
        let path = log.path().to_path_buf();
        if let Err(e) = log.complete(status).await {
            tracing::warn!(path = %path.display(), "invocation_log: failed to complete: {e:#}");
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
    use crate::loom::testing::MockLoomRunner;
    use crate::loom::LoomRunner;
    use chrono::TimeZone;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn manifest() -> PathBuf {
        PathBuf::from("./manifests/glass.toml")
    }

    /// Tests don't care where the dm-log lands; just give each test a fresh
    /// tempdir so they can run in parallel without stomping each other.
    fn fresh_dm_log() -> (tempfile::TempDir, DmLog) {
        let dir = tempfile::TempDir::new().unwrap();
        let log = DmLog::new(dir.path().join("dm-log.jsonl"));
        (dir, log)
    }

    /// Per-test tempdir for invocation logs. Tests assert on bus behavior,
    /// not the contents of the per-invocation files; this is just a real
    /// directory the bus can write to without polluting `$GLASS_SYSTEM_DATA`.
    fn fresh_invocations_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn make_dispatcher<R: LoomRunner + 'static>(runner: R) -> Arc<Dispatcher> {
        Arc::new(Dispatcher::new(Arc::new(runner)))
    }

    /// Fixed timestamp every test uses for inbound DMs. Pinned so test
    /// assertions can match the formatted prompt prefix exactly.
    fn ts() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 5, 13, 14, 30, 0).unwrap()
    }

    fn dm(author: AuthorId, channel: ConversationId, content: &str) -> IncomingDm {
        IncomingDm {
            author,
            channel,
            content: content.into(),
            timestamp: ts(),
        }
    }

    /// The wire-shaped prompt the bus passes to the runner for `content`.
    fn prompt(content: &str) -> String {
        format!("[2026-05-13 14:30] {content}")
    }

    #[tokio::test]
    async fn run_loop_replies_to_owner_and_ignores_others() {
        let runner = Arc::new(MockLoomRunner::new(&[
            &["echo: hi"],
            &["echo: still there?"],
        ]));
        let dispatcher = Dispatcher::new(runner.clone());
        let (_dlog_dir, dm_log) = fresh_dm_log();
        let bus = StubBus::new();
        let owner = AuthorId(42);
        let intruder = AuthorId(99);
        let owner_channel = ConversationId(200);

        bus.push(dm(intruder, ConversationId(100), "intruder"))
            .await;
        bus.push(dm(owner, owner_channel, "hi")).await;
        bus.push(dm(owner, owner_channel, "still there?")).await;
        bus.close();

        let inv_dir = fresh_invocations_dir();
        run(
            &bus,
            &dispatcher,
            &dm_log,
            inv_dir.path(),
            &manifest(),
            owner,
        )
        .await
        .unwrap();

        let replies = bus.replies().await;
        assert_eq!(replies.len(), 2, "intruder message should be ignored");
        assert_eq!(replies[0].0, owner_channel);
        assert_eq!(replies[0].1, "echo: hi");
        assert_eq!(replies[1].1, "echo: still there?");

        let calls = runner.calls();
        assert_eq!(
            calls,
            vec![
                (manifest(), prompt("hi")),
                (manifest(), prompt("still there?")),
            ]
        );
    }

    #[tokio::test]
    async fn run_loop_skips_empty_messages() {
        let runner = Arc::new(MockLoomRunner::new(&[&["echo: real"]]));
        let dispatcher = Dispatcher::new(runner);
        let (_dlog_dir, dm_log) = fresh_dm_log();
        let bus = StubBus::new();
        let owner = AuthorId(42);

        bus.push(dm(owner, ConversationId(1), "   ")).await;
        bus.push(dm(owner, ConversationId(1), "real")).await;
        bus.close();

        let inv_dir = fresh_invocations_dir();
        run(
            &bus,
            &dispatcher,
            &dm_log,
            inv_dir.path(),
            &manifest(),
            owner,
        )
        .await
        .unwrap();
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

        // First call pends forever (will be cancelled). Second call returns
        // a canned reply. Genuinely different from MockLoomRunner (which
        // always pops a queue), so kept inline.
        struct InterruptableRunner {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl LoomRunner for InterruptableRunner {
            async fn run(
                &self,
                _manifest: &Path,
                prompt: &str,
                tx: mpsc::Sender<String>,
                _log: Option<&mut crate::invocation_log::InvocationLog>,
            ) -> Result<()> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                tx.send(format!("reply to {prompt}")).await.ok();
                Ok(())
            }
        }

        let runner = Arc::new(InterruptableRunner {
            calls: AtomicUsize::new(0),
        });
        let dispatcher = Arc::new(Dispatcher::new(runner.clone()));
        let (_dlog_dir, dm_log) = fresh_dm_log();
        let bus = Arc::new(StubBus::new());
        let owner = AuthorId(42);
        let channel = ConversationId(1);

        let bus_t = bus.clone();
        let dispatcher_t = dispatcher.clone();
        let dm_log_t = dm_log.clone();
        let inv_dir = fresh_invocations_dir();
        let inv_path = inv_dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run(
                &*bus_t,
                &dispatcher_t,
                &dm_log_t,
                &inv_path,
                &PathBuf::from("./manifests/glass.toml"),
                owner,
            )
            .await
        });

        // First message: runner pends forever.
        bus.push(dm(owner, channel, "first")).await;

        // Wait until the runner is reached, so we know the bot is mid-handle
        // when we push the second message.
        for _ in 0..1000 {
            if runner.calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);

        // Second message: should cancel the first handle, restart with this.
        bus.push(dm(owner, channel, "second")).await;
        bus.close();

        task.await.unwrap().unwrap();

        // Both messages reached the runner (first cancelled mid-flight, second completed).
        assert_eq!(runner.calls.load(Ordering::SeqCst), 2);

        // One reply was sent over the bus (the cancelled turn produces nothing).
        // The runner echoes back the prompt it received, including the
        // timestamp prefix the bus prepended.
        let replies = bus.replies().await;
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].1, format!("reply to {}", prompt("second")));
    }

    #[tokio::test(start_paused = true)]
    async fn typing_indicator_refreshes_during_long_run() {
        // Paused time lets us drive the typing-tick interval deterministically.
        // SlowRunner parks on a sleep we control via tokio::time::advance.
        use async_trait::async_trait;
        use std::time::Duration;

        struct SlowRunner;
        #[async_trait]
        impl LoomRunner for SlowRunner {
            async fn run(
                &self,
                _manifest: &Path,
                _prompt: &str,
                tx: mpsc::Sender<String>,
                _log: Option<&mut crate::invocation_log::InvocationLog>,
            ) -> Result<()> {
                tokio::time::sleep(Duration::from_secs(60)).await;
                tx.send("done".into()).await.ok();
                Ok(())
            }
        }

        let dispatcher = make_dispatcher(SlowRunner);
        let (_dlog_dir, dm_log) = fresh_dm_log();
        let bus = Arc::new(StubBus::new());
        let owner = AuthorId(42);

        bus.push(dm(owner, ConversationId(1), "hi")).await;

        let bus_t = bus.clone();
        let dispatcher_t = dispatcher.clone();
        let dm_log_t = dm_log.clone();
        let inv_dir = fresh_invocations_dir();
        let inv_path = inv_dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run(
                &*bus_t,
                &dispatcher_t,
                &dm_log_t,
                &inv_path,
                &PathBuf::from("./manifests/glass.toml"),
                owner,
            )
            .await
        });

        // Advance past the 60s runner sleep in 7s steps. Yield between
        // advances so the bot loop polls the typing interval.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(7)).await;
        }
        tokio::task::yield_now().await;

        bus.close();
        task.await.unwrap().unwrap();

        // The exact tick count depends on how quickly the bus reaches its
        // select! loop relative to paused-time advances (dm_log writes and
        // dispatcher lock acquisition each consume a few yield steps before
        // the inner select! starts ticking). What we care about is that the
        // typing indicator refreshes *many* times during a 60s run, not
        // exactly N. 60s / 7s ≈ 8, plus the immediate first tick ≈ 9;
        // anywhere in [6, 10] confirms the refresh behaviour.
        let count = bus.typing_count();
        assert!(
            (6..=10).contains(&count),
            "expected typing to refresh ~6-10 times during 60s, got {count}"
        );
        assert_eq!(bus.replies().await.len(), 1);
        assert_eq!(bus.replies().await[0].1, "done");
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
