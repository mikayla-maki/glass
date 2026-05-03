use crate::agent::AgentRuntime;
use crate::compaction::{self, CompactionConfig};
use crate::config::Workspace;
use crate::events::{self, Event};
use crate::memory;
use anyhow::{bail, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

// Holds the turn lock; one DM thinking at a time regardless of input source.
pub struct TurnEngine {
    workspace: Workspace,
    runtime: Arc<dyn AgentRuntime>,
    compaction_cfg: CompactionConfig,
    turn_lock: Mutex<()>,
}

impl TurnEngine {
    pub fn new(
        workspace: Workspace,
        runtime: Arc<dyn AgentRuntime>,
        compaction_cfg: CompactionConfig,
    ) -> Self {
        Self {
            workspace,
            runtime,
            compaction_cfg,
            turn_lock: Mutex::new(()),
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub async fn handle(&self, user_text: &str) -> Result<String> {
        let _guard = self.turn_lock.lock().await;
        let events_path = self.workspace.events_log();
        let current_path = self.workspace.current_log();

        memory::render_agents_md(&self.workspace)?;

        // Compact BEFORE appending the new user msg so any new Summary lands
        // behind it and the user msg ends up at the tail of the active window.
        compaction::maybe_compact(&self.workspace, &*self.runtime, &self.compaction_cfg).await?;

        events::append_to_both(&events_path, &current_path, &Event::user(user_text))?;

        // Trailing event is the user msg we just appended; split it off so
        // it appears in the prompt as "owner just sent" rather than in the
        // recent-history section.
        let current = events::load_log(&current_path)?;
        let (trailing, prior) = match current.split_last() {
            Some((Event::UserMessage { content, .. }, rest)) => (content.clone(), rest.to_vec()),
            _ => bail!("expected trailing UserMessage in current log after append; log malformed"),
        };

        let reply = self
            .runtime
            .run_turn(&self.workspace.root, &prior, &trailing)
            .await?;

        events::append_to_both(&events_path, &current_path, &Event::agent(reply.clone()))?;

        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::testing::{read_log, E};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    struct FakeRuntime {
        replies: StdMutex<VecDeque<String>>,
        summaries: StdMutex<VecDeque<String>>,
    }

    impl FakeRuntime {
        fn new(replies: Vec<&'static str>, summaries: Vec<&'static str>) -> Self {
            Self {
                replies: StdMutex::new(replies.into_iter().map(String::from).collect()),
                summaries: StdMutex::new(summaries.into_iter().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl AgentRuntime for FakeRuntime {
        async fn run_turn(&self, _w: &Path, _h: &[Event], _m: &str) -> Result<String> {
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeRuntime: out of replies"))
        }
        async fn summarize(
            &self,
            _w: &Path,
            _prior: Option<&str>,
            _transcript: &[Event],
        ) -> Result<String> {
            Ok(self
                .summaries
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeRuntime: out of summaries"))
        }
    }

    fn fresh_ws() -> (TempDir, Workspace) {
        let tmp = TempDir::new().unwrap();
        let ws = Workspace {
            root: tmp.path().to_path_buf(),
        };
        ws.ensure_layout().unwrap();
        std::fs::write(ws.blocks_dir().join("identity.md"), "# Identity\nGlass.").unwrap();
        (tmp, ws)
    }

    fn loose() -> CompactionConfig {
        CompactionConfig {
            context_window_tokens: 10_000_000,
            threshold_pct: 0.7,
            keep_recent_tokens: 1000,
        }
    }

    fn tight() -> CompactionConfig {
        CompactionConfig {
            context_window_tokens: 1000,
            threshold_pct: 0.7,
            keep_recent_tokens: 200,
        }
    }

    #[tokio::test]
    async fn two_turns_no_compaction_both_logs_match() {
        let (_tmp, ws) = fresh_ws();
        let runtime = Arc::new(FakeRuntime::new(vec!["hi", "still here"], vec![]));
        let engine = TurnEngine::new(ws.clone(), runtime, loose());

        engine.handle("hello").await.unwrap();
        engine.handle("how are you?").await.unwrap();

        let expected = vec![
            E::user("hello"),
            E::agent("hi"),
            E::user("how are you?"),
            E::agent("still here"),
        ];
        assert_eq!(read_log(&ws.events_log()), expected);
        assert_eq!(read_log(&ws.current_log()), expected);
    }

    #[tokio::test]
    async fn compaction_diverges_logs_audit_keeps_everything_current_compacts() {
        let (_tmp, ws) = fresh_ws();
        // ~216 tokens/turn (big_user ~208 + reply ~8). Threshold 700 trips
        // on turn 5's compaction check (sees ~864 tokens of prior history).
        let big = "x".repeat(800);
        let runtime = Arc::new(FakeRuntime::new(
            vec!["r1", "r2", "r3", "r4", "r5"],
            vec!["compaction summary"],
        ));
        let engine = TurnEngine::new(ws.clone(), runtime, tight());

        for _ in 0..5 {
            engine.handle(&big).await.unwrap();
        }

        // Audit log keeps every event ever — 5 user messages, 5 replies, and
        // the inserted Summary.
        let audit = read_log(&ws.events_log());
        let summaries = audit.iter().filter(|e| matches!(e, E::Summary(_))).count();
        let users = audit.iter().filter(|e| matches!(e, E::User(_))).count();
        let agents = audit.iter().filter(|e| matches!(e, E::Agent(_))).count();
        assert_eq!(users, 5);
        assert_eq!(agents, 5);
        assert!(
            summaries >= 1,
            "expected ≥1 Summary in audit; got {summaries}"
        );

        // Current log is bounded: at minimum a Summary, plus at most a
        // handful of recent verbatim events.
        let current = read_log(&ws.current_log());
        assert!(
            current.iter().any(|e| matches!(e, E::Summary(_))),
            "current log should have a Summary after compaction: {current:?}"
        );
        assert!(
            current.len() < audit.len(),
            "current ({}) should be shorter than audit ({})",
            current.len(),
            audit.len()
        );
    }

    #[tokio::test]
    async fn runtime_receives_summary_at_head_of_prior_after_compaction() {
        // Captures what the runtime sees in `prior` so we can assert the
        // post-compaction shape directly.
        struct CapturingRuntime {
            replies: StdMutex<VecDeque<String>>,
            summaries: StdMutex<VecDeque<String>>,
            run_priors: StdMutex<Vec<Vec<E>>>,
        }
        #[async_trait]
        impl AgentRuntime for CapturingRuntime {
            async fn run_turn(&self, _w: &Path, history: &[Event], _m: &str) -> Result<String> {
                self.run_priors
                    .lock()
                    .unwrap()
                    .push(crate::events::testing::simplify(history));
                Ok(self.replies.lock().unwrap().pop_front().unwrap())
            }
            async fn summarize(
                &self,
                _w: &Path,
                _prior: Option<&str>,
                _t: &[Event],
            ) -> Result<String> {
                Ok(self.summaries.lock().unwrap().pop_front().unwrap())
            }
        }

        let (_tmp, ws) = fresh_ws();
        let big = "x".repeat(800);
        let runtime = Arc::new(CapturingRuntime {
            replies: StdMutex::new(
                ["r1", "r2", "r3", "r4", "r5"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            summaries: StdMutex::new(["compacted"].iter().map(|s| s.to_string()).collect()),
            run_priors: StdMutex::new(vec![]),
        });
        let engine = TurnEngine::new(ws, runtime.clone(), tight());

        for _ in 0..5 {
            engine.handle(&big).await.unwrap();
        }

        // After at least one compaction, some run_turn call's `prior` should
        // start with a Summary.
        let priors = runtime.run_priors.lock().unwrap();
        let any_starts_with_summary = priors
            .iter()
            .any(|p| matches!(p.first(), Some(E::Summary(_))));
        assert!(
            any_starts_with_summary,
            "expected a turn whose prior starts with a Summary; got {priors:#?}"
        );
    }
}
