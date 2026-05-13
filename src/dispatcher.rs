use crate::invocation_log::InvocationLog;
use crate::loom::LoomRunner;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Serializes every Loom invocation through a single lock. The bus loop and
/// (in v0.4) the cron poller both call `dispatch`; whichever holds the lock
/// runs, the other waits. There is never more than one Glass turn running at
/// a time. Sequential is the feature: it gives Glass a single-threaded
/// mental model and rules out two cron fires (or a DM + a cron fire)
/// confusing each other.
pub struct Dispatcher {
    runner: Arc<dyn LoomRunner>,
    lock: Mutex<()>,
}

impl Dispatcher {
    pub fn new(runner: Arc<dyn LoomRunner>) -> Self {
        Self {
            runner,
            lock: Mutex::new(()),
        }
    }

    /// Acquire the turn lock and run one invocation. Cancellation is the
    /// same as before: the caller can drop the returned future, which drops
    /// the lock guard, the runner future, and the loom child (via
    /// `kill_on_drop`) in that order.
    pub async fn dispatch(
        &self,
        manifest: &Path,
        prompt: &str,
        tx: mpsc::Sender<String>,
        log: Option<&mut InvocationLog>,
    ) -> Result<()> {
        let _guard = self.lock.lock().await;
        self.runner.run(manifest, prompt, tx, log).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loom::testing::MockLoomRunner;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn dispatch_serializes_invocations() {
        use async_trait::async_trait;

        // Each invocation observes "active" being 1 while it runs. If the
        // dispatcher ran them concurrently, "active" would exceed 1.
        struct CountingRunner {
            active: AtomicUsize,
            max_seen: AtomicUsize,
        }
        #[async_trait]
        impl LoomRunner for CountingRunner {
            async fn run(
                &self,
                _manifest: &Path,
                _prompt: &str,
                _tx: mpsc::Sender<String>,
                _log: Option<&mut InvocationLog>,
            ) -> Result<()> {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let runner = Arc::new(CountingRunner {
            active: AtomicUsize::new(0),
            max_seen: AtomicUsize::new(0),
        });
        let dispatcher = Arc::new(Dispatcher::new(runner.clone()));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let d = dispatcher.clone();
            handles.push(tokio::spawn(async move {
                let (tx, _rx) = mpsc::channel(1);
                d.dispatch(&PathBuf::from("m"), "p", tx, None)
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(runner.max_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_forwards_to_runner() {
        let runner = Arc::new(MockLoomRunner::new(&[&["hello"]]));
        let dispatcher = Dispatcher::new(runner.clone());

        let (tx, mut rx) = mpsc::channel(4);
        dispatcher
            .dispatch(&PathBuf::from("./m.toml"), "hi there", tx, None)
            .await
            .unwrap();

        assert_eq!(rx.recv().await, Some("hello".to_string()));
        assert_eq!(
            runner.calls(),
            vec![(PathBuf::from("./m.toml"), "hi there".to_string())]
        );
    }
}
