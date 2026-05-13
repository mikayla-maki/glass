/**
 * @glass/companion-tools — orchestrator-side scaffolding for Glass.
 *
 * Contributes two tools and one session layer:
 *
 *   - `send_dm`   — post a Discord DM to Mikayla outside the normal reply
 *                   flow (used by the cron agent, which has no implicit
 *                   reply channel, and by the DM agent for "going async"
 *                   beats).
 *   - `schedule`  — register a one-shot or recurring prompt for future
 *                   firing. The orchestrator persists the entry; the
 *                   cron poller fires it.
 *   - `companion` session layer — contributes a `## recent conversation`
 *                   system-prompt section by reading the tail of
 *                   `$GLASS_DM_LOG`. Useful for the cron agent (no Loom
 *                   FileSession history of its own); the DM agent doesn't
 *                   need it (FileSession already carries full conversation
 *                   context).
 *
 * Both tools talk to the Glass orchestrator over a unix socket whose path
 * is delivered as a Loom secret named `GLASS_ORCHESTRATOR_SOCK`. The
 * orchestrator exports that env var when it spawns the loom subprocess,
 * Loom's `EnvSecretsStore` picks it up, and we receive it filtered via
 * `ctx.secrets` (declared in each tool's `secrets.required`). Tools never
 * read `process.env` directly and never inspect agent identity — both are
 * concerns for the orchestrator to own.
 *
 * Wire protocol (newline-delimited JSON):
 *
 *   request:   `{ id, kind: "send_dm" | "schedule", ... }`
 *   response:  `{ id, ok: true, result?: ... }`
 *              `{ id, ok: false, error: "..." }`
 */

import * as net from "node:net";
import { randomUUID } from "node:crypto";

import * as fs from "node:fs/promises";

import type {
  Agent,
  CapabilitySet,
  FactoryContext,
  LoomProviderApi,
  Session,
  SessionUpdate,
  Tool,
  ToolConfig,
  ToolContext,
  Tools,
  ToolResult,
} from "@mcmaki/loom";

const PROVIDER_NAME = "companion";
const SOCKET_SECRET = "GLASS_ORCHESTRATOR_SOCK";
const DM_LOG_SECRET = "GLASS_DM_LOG";
const DEFAULT_RECENT_COUNT = 20;

interface SocketResponse {
  id: string;
  ok: boolean;
  error?: string;
  result?: unknown;
}

async function sendRequest(
  sockPath: string,
  body: Record<string, unknown>,
): Promise<SocketResponse> {
  const id = randomUUID();
  const payload = JSON.stringify({ id, ...body }) + "\n";

  return new Promise((resolve, reject) => {
    const client = net.createConnection(sockPath);
    let buf = "";
    let settled = false;
    const settle = (fn: () => void) => {
      if (settled) return;
      settled = true;
      fn();
      client.end();
    };

    client.on("connect", () => client.write(payload));
    client.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      const nl = buf.indexOf("\n");
      if (nl < 0) return;
      const line = buf.slice(0, nl);
      try {
        const resp = JSON.parse(line) as SocketResponse;
        settle(() => resolve(resp));
      } catch (e) {
        settle(() => reject(e));
      }
    });
    client.on("error", (e) => settle(() => reject(e)));
    client.on("close", () =>
      settle(() =>
        reject(new Error("orchestrator closed socket without responding")),
      ),
    );
  });
}

function resolveSocketPath(ctx: ToolContext): string {
  const path = ctx.secrets[SOCKET_SECRET];
  if (!path) {
    throw new Error(
      `${SOCKET_SECRET} secret was not provided; the Glass companion tools ` +
        "only run under the Glass orchestrator (not bare `loom prompt`).",
    );
  }
  return path;
}

