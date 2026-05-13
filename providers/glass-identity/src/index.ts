/**
 * @glass/identity — contributes a Session that sets up Glass's per-turn
 * situational context: the contents of `_glass/soul.md` from the vault
 * (under a `## soul` header) and the current wall-clock time in the
 * orchestrator's local timezone.
 *
 * Time lives here, alongside soul, because both are answers to "where /
 * when am I right now?" — the same kind of context that pairs with a
 * static identity to ground a turn. The companion-tools package handles
 * outward action (send_dm, schedule); identity handles inward grounding.
 *
 * The agent updates soul.md via the regular `edit_file` builtin (gated by
 * the manifest's `[capabilities.edit_file].paths`); this provider never
 * writes.
 *
 * Manifest shape:
 *
 *   [providers]
 *   identity = { path = "../providers/glass-identity" }
 *
 *   [session]
 *   layers = ["compacting", "identity", "file"]
 *
 *   [session.identity]
 *   vault_path = "${GLASS_VAULT_DIR}"
 */

import * as path from "node:path";
import * as os from "node:os";
import * as fs from "node:fs/promises";

import type {
  FactoryContext,
  LoomProviderApi,
  Session,
  SessionUpdate,
  TrustedPath,
} from "@mcmaki/loom";

const PROVIDER_NAME = "glass-identity";

// Generic fallback when the manifest doesn't set `[session.identity].vault_path`.
// Real installs almost always override via the manifest (typically using the
// `${GLASS_VAULT_DIR}` substitution — see manifests/glass.toml). Set this to
// something neutral so a bare-bones agent.toml without a vault config still
// boots, even if it can't read soul.md.
const DEFAULT_VAULT_PATH = "~/glass-vault";

interface IdentityConfig {
  vault_path: string;
}

function readIdentityConfig(raw: unknown): IdentityConfig {
  const c = (raw ?? {}) as Record<string, unknown>;
  const v =
    typeof c.vault_path === "string" && c.vault_path.length > 0
      ? c.vault_path
      : DEFAULT_VAULT_PATH;
  return { vault_path: expandTilde(v) };
}

function expandTilde(p: string): string {
  if (p === "~") return os.homedir();
  if (p.startsWith("~/")) return path.join(os.homedir(), p.slice(2));
  return p;
}

async function readFileOrNull(p: string): Promise<string | null> {
  try {
    return await fs.readFile(p, "utf8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw e;
  }
}

class IdentitySession implements Session {
  private readonly soulPath: string;

  constructor(vaultPath: string) {
    this.soulPath = path.join(vaultPath, "_glass", "soul.md");
  }

  async push(event: SessionUpdate): Promise<SessionUpdate[]> {
    return [event];
  }

  async pull(below: SessionUpdate[]): Promise<SessionUpdate[]> {
    return below;
  }

  async systemPromptSection(): Promise<string> {
    const soul = ((await readFileOrNull(this.soulPath)) ?? "").trim();
    const parts: string[] = [];
    if (soul.length > 0) parts.push(`## soul\n\n${soul}`);
    parts.push(`The current time is ${formatNow(new Date())}.`);
    return parts.join("\n\n");
  }

  trustedPaths(): TrustedPath[] {
    return [
      {
        path: this.soulPath,
        access: "read",
        reason:
          "Glass identity document, loaded into the system prompt every turn",
      },
    ];
  }
}

/**
 * Friendly local-time formatting for the system prompt's `current time`
 * line. Includes day-of-week (useful for scheduling), date, 24-hour time,
 * and timezone abbreviation. Uses the host's `Intl` locale data, which
 * follows the orchestrator's system timezone — the same source the Rust
 * side uses for `chrono::Local`, so schedule entries computed by the
 * agent and parsed by the orchestrator agree on what "now" means.
 */
function formatNow(d: Date): string {
  return new Intl.DateTimeFormat("en-US", {
    weekday: "long",
    year: "numeric",
    month: "long",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
    timeZoneName: "short",
  }).format(d);
}

export function register(api: LoomProviderApi): void {
  api.registerSession({
    name: PROVIDER_NAME,
    async create(
      config: Record<string, unknown>,
      _ctx: FactoryContext,
    ): Promise<Session> {
      const { vault_path } = readIdentityConfig(config);
      return new IdentitySession(vault_path);
    },
  });
}
