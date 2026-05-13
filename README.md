# Glass

*A Claude glass — a small tinted mirror that simplifies and reflects the essence of the world.*

Glass is a long-running personal companion that surfaces today as a Discord DM
bot. The agent itself runs inside [Loom](https://github.com/mcmaki/loom), a
manifest-driven agent runtime; this repo is a thin Rust orchestrator around it.

Named for the [Claude glass](https://en.wikipedia.org/wiki/Claude_glass), the
pocket mirror 18th-century travelers used to see the world reflected in
simpler, more essential tones. Claude is also the model powering it.

This project is opinionated but generic: clone it, point it at your vault, and
you have your own Glass. The bot's identity (its "self") lives in your vault's
`_glass/soul.md` and is whatever you and the bot make of it together.

## Architecture

The real design lives in [`docs/architecture.md`](./docs/architecture.md).
Read that first. Historical designs:
[`docs/refactor-v0.2.md`](./docs/refactor-v0.2.md) (the previous Rust-only
shape) and [`docs/old-design.md`](./docs/old-design.md) (the original v0
sketch).

## Running

Loom is vendored as a workspace dependency (see `package.json`), so there's
no global install step.

### First time

1. **Pick a vault directory.** Anywhere on your filesystem. A directory inside
   a synced folder (Dropbox / iCloud Drive / git) is recommended so the agent's
   content survives across machines, but it's not required. Create the
   directory (e.g. `mkdir -p ~/Dropbox/glass-vault`) and a `_glass/`
   subdirectory inside it.

2. **Configure environment.** Copy `.env.example` to `.env` and fill in:
   - `DISCORD_BOT_TOKEN` — from <https://discord.com/developers/applications>.
   - `OPERATOR_DISCORD_ID` — your numeric Discord user ID (Developer Mode →
     right-click profile → Copy User ID).
   - `ANTHROPIC_API_KEY` — Claude API key.
   - `GLASS_VAULT_DIR` — absolute path to the vault you just created. Must
     be absolute (no `~`); the manifests reference it via `${GLASS_VAULT_DIR}`
     substitution. All capability paths point at this directory, so Glass can
     only read/write/edit files under it.

3. **(Optional) Drop a soul file.** Create `<vault>/_glass/soul.md` with
   whatever identity text you want loaded into the system prompt every turn —
   your name, your relationship with the bot, anything she should remember.
   Empty or missing is fine; she can write her own soul as she gets to know
   you.

4. **(Optional) Install `gcalcli`** if you want calendar tools. `pip install
   gcalcli` (or `brew install gcalcli`), then `gcalcli init` to authorize.
   Without it, `calendar_list` and `calendar_add` will return a clear "not on
   PATH" error; everything else works.

5. **Run it.** `./scripts/run.sh`. First run takes a minute (`npm install` +
   `cargo build`); subsequent runs are fast.

### Day to day

- `./scripts/run.sh` — start the bot.
- `cargo run -- cron list` — show currently scheduled cron entries.
- `cargo run -- cron rm <id>` — remove one (id or unique prefix).
- `./node_modules/.bin/loom audit manifests/glass.toml` — dump the resolved
  capability tree without running anything.
- `./node_modules/.bin/loom prompt manifests/glass.toml "hello"` — one-shot
  prompt from the CLI, bypassing Discord.

### Storage layers

- **Vault** (`$GLASS_VAULT_DIR`) — Glass-the-agent's content. What she thinks
  and writes about. Sync this however you like.
- **Loom data** (`~/Library/Application Support/loom/agents/glass/` on macOS,
  `$XDG_DATA_HOME/loom/agents/glass/` on Linux) — Loom's session storage.
- **Glass system** (`~/Library/Application Support/Glass/` on macOS,
  `$XDG_DATA_HOME/glass/` on Linux) — orchestrator state: dm-log,
  invocations/, cron.jsonl, orchestrator.sock. Override with
  `GLASS_SYSTEM_DATA`. Per-invocation transcripts (system prompt + every
  agent event + outcome) live in `invocations/`; `jq` them for forensics.

## Layout

- `src/` — Rust orchestrator (Discord bus, Loom subprocess wrapper, cron
  poller, invocation logger, CLI).
- `manifests/glass.toml` — DM agent manifest (model, tools, session, caps).
- `manifests/cron.toml` — cron agent manifest (stateless per fire,
  `send_dm`-only output).
- `providers/glass-identity/` — loads `_glass/soul.md` and current time into
  the system prompt every turn.
- `providers/glass-companion-tools/` — `send_dm`, `schedule`, plus a session
  layer that surfaces recent conversation to the cron agent.
- `providers/glass-calendar/` — typed wrapper around `gcalcli` for read +
  add.
- `docs/` — architecture + historical design notes.
- `AGENTS.md` — project rules for human/AI contributors.
