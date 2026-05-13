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

Glass is three pieces:

- A **Rust orchestrator** (this repo, in `src/`). Owns Discord, the turn
  lock (sequential dispatch), the cron poller, the unix socket companion
  tools talk back on, and per-invocation logging. Doesn't run any agent
  loop itself — it spawns `loom prompt` per turn and streams the output
  back to Discord.
- **[Loom](https://github.com/mcmaki/loom)**, vendored as an npm workspace
  dependency. Runs the actual agent loop: harness, session, tool dispatch.
  Glass configures it through manifests in `manifests/`.
- **TypeScript providers** (under `providers/`) that contribute the
  Glass-specific bits to Loom: identity & current-time system-prompt
  sections, the `schedule`/`send_dm` tools, the `dm-history` session layer,
  and the gcalcli wrapper.

### Two agents, one self

Glass runs in two contexts that share `_glass/soul.md` (so it's one
identity) but differ in everything else:

- **DM agent** (`manifests/glass.toml`) — interactive. Session layers are
  `["compacting", "identity", "file"]`: `compacting` keeps context
  bounded as the chat grows; `file` persists across restarts. Output
  streams to Discord in real time.
- **Cron agent** (`manifests/cron.toml`) — stateless per fire, woken by
  scheduled prompts the agent set for herself. Session layers are
  `["identity", "companion", "in-memory"]`: no cross-fire persistence, but
  the `companion` layer surfaces the last ~20 DM messages as `## recent
  conversation` so she has context for what to say (or not say). Output
  reaches you only through explicit `send_dm` calls; otherwise the turn
  is silent.

### Three storage layers

- **Vault** (`$GLASS_VAULT_DIR`) — Glass-the-agent's content. Her soul,
  her notes, anything she writes. Sync it however you like (Dropbox,
  iCloud, git). The manifests pin all filesystem capabilities here, so
  this is also the security boundary for her file access.
- **Loom data** (`~/Library/Application Support/loom/agents/glass/`) —
  Loom's session storage: every turn's events, the `FileSession`'s log.
  Loom owns this.
- **Glass system** (`$GLASS_SYSTEM_DATA`, default
  `~/Library/Application Support/Glass/`) — orchestrator operational
  state: `dm-log.jsonl` (every inbound/outbound DM), `cron.jsonl`
  (scheduled entries), `invocations/*.jsonl` (per-turn audit logs with
  the full assembled system prompt + every agent event), and the
  `orchestrator.sock` companion tools connect to.

### Trust line

Two enforced boundaries, both at startup:

- **Operator gate.** Every incoming DM is checked against `OPERATOR_DISCORD_ID`
  before reaching Loom. Anyone else is dropped silently with a warn log.
- **Capability paths.** `bash`, `read_file`, `write_file`, `edit_file`,
  and `find` are all scoped to `$GLASS_VAULT_DIR` in both manifests via
  `${GLASS_VAULT_DIR}` substitution. Glass can't escape her vault for
  filesystem ops; `bash` runs under macOS's `sandbox-exec` via Loom's
  built-in sandbox integration.

The operators's identity (name, relationship, history, preferences) lives in
`_glass/soul.md` — per-install user content, not committed code. Glass
reads it into her system prompt every turn via the `glass-identity`
provider. That's why this repo is genuinely shareable: the code has no
hardcoded sense of *who* it's for.

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
- `jq -c . < "$GLASS_SYSTEM_DATA/invocations/<file>.jsonl"` — forensicate
  any past turn. Each file has the assembled system prompt, every agent
  event, and the final outcome.

### Autostart at login (macOS)

`./scripts/install-autostart.sh` installs a LaunchAgent at
`~/Library/LaunchAgents/dev.glass.glass.plist`. It builds the release
binary, builds the providers, writes the plist, and loads it via
`launchctl`. Glass then starts at login and auto-restarts on crash; logs
go to `$GLASS_SYSTEM_DATA/launchd.{out,err}.log`.

Idempotent — re-run after `git pull` to rebuild and reload. Uninstall
with `./scripts/uninstall-autostart.sh`. Manual control via the standard
`launchctl load|unload` against the plist path.

(Linux: equivalent setup via a systemd user service. Not scripted here.)

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
- `AGENTS.md` — project rules for human/AI contributors.
