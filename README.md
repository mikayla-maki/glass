# Glass

*A Claude glass — a small tinted mirror that simplifies and reflects the essence of the world.*

Glass is a long-running personal companion. It surfaces today as a Discord DM
bot. The agent itself runs inside [Loom](https://github.com/mikayla-anne/loom),
a manifest-driven agent runtime; this repo is a thin Rust orchestrator around
it.

Named for the [Claude glass](https://en.wikipedia.org/wiki/Claude_glass), the
pocket mirror 18th-century travelers used to see the world reflected in
simpler, more essential tones. Claude is also the model powering it.

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

1. **Configure secrets.** Copy `.env.example` to `.env` and fill in
   `DISCORD_BOT_TOKEN`, `OWNER_DISCORD_ID`, and `ANTHROPIC_API_KEY`. All
   other env vars have working defaults.
2. **Point the manifest at your vault.** Edit `manifests/glass.toml` and
   replace every occurrence of `/Users/mikaylamaki/Dropbox/glass-vault`
   under `[capabilities]` with the absolute path to your own vault
   directory. (Loom doesn't expand `~` in capability paths yet — worth
   filing upstream.) If your vault isn't at `~/Dropbox/glass-vault`, also
   add a `[session.identity]` block:

   ```toml
   [session.identity]
   vault_path = "/absolute/path/to/your/vault"
   ```
3. **Drop a soul file.** Create `<vault>/_glass/soul.md` with whatever
   identity text you want loaded into the system prompt every turn.
   Empty / missing is fine; the agent can write its own.
4. **Run it.** `./scripts/run.sh`. First run takes a minute (npm install +
   cargo build); subsequent runs are fast.

### Day to day

- `./scripts/run.sh` — start the bot.
- `./node_modules/.bin/loom audit manifests/glass.toml` — dump the
  resolved capability tree without running anything.
- `./node_modules/.bin/loom prompt manifests/glass.toml "hello"` — one-shot
  prompt from the CLI, bypassing Discord.
- `./node_modules/.bin/loom run manifests/glass.toml` — interactive REPL
  against the same agent.

### Storage layers

- **Vault** (`~/Dropbox/glass-vault` by default) — Glass-the-agent's
  content. Synced. Soul, journal, notes.
- **Loom data** (`~/Library/Application Support/loom/agents/glass/` on
  macOS) — Loom's session storage. Not synced. Owned by Loom.
- **Glass system** (`~/Library/Application Support/Glass/` on macOS) —
  orchestrator's operational state. Not synced. Override with
  `GLASS_SYSTEM_DATA` if you want it somewhere specific.

## Layout

- `src/` — Rust orchestrator (Discord bus, Loom subprocess wrapper, config).
- `manifests/glass.toml` — the agent manifest (model, tools, session, caps).
- `providers/glass-identity/` — npm package that loads `_glass/soul.md` into
  the system prompt.
- `docs/` — architecture + historical design notes.
- `AGENTS.md` — project rules for human/AI contributors.
