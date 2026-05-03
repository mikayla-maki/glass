use crate::agent::AgentRuntime;
use crate::config::Workspace;
use crate::events::{self, Event};
use anyhow::Result;
use tracing::info;

#[derive(Debug, Clone)]
pub struct CompactionConfig {
    pub context_window_tokens: usize,
    pub threshold_pct: f32,
    pub keep_recent_tokens: usize,
}

impl CompactionConfig {
    pub fn threshold_tokens(&self) -> usize {
        (self.context_window_tokens as f32 * self.threshold_pct) as usize
    }
}

// Returns true if a compaction ran. Idempotent: calling twice in a row
// when only one compaction was needed is a no-op the second time.
pub async fn maybe_compact(
    workspace: &Workspace,
    runtime: &dyn AgentRuntime,
    cfg: &CompactionConfig,
) -> Result<bool> {
    let current_path = workspace.current_log();
    let events_path = workspace.events_log();
    let current = events::load_log(&current_path)?;

    let current_tokens: usize = current.iter().map(Event::estimated_tokens).sum();
    let threshold = cfg.threshold_tokens();

    if current_tokens < threshold {
        return Ok(false);
    }

    info!(
        current_tokens,
        threshold, "compaction threshold reached, compacting"
    );

    // Walk back accumulating tokens until we hit keep_recent. `cut` splits
    // current[..cut] (folded into summary) from current[cut..] (verbatim).
    let mut tail_tokens = 0;
    let mut cut = current.len();
    for i in (0..current.len()).rev() {
        tail_tokens += current[i].estimated_tokens();
        if tail_tokens >= cfg.keep_recent_tokens {
            cut = i;
            break;
        }
    }

    if cut == 0 {
        // keep_recent > total tokens; nothing older to compact.
        return Ok(false);
    }

    let to_compact = &current[..cut];

    // Peel a prior summary off the head and pass it forward so facts from
    // earlier-still compactions don't get lost.
    let prior_summary: Option<String> = match to_compact.first() {
        Some(Event::Summary { body, .. }) => Some(body.clone()),
        _ => None,
    };
    let transcript: Vec<Event> = if prior_summary.is_some() {
        to_compact[1..].to_vec()
    } else {
        to_compact.to_vec()
    };

    info!(
        events_to_compact = transcript.len(),
        events_kept = current.len() - cut,
        "summarizing"
    );

    let body = runtime
        .summarize(&workspace.root, prior_summary.as_deref(), &transcript)
        .await?;

    let new_summary = Event::summary(body);

    events::append(&events_path, &new_summary)?;
    let new_current: Vec<Event> = std::iter::once(new_summary)
        .chain(current[cut..].iter().cloned())
        .collect();
    events::write_atomic(&current_path, &new_current)?;

    Ok(true)
}

#[cfg(test)]
pub mod testing {
    use super::CompactionConfig;

    pub fn tight() -> CompactionConfig {
        CompactionConfig {
            context_window_tokens: 1000,
            threshold_pct: 0.7, // = 700-token threshold
            keep_recent_tokens: 200,
        }
    }

