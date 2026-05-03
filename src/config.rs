use crate::bus::AuthorId;
use crate::events::{self, Event};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct Config {
    pub discord_token: String,
    pub owner_id: AuthorId,
    pub workspace: Workspace,
    pub pi_command: String,
    pub models_config_path: PathBuf,
}

#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn blocks_dir(&self) -> PathBuf {
        self.root.join("blocks")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn history_dir(&self) -> PathBuf {
        self.root.join("history")
    }
    pub fn agents_md(&self) -> PathBuf {
        self.root.join("AGENTS.md")
    }
    pub fn events_log(&self) -> PathBuf {
        self.history_dir().join("events.jsonl")
    }
    pub fn current_log(&self) -> PathBuf {
        self.history_dir().join("current.jsonl")
    }

    pub fn ensure_layout(&self) -> Result<()> {
        std::fs::create_dir_all(self.blocks_dir())?;
        std::fs::create_dir_all(self.state_dir())?;
        std::fs::create_dir_all(self.history_dir())?;
        Ok(())
    }

    // Audit first; if we crash between writes, audit is canonical and
    // current.jsonl is one event behind (recoverable, fail mode of choice).
    pub fn append_event(&self, event: &Event) -> Result<()> {
        events::append(&self.events_log(), event)?;
        events::append(&self.current_log(), event)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn tempdir() -> (tempfile::TempDir, Self) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = Workspace {
            root: tmp.path().to_path_buf(),
        };
        ws.ensure_layout().unwrap();
        (tmp, ws)
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let discord_token =
            std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN not set")?;

        let owner_id = std::env::var("OWNER_DISCORD_ID")
            .context("OWNER_DISCORD_ID not set")?
            .parse::<u64>()
            .map(AuthorId)
            .context("OWNER_DISCORD_ID must be a numeric Discord user ID")?;

        let workspace_root =
            std::env::var("WORKSPACE_PATH").unwrap_or_else(|_| "./workspace".into());

        let pi_command = std::env::var("PI_COMMAND").unwrap_or_else(|_| "pi".into());

        let models_config_path = PathBuf::from(
            std::env::var("MODELS_CONFIG").unwrap_or_else(|_| "./models.toml".into()),
        );

        Ok(Self {
            discord_token,
            owner_id,
            workspace: Workspace {
                root: PathBuf::from(workspace_root),
            },
            pi_command,
            models_config_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::testing::{read_log, E};

    #[test]
    fn append_event_writes_to_both_logs() {
        let (_tmp, ws) = Workspace::tempdir();
        ws.append_event(&Event::user("hello")).unwrap();
        ws.append_event(&Event::agent("hi")).unwrap();

        let expected = vec![E::user("hello"), E::agent("hi")];
        assert_eq!(read_log(&ws.events_log()), expected);
        assert_eq!(read_log(&ws.current_log()), expected);
    }
}
