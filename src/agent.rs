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

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // Configurable mock: pops canned replies/summaries off queues, records
    // what each call saw. Empty queue → panic on call (acts as
    // `unreachable!` for tests that should never hit a given method).
    pub struct FakeAgent {
        replies: Mutex<VecDeque<String>>,
        summaries: Mutex<VecDeque<String>>,
        priors: Mutex<Vec<Vec<Event>>>,
        summary_calls: Mutex<Vec<(Option<String>, Vec<Event>)>>,
    }

    impl FakeAgent {
        pub fn new(replies: &[&str], summaries: &[&str]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
                summaries: Mutex::new(summaries.iter().map(|s| s.to_string()).collect()),
                priors: Mutex::new(vec![]),
                summary_calls: Mutex::new(vec![]),
            }
        }

        pub fn priors_seen(&self) -> Vec<Vec<Event>> {
            self.priors.lock().unwrap().clone()
        }

        pub fn summary_calls(&self) -> Vec<(Option<String>, Vec<Event>)> {
            self.summary_calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentRuntime for FakeAgent {
        async fn run_turn(&self, _w: &Path, history: &[Event], _m: &str) -> Result<String> {
            self.priors.lock().unwrap().push(history.to_vec());
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeAgent: out of replies"))
        }

        async fn summarize(
            &self,
            _w: &Path,
            prior: Option<&str>,
            transcript: &[Event],
        ) -> Result<String> {
            self.summary_calls
                .lock()
                .unwrap()
                .push((prior.map(String::from), transcript.to_vec()));
            Ok(self
                .summaries
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeAgent: out of summaries"))
        }
    }
}
