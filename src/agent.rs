use crate::events::Event;
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

// Two methods, one trait, so tests can stub the LLM with one mock.
// PiRuntime spawns `pi -p` for both; alternatives could call the API
// directly or use a cheaper model for summarize.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn run_turn(
        &self,
        workspace: &Path,
        history: &[Event],
        user_message: &str,
    ) -> Result<String>;

    async fn summarize(
        &self,
        workspace: &Path,
        prior_summary: Option<&str>,
        transcript: &[Event],
    ) -> Result<String>;
}