class SendDmTool implements Tool {
  name = "send_dm";
  description =
    "Send a Discord DM to Mikayla outside the normal reply flow. Use this " +
    "from a scheduled wake-up (the cron agent has no implicit reply " +
    "channel) or when you want to announce 'going async, will follow up' " +
    "mid-conversation. The orchestrator coalesces rapid-fire calls into " +
    "one message so you don't spam Discord — it's still best to write one " +
    "clear message rather than several fragments.";
  inputSchema = {
    type: "object",
    required: ["content"],
    additionalProperties: false,
    properties: {
      content: {
        type: "string",
        description:
          "The message to send. Plain prose; Discord markdown is fine.",
        minLength: 1,
      },
    },
  };
  secrets = { required: [SOCKET_SECRET] };

  async execute(input: unknown, ctx: ToolContext): Promise<ToolResult> {
    const content = (input as { content?: string }).content?.trim() ?? "";
    if (!content) {
      return { content: "send_dm: content is required", isError: true };
    }
    let sock: string;
    try {
      sock = resolveSocketPath(ctx);
    } catch (e) {
      return { content: (e as Error).message, isError: true };
    }
    try {
      const resp = await sendRequest(sock, { kind: "send_dm", content });
      if (resp.ok) return { content: "DM queued" };
      return {
        content: `send_dm failed: ${resp.error ?? "(unknown error)"}`,
        isError: true,
      };
    } catch (e) {
      return {
        content: `send_dm transport error: ${(e as Error).message}`,
        isError: true,
      };
    }
  }
}

class ScheduleTool implements Tool {
  name = "schedule";
  description =
    "Schedule a future prompt for yourself. Use `when` for a one-shot in " +
    "your local timezone — `HH:MM` fires at the next occurrence within the " +
    "next 24 hours (e.g. `15:30` today if it's morning, tomorrow if it's " +
    "already evening), or `YYYY-MM-DD HH:MM` for a specific date and time. " +
    "Use `cron` for a recurring schedule (standard 5-field expression, " +
    "interpreted in local timezone). When the time comes, the orchestrator " +
    "fires the cron agent with `what` as its prompt. The cron agent has " +
    "your identity (soul.md) and recent DM history loaded, but no full " +
    "conversation context — write `what` as if leaving yourself a note.";
  inputSchema = {
    type: "object",
    required: ["what"],
    additionalProperties: false,
    properties: {
      what: {
        type: "string",
        description:
          "Prompt to send to your future self. Be specific — the cron " +
          "agent won't see the conversation where you scheduled this.",
        minLength: 1,
      },
      when: {
        type: "string",
        description:
          "Local-time one-shot. `HH:MM` for the next occurrence within " +
          "24 hours, or `YYYY-MM-DD HH:MM` for a specific moment. Mutually " +
          "exclusive with `cron`.",
      },
      cron: {
        type: "string",
        description:
          "5-field cron expression (minute hour dom month dow), local " +
          "timezone. Mutually exclusive with `when`.",
      },
    },
  };
  secrets = { required: [SOCKET_SECRET] };

  async execute(input: unknown, ctx: ToolContext): Promise<ToolResult> {
    const i = input as { what?: string; when?: string; cron?: string };
    const what = i.what?.trim() ?? "";
    if (!what) {
      return { content: "schedule: 'what' is required", isError: true };
    }
    if (!i.when && !i.cron) {
      return {
        content: "schedule: must specify either 'when' or 'cron'",
        isError: true,
      };
    }
    if (i.when && i.cron) {
      return {
        content: "schedule: 'when' and 'cron' are mutually exclusive",
        isError: true,
      };
    }
    let sock: string;
    try {
      sock = resolveSocketPath(ctx);
    } catch (e) {
      return { content: (e as Error).message, isError: true };
    }
    try {
      const resp = await sendRequest(sock, {
        kind: "schedule",
        what,
        when: i.when ?? null,
        cron: i.cron ?? null,
      });
      if (resp.ok) {
        const r = (resp.result ?? {}) as { id?: string };
        return {
          content: r.id ? `Scheduled (id: ${r.id})` : "Scheduled",
        };
      }
      return {
        content: `schedule failed: ${resp.error ?? "(unknown error)"}`,
        isError: true,
      };
    } catch (e) {
      return {
        content: `schedule transport error: ${(e as Error).message}`,
        isError: true,
      };
    }
  }
}

