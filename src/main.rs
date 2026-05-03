use anyhow::Result;
use glass::{
    bus, compaction::CompactionConfig, config::Config, discord, models::ModelsConfig,
    pi::PiRuntime, turn::TurnEngine,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "glass=info,serenity=warn".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    cfg.workspace.ensure_layout()?;

    let model = ModelsConfig::load(&cfg.models_config_path)?.resolve()?;
    tracing::info!(
        name = %model.name,
        context_window = model.context_window_tokens,
        "selected model"
    );

    let runtime = Arc::new(PiRuntime {
        command: cfg.pi_command.clone(),
        model_arg: model.pi_arg.clone(),
        extra_args: model.extra_pi_args.clone(),
    });

    let compaction_cfg = CompactionConfig {
        context_window_tokens: model.context_window_tokens,
        threshold_pct: model.compaction_threshold_pct,
        keep_recent_tokens: model.keep_recent_tokens,
    };

    let engine = TurnEngine::new(cfg.workspace.clone(), runtime, compaction_cfg);

    let (sbus, _gateway) = discord::connect(&cfg.discord_token).await?;
    bus::run(&sbus, &engine, cfg.owner_id).await?;

    Ok(())
}
