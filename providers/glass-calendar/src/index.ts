/**
 * @glass/calendar — typed wrapper around the `gcalcli` CLI.
 *
 * Two tools, both shelling out to `gcalcli` via `execFile` (argv-only, no
 * shell). The agent provides structured input; we map it to specific
 * `gcalcli` flags. There is no codepath that lets the agent pass arbitrary
 * shell strings or pick a different binary — the narrower surface is the
 * whole point of writing a typed provider instead of granting `bash` +
 * `gcalcli` on PATH.
 *
 * Auth: gcalcli stores OAuth tokens at `~/.gcalcli_oauth` after a one-time
 * `gcalcli init`. The Loom subprocess inherits HOME, so `gcalcli` finds the
 * tokens without anything from us. If the tokens are missing, gcalcli will
 * try to prompt on stdin; we pipe stdin to /dev/null implicitly (no shell,
 * no PTY) so it fails fast with a clear stderr message.
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";

import type {
  Agent,
  CapabilitySet,
  FactoryContext,
  LoomProviderApi,
  Tool,
  ToolConfig,
  Tools,
  ToolResult,
} from "@mcmaki/loom";

const PROVIDER_NAME = "calendar";
const GCALCLI = "gcalcli";
const TIMEOUT_MS = 30_000;

const execFileP = promisify(execFile);

interface AgendaEvent {
  start: string;
  end: string;
  title: string;
  location?: string;
}

/**
 * Parse `gcalcli agenda --tsv` output. Each non-blank line is one event,
 * tab-separated: `start_date \t start_time \t end_date \t end_time \t
 * title [\t location]`. All-day events have empty time fields. We collapse
 * date+time into a single "YYYY-MM-DD HH:MM" string for the model.
 */
function parseAgendaTsv(stdout: string): AgendaEvent[] {
  const events: AgendaEvent[] = [];
  for (const line of stdout.split("\n")) {
    if (!line.trim()) continue;
    const parts = line.split("\t");
    if (parts.length < 5) continue;
    const startDate = parts[0] ?? "";
    // Skip gcalcli's TSV header row.
    if (startDate === "start_date") continue;
    const startTime = parts[1] ?? "";
    const endDate = parts[2] ?? "";
    const endTime = parts[3] ?? "";
    const title = parts[4] ?? "";
    const location = (parts[5] ?? "").trim() || undefined;
    const start = startTime ? `${startDate} ${startTime}` : startDate;
    const end = endTime ? `${endDate} ${endTime}` : endDate;
    events.push({ start, end, title, ...(location ? { location } : {}) });
  }
  return events;
}

class CalendarListTool implements Tool {
  name = "calendar_list";
  description =
    "List upcoming events from Google Calendar via gcalcli. Defaults to " +
    "today through one week out if no range is given. Returns a JSON array " +
    "of `{start, end, title, location?}` objects. " +
    "`start_when` / `end_when` accept gcalcli's natural-language date " +
    "parser: 'today', 'tomorrow', 'next monday', or YYYY-MM-DD. Use this " +
    "to check what's coming up before scheduling, surface conflicts, or " +
    "reference recent events.";
  inputSchema = {
    type: "object",
    additionalProperties: false,
    properties: {
      start_when: {
        type: "string",
        description:
          "When to start. Natural language or YYYY-MM-DD. Default: today.",
      },
      end_when: {
        type: "string",
        description:
          "When to end. Natural language or YYYY-MM-DD. Default: one week from start.",
      },
    },
  };

  async execute(input: unknown): Promise<ToolResult> {
    const i = (input ?? {}) as { start_when?: string; end_when?: string };
    const args: string[] = ["agenda", "--nocolor", "--tsv"];
    if (i.start_when) args.push(i.start_when);
    if (i.end_when) args.push(i.end_when);
    try {
      const { stdout } = await execFileP(GCALCLI, args, {
        timeout: TIMEOUT_MS,
        maxBuffer: 1024 * 1024,
      });
      const events = parseAgendaTsv(stdout);
      if (events.length === 0) {
        return { content: "(no events in the requested range)" };
      }
      return { content: JSON.stringify(events, null, 2) };
    } catch (e) {
      return {
        content: `calendar_list failed: ${gcalcliErrorMessage(e)}`,
        isError: true,
      };
    }
  }
}

