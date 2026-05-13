use crate::bus::AuthorId;
use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct Config {
    pub discord_token: String,
    pub owner_id: AuthorId,
    pub loom_command: String,
    pub manifest: PathBuf,
    pub cron_manifest: PathBuf,
    pub system_data: PathBuf,
}

impl Config {
    pub fn dm_log_path(&self) -> PathBuf {
        self.system_data.join("dm-log.jsonl")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.system_data.join("orchestrator.sock")
    }

    pub fn cron_path(&self) -> PathBuf {
        self.system_data.join("cron.jsonl")
    }

    pub fn invocations_dir(&self) -> PathBuf {
        self.system_data.join("invocations")
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let discord_token =
            std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN not set")?;

        let owner_id = std::env::var("OPERATOR_DISCORD_ID")
            .context("OPERATOR_DISCORD_ID not set")?
            .parse::<u64>()
            .map(AuthorId)
            .context("OPERATOR_DISCORD_ID must be a numeric Discord user ID")?;

        let loom_command = std::env::var("LOOM_COMMAND").unwrap_or_else(|_| "loom".into());

        let manifest = PathBuf::from(
            std::env::var("MANIFEST").unwrap_or_else(|_| "./manifests/glass.toml".into()),
        );

        let cron_manifest = PathBuf::from(
            std::env::var("CRON_MANIFEST").unwrap_or_else(|_| "./manifests/cron.toml".into()),
        );

        let system_data = match std::env::var("GLASS_SYSTEM_DATA") {
            Ok(p) => PathBuf::from(p),
            Err(_) => dirs::data_dir()
                .context("could not resolve platform data dir for default GLASS_SYSTEM_DATA")?
                .join("Glass"),
        };

        Ok(Self {
            discord_token,
            owner_id,
            loom_command,
            manifest,
            cron_manifest,
            system_data,
        })
    }

    /// Create the Glass system storage directory if it doesn't exist. The
    /// dm-log lives at the root of this dir; v0.4 will add `invocations/`
    /// and an `orchestrator.sock` here as well.
    pub fn ensure_system_layout(&self) -> Result<()> {
        std::fs::create_dir_all(&self.system_data).with_context(|| {
            format!(
                "failed to create GLASS_SYSTEM_DATA at {}",
                self.system_data.display()
            )
        })?;
        std::fs::create_dir_all(self.invocations_dir()).with_context(|| {
            format!(
                "failed to create invocations dir at {}",
                self.invocations_dir().display()
            )
        })?;
        Ok(())
    }
}