    pub fn loose() -> CompactionConfig {
        CompactionConfig {
            context_window_tokens: 10_000_000,
            threshold_pct: 0.7,
            keep_recent_tokens: 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{loose, tight};
    use super::*;
    use crate::agent::testing::FakeAgent;
    use crate::events::testing::{read_log, simplify, E};

    fn seed(ws: &Workspace, events: &[Event]) {
        for e in events {
            ws.append_event(e).unwrap();
        }
    }

    #[tokio::test]
    async fn no_op_when_under_threshold() {
        let (_tmp, ws) = Workspace::tempdir();
        seed(
            &ws,
            &[
                Event::user("hello"),
                Event::agent("hi"),
                Event::user("brief chat"),
            ],
        );
        let runtime = FakeAgent::new(&[], &[]);

        let did = maybe_compact(&ws, &runtime, &loose()).await.unwrap();

        assert!(!did);
        assert!(runtime.summary_calls().is_empty());
        let expected = vec![E::user("hello"), E::agent("hi"), E::user("brief chat")];
        assert_eq!(read_log(&ws.events_log()), expected);
        assert_eq!(read_log(&ws.current_log()), expected);
    }

    #[tokio::test]
    async fn rewrites_current_log_with_summary_plus_kept_tail() {
        let (_tmp, ws) = Workspace::tempdir();
        // Each ~200 tokens; 4 events = ~800 tokens, over the 700 threshold.
        let big = "x".repeat(800);
        seed(
            &ws,
            &[
                Event::user(&big),
                Event::agent(&big),
                Event::user(&big),
                Event::agent(&big),
            ],
        );
        let runtime = FakeAgent::new(&[], &["the summary"]);

        let did = maybe_compact(&ws, &runtime, &tight()).await.unwrap();

        assert!(did);

        // Audit: the four originals + a Summary at the end. Untouched history.
        assert_eq!(
            read_log(&ws.events_log()),
            vec![
                E::user(&big),
                E::agent(&big),
                E::user(&big),
                E::agent(&big),
                E::summary("the summary"),
            ]
        );

        // Current: [Summary, kept tail]. With keep_recent=200 and the agent-big
        // tail event being ~200 tokens, exactly one event is kept.
        assert_eq!(
            read_log(&ws.current_log()),
            vec![E::summary("the summary"), E::agent(&big)]
        );
    }

    #[tokio::test]
    async fn carries_prior_summary_forward_into_next_summarize_call() {
        let (_tmp, ws) = Workspace::tempdir();
        let big = "x".repeat(800);
        // Pre-seed current.jsonl with a Summary at the head — the post-compaction
        // shape from a previous run.
        ws.append_event(&Event::summary("first summary")).unwrap();
        for _ in 0..4 {
            ws.append_event(&Event::user(&big)).unwrap();
            ws.append_event(&Event::agent(&big)).unwrap();
        }
        let runtime = FakeAgent::new(&[], &["second summary"]);

        let did = maybe_compact(&ws, &runtime, &tight()).await.unwrap();

        assert!(did);

        // The summarize call received the prior summary as `prior`,
        // and the transcript contained no Summary events (it was peeled off).
        let calls = runtime.summary_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.as_deref(), Some("first summary"));
        assert!(calls[0]
            .1
            .iter()
            .all(|e| !matches!(e, Event::Summary { .. })));

        // Current log now starts with the NEW summary, not the old one.
        let current = read_log(&ws.current_log());
        assert_eq!(current.first(), Some(&E::summary("second summary")));
    }

    #[tokio::test]
    async fn second_call_in_a_row_is_a_no_op() {
        let (_tmp, ws) = Workspace::tempdir();
        let big = "x".repeat(800);
        for _ in 0..4 {
            ws.append_event(&Event::user(&big)).unwrap();
            ws.append_event(&Event::agent(&big)).unwrap();
        }
        let runtime = FakeAgent::new(&[], &["summary"]);

        // First call: triggers.
        assert!(maybe_compact(&ws, &runtime, &tight()).await.unwrap());
        // Second call: current.jsonl is now small ([Summary, kept]); under threshold.
        assert!(!maybe_compact(&ws, &runtime, &tight()).await.unwrap());
    }

    // Regression: an earlier single-file design walked back to the Summary
    // and silently dropped the kept tail. Pin it.
    #[tokio::test]
    async fn does_not_lose_kept_tail() {
        let (_tmp, ws) = Workspace::tempdir();
        let big = "x".repeat(800);
        seed(
            &ws,
            &[
                Event::user(&big),
                Event::agent(&big),
                Event::user(&big),
                Event::agent("specifically this kept tail event"),
            ],
        );
        let runtime = FakeAgent::new(&[], &["summary"]);

        maybe_compact(&ws, &runtime, &tight()).await.unwrap();

        let current = simplify(&events::load_log(&ws.current_log()).unwrap());
        assert!(
            current
                .iter()
                .any(|e| matches!(e, E::Agent(s) if s == "specifically this kept tail event")),
            "expected the kept tail to be preserved verbatim in current.jsonl, got: {current:?}"
        );
    }
}