class CalendarAddTool implements Tool {
  name = "calendar_add";
  description =
    "Add an event to Google Calendar via gcalcli. `when` accepts " +
    "natural language ('2pm tomorrow', 'sunday 9am') or ISO format — " +
    "gcalcli does the date parsing, so you can pass what's natural. " +
    "`duration` is in minutes (default 60). Use the optional `calendar` " +
    "field to add to a non-primary calendar by name; omit it to use the " +
    "default. The tool returns gcalcli's confirmation line, which usually " +
    "includes the event link.";
  inputSchema = {
    type: "object",
    required: ["title", "when"],
    additionalProperties: false,
    properties: {
      title: {
        type: "string",
        minLength: 1,
        description: "Event title.",
      },
      when: {
        type: "string",
        minLength: 1,
        description:
          "Start time. Natural language ('2pm tomorrow', 'sunday 9am') " +
          "or ISO format. Local timezone.",
      },
      duration: {
        type: "number",
        minimum: 1,
        description: "Duration in minutes. Default 60.",
      },
      location: {
        type: "string",
        description: "Where the event is. Maps to gcalcli's --where.",
      },
      description: {
        type: "string",
        description: "Long-form notes about the event.",
      },
      calendar: {
        type: "string",
        description: "Calendar name. Omit for the primary calendar.",
      },
    },
  };

  async execute(input: unknown): Promise<ToolResult> {
    const i = input as {
      title?: string;
      when?: string;
      duration?: number;
      location?: string;
      description?: string;
      calendar?: string;
    };
    const title = (i.title ?? "").trim();
    const when = (i.when ?? "").trim();
    if (!title) {
      return { content: "calendar_add: title is required", isError: true };
    }
    if (!when) {
      return { content: "calendar_add: when is required", isError: true };
    }

    const args: string[] = ["add", "--default-reminders"];
    args.push("--title", title);
    args.push("--when", when);
    if (typeof i.duration === "number" && Number.isFinite(i.duration) && i.duration > 0) {
      args.push("--duration", String(Math.round(i.duration)));
    }
    if (i.location) args.push("--where", i.location);
    if (i.description) args.push("--description", i.description);
    if (i.calendar) args.push("--cal", i.calendar);

    try {
      const { stdout } = await execFileP(GCALCLI, args, {
        timeout: TIMEOUT_MS,
        maxBuffer: 1024 * 1024,
      });
      const confirmation = stdout.trim();
      return {
        content: confirmation
          ? `Added: ${title} (${when})\n${confirmation}`
          : `Added: ${title} (${when})`,
      };
    } catch (e) {
      return {
        content: `calendar_add failed: ${gcalcliErrorMessage(e)}`,
        isError: true,
      };
    }
  }
}

/**
 * `execFile` errors come in two shapes:
 *
 *   - `ENOENT`: the binary isn't on PATH. Surface that specifically so
 *     it's obvious the host machine is missing gcalcli, not a transient
 *     auth or parsing issue.
 *   - Non-zero exit: stderr usually has the diagnostic message. Include
 *     it verbatim (trimmed) since gcalcli's errors are already human-
 *     readable ("auth error", "calendar not found", etc.).
 */
function gcalcliErrorMessage(e: unknown): string {
  const err = e as NodeJS.ErrnoException & { stderr?: string; stdout?: string };
  if (err.code === "ENOENT") {
    return (
      "gcalcli is not on PATH. Install with `pip install gcalcli` (or `brew " +
      "install gcalcli`), then run `gcalcli init` once to authorize."
    );
  }
  const stderr = (err.stderr ?? "").toString().trim();
  if (stderr) return stderr;
  return err.message ?? String(e);
}

class CalendarTools implements Tools {
  resolveTool(
    name: string,
    _config: ToolConfig,
    _agent: Agent,
    _capabilities: CapabilitySet | undefined,
  ): Tool | null {
    switch (name) {
      case "calendar_list":
        return new CalendarListTool();
      case "calendar_add":
        return new CalendarAddTool();
      default:
        return null;
    }
  }
}

export function register(api: LoomProviderApi): void {
  api.registerTools({
    name: PROVIDER_NAME,
    async create(
      _config: Record<string, unknown>,
      _ctx: FactoryContext,
    ): Promise<Tools> {
      return new CalendarTools();
    },
  });
}
