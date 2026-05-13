use anyhow::Result;
use glass::{
    bus,
    bus::MessageBus,
    config::Config,
    cron::{self, CronStore, RemoveResult},
    discord,
    dispatcher::Dispatcher,
    dm_log::DmLog,
    loom::{LoomCli, LoomRunner},
    orchestrator_socket,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    match argv.as_slice() {
        [] => run_daemon().await,
        ["cron", "list"] => cmd_cron_list().await,
        ["cron", "rm", id] => cmd_cron_rm(id).await,
        ["help"] | ["-h"] | ["--help"] => {
            print_usage(&mut std::io::stdout())?;
            Ok(())
        }
        _ => {
            eprintln!("error: unknown command: {}\n", args.join(" "));
            print_usage(&mut std::io::stderr())?;
            std::process::exit(2);
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "glass=info,serenity=warn".into()),
        )
        .init();
}

fn print_usage<W: std::io::Write>(w: &mut W) -> std::io::Result<()> {
    writeln!(w, "usage:")?;
    writeln!(w, "  glass                 run the orchestrator daemon")?;
    writeln!(
        w,
        "  glass cron list       show currently scheduled cron entries"
    )?;
    writeln!(
        w,
        "  glass cron rm <id>    remove a scheduled entry (id or unique prefix)"
    )?;
    writeln!(w, "  glass help            show this message")?;
    Ok(())
}

// ─── glass cron list ───────────────────────────────────────────────────────

async fn cmd_cron_list() -> Result<()> {
    let cfg = Config::from_env()?;
    let store = CronStore::new(cfg.cron_path());
    let entries = store.list().await?;
    if entries.is_empty() {
        println!("No scheduled entries.");
        return Ok(());
    }
    let now = chrono::Local::now();
    let n = entries.len();
    println!(
        "{n} scheduled {}:\n",
        if n == 1 { "entry" } else { "entries" }
    );
    for entry in &entries {
        print!("{}", cron::format_entry_line(entry, now));
        println!();
    }
    Ok(())
}

// ─── glass cron rm ────────────────────────────────────────────────────────────

async fn cmd_cron_rm(id_or_prefix: &str) -> Result<()> {
    let cfg = Config::from_env()?;
    let store = CronStore::new(cfg.cron_path());
    match store.remove(id_or_prefix).await? {
        RemoveResult::Removed(entry) => {
            println!("Removed cron entry {}.", entry.id);
            let preview = entry.what.lines().next().unwrap_or("").trim();
            if !preview.is_empty() {
                println!("  prompt: {preview}");
            }
            Ok(())
        }
        RemoveResult::NotFound => {
            eprintln!("No cron entry matches {id_or_prefix:?}.");
            std::process::exit(1);
        }
        RemoveResult::Ambiguous(ids) => {
            eprintln!(
                "Prefix {id_or_prefix:?} matches {} entries; use a longer prefix or the full id:",
                ids.len()
            );
            for id in ids {
                eprintln!("  {id}");
            }
            std::process::exit(1);
        }
    }
}

// ─── daemon (default) ───────────────────────────────────────────────────────

async fn run_daemon() -> Result<()> {
    let cfg = Config::from_env()?;
    if !cfg.manifest.exists() {
        anyhow::bail!(
            "manifest not found at {} (set MANIFEST or run from the repo root)",
            cfg.manifest.display()
        );
    }
    if !cfg.cron_manifest.exists() {
        // Cron is best-effort — log a warning but don't refuse to start.
        // The poller will still run; entries will fail-to-dispatch with a
        // clean error in tracing rather than crashing the bot.
        tracing::warn!(
            cron_manifest = %cfg.cron_manifest.display(),
            "cron manifest not found; scheduled fires will fail to dispatch"
        );
    }
    cfg.ensure_system_layout()?;

    let dm_log = DmLog::new(cfg.dm_log_path());
    let socket_path = cfg.socket_path();
    let cron_store = CronStore::new(cfg.cron_path());
    let invocations_dir = cfg.invocations_dir();

    tracing::info!(
        manifest = %cfg.manifest.display(),
        cron_manifest = %cfg.cron_manifest.display(),
        loom_command = %cfg.loom_command,
        system_data = %cfg.system_data.display(),
        dm_log = %dm_log.path().display(),
        socket = %socket_path.display(),
        cron = %cron_store.path().display(),
        invocations = %invocations_dir.display(),
        "glass starting"
    );

    // Loom inherits two runtime-resolved paths as Loom secrets:
    //   `GLASS_ORCHESTRATOR_SOCK` — required by `send_dm` and `schedule`
    //                              tools (companion-tools package).
    //   `GLASS_DM_LOG`            — required by the `companion` session
    //                              layer (cron agent's `## recent
    //                              conversation` source).
    // Both are declared in the providers' `secrets.required` blocks and
    // surfaced to the consumer via `ctx.secrets`. The orchestrator never
    // reaches into the subprocess's process env directly.
    let runner: Arc<dyn LoomRunner> = Arc::new(
        LoomCli::new(cfg.loom_command)
            .with_env(
                "GLASS_ORCHESTRATOR_SOCK",
                socket_path.to_string_lossy().to_string(),
            )
            .with_env("GLASS_DM_LOG", dm_log.path().to_string_lossy().to_string()),
    );
    let dispatcher = Arc::new(Dispatcher::new(runner));

    let connected = discord::connect(&cfg.discord_token, cfg.operator_id).await?;
    tracing::info!(
        operator_channel = ?connected.operator_channel,
        "discord connected; operator DM channel resolved"
    );

    let bus_arc: Arc<dyn MessageBus> = Arc::new(connected.bus);
    orchestrator_socket::spawn(
        socket_path,
        cron_store.clone(),
        bus_arc.clone(),
        connected.operator_channel,
        dm_log.clone(),
    )
    .await?;

    cron::spawn_poller(
        cron_store,
        dispatcher.clone(),
        cfg.cron_manifest,
        invocations_dir.clone(),
        cron::DEFAULT_POLL_INTERVAL,
    );

    bus::run(
        &*bus_arc,
        &dispatcher,
        &dm_log,
        &invocations_dir,
        &cfg.manifest,
        cfg.operator_id,
    )
    .await?;

    Ok(())
}