class CompanionTools implements Tools {
  resolveTool(
    name: string,
    _config: ToolConfig,
    _agent: Agent,
    _capabilities: CapabilitySet | undefined,
  ): Tool | null {
    switch (name) {
      case "send_dm":
        return new SendDmTool();
      case "schedule":
        return new ScheduleTool();
      default:
        return null;
    }
  }
}

// ─── Session: `## recent conversation` ─────────────────────────────────────

interface DmEntry {
  at: string;
  direction: "in" | "out";
  content: string;
}

async function readRecentDms(path: string, count: number): Promise<DmEntry[]> {
  let raw: string;
  try {
    raw = await fs.readFile(path, "utf8");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw e;
  }
  const lines = raw.split("\n").filter((l) => l.trim().length > 0);
  const tail = lines.slice(-count);
  const entries: DmEntry[] = [];
  for (const line of tail) {
    try {
      const e = JSON.parse(line) as DmEntry;
      // Defensive: tolerate older log entries that may have extra fields,
      // but reject entries missing the ones we depend on.
      if (
        typeof e.at === "string" &&
        (e.direction === "in" || e.direction === "out") &&
        typeof e.content === "string"
      ) {
        entries.push(e);
      }
    } catch {
      // skip malformed lines
    }
  }
  return entries;
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : `${n}`;
}

function formatLogTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return (
    `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ` +
    `${pad2(d.getHours())}:${pad2(d.getMinutes())}`
  );
}

function formatEntry(e: DmEntry): string {
  const speaker = e.direction === "in" ? "Mikayla" : "me";
  const time = formatLogTime(e.at);
  const lines = e.content.split("\n");
  const first = lines[0] ?? "";
  const rest = lines.slice(1).map((l) => `  ${l}`);
  return [`[${time}] ${speaker}: ${first}`, ...rest].join("\n");
}

export function formatRecentConversation(entries: DmEntry[]): string {
  if (entries.length === 0) {
    return "## recent conversation\n\n(no recent DMs)";
  }
  const body = entries.map(formatEntry).join("\n");
  return `## recent conversation\n\n${body}`;
}

class CompanionSession implements Session {
  constructor(
    private readonly dmLogPath: string,
    private readonly recentCount: number,
  ) {}

  async push(event: SessionUpdate): Promise<SessionUpdate[]> {
    return [event];
  }

  async pull(below: SessionUpdate[]): Promise<SessionUpdate[]> {
    return below;
  }

  async systemPromptSection(): Promise<string> {
    const entries = await readRecentDms(this.dmLogPath, this.recentCount);
    return formatRecentConversation(entries);
  }
}

interface CompanionSessionConfig {
  recent_count?: number;
}

function readCompanionConfig(raw: unknown): { recentCount: number } {
  const c = (raw ?? {}) as CompanionSessionConfig;
  const n =
    typeof c.recent_count === "number" && Number.isFinite(c.recent_count) && c.recent_count > 0
      ? Math.floor(c.recent_count)
      : DEFAULT_RECENT_COUNT;
  return { recentCount: n };
}

// ─── Registration ──────────────────────────────────────────────────────────

export function register(api: LoomProviderApi): void {
  api.registerTools({
    name: PROVIDER_NAME,
    async create(
      _config: Record<string, unknown>,
      _ctx: FactoryContext,
    ): Promise<Tools> {
      return new CompanionTools();
    },
  });

  api.registerSession({
    name: PROVIDER_NAME,
    secrets: { required: [DM_LOG_SECRET] },
    async create(
      config: Record<string, unknown>,
      _ctx: FactoryContext,
      secrets: Record<string, string>,
    ): Promise<Session> {
      const dmLogPath = secrets[DM_LOG_SECRET];
      if (!dmLogPath) {
        throw new Error(
          `${DM_LOG_SECRET} secret was not provided; companion session needs ` +
            "the orchestrator-managed dm-log path.",
        );
      }
      const { recentCount } = readCompanionConfig(config);
      return new CompanionSession(dmLogPath, recentCount);
    },
  });
}
