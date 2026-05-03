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

    pub async fn handle(&self, user_text: &str) -> Result<String> {
        let _guard = self.turn_lock.lock().await;

        memory::render_agents_md(&self.workspace)?;

        // Compact BEFORE appending the new user msg so any new Summary lands
        // behind it and the user msg ends up at the tail of the active window.
        compaction::maybe_compact(&self.workspace, &*self.runtime, &self.compaction_cfg).await?;

        self.workspace.append_event(&Event::user(user_text))?;

        // Trailing event is the user msg we just appended; split it off so
        // it appears in the prompt as "owner just sent" rather than in the
        // recent-history section.
        let current = events::load_log(&self.workspace.current_log())?;
        let (trailing, prior) = match current.split_last() {
            Some((Event::UserMessage { content, .. }, rest)) => (content.clone(), rest.to_vec()),
            _ => bail!("expected trailing UserMessage in current log after append; log malformed"),
        };

        let reply = self
            .runtime
            .run_turn(&self.workspace.root, &prior, &trailing)
            .await?;

        self.workspace.append_event(&Event::agent(reply.clone()))?;

        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testing::FakeAgent;
    use crate::compaction::testing::{loose, tight};
    use crate::events::testing::{read_log, E};

    fn fresh_ws() -> (tempfile::TempDir, Workspace) {
        let (tmp, ws) = Workspace::tempdir();
        std::fs::write(ws.blocks_dir().join("identity.md"), "# Identity\nGlass.").unwrap();
        (tmp, ws)
    }

    #[tokio::test]
    async fn two_turns_no_compaction_both_logs_match() {
        let (_tmp, ws) = fresh_ws();
        let runtime = Arc::new(FakeAgent::new(&["hi", "still here"], &[]));
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
        let runtime = Arc::new(FakeAgent::new(
            &["r1", "r2", "r3", "r4", "r5"],
            &["compaction summary"],
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
        let (_tmp, ws) = fresh_ws();
        let big = "x".repeat(800);
        let runtime = Arc::new(FakeAgent::new(
            &["r1", "r2", "r3", "r4", "r5"],
            &["compacted"],
        ));
        let engine = TurnEngine::new(ws, runtime.clone(), tight());

        for _ in 0..5 {
            engine.handle(&big).await.unwrap();
        }

        // After at least one compaction, some run_turn call's `prior` should
        // start with a Summary.
        let priors = runtime.priors_seen();
        let any_starts_with_summary = priors
            .iter()
            .any(|p| matches!(p.first(), Some(Event::Summary { .. })));
        assert!(
            any_starts_with_summary,
            "expected a turn whose prior starts with a Summary"
        );
    }
}
