use crate::bus::AuthorId;
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
