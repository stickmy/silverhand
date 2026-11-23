import type {
  ConversationActivityEvent,
  AgentStreamActivityEvent,
  ToolOutcome,
} from "@silverhand/shared";
import { decodeTargetForDisplay, ToolSchema, type Tool } from "@silverhand/shared";

export type ActivityEvent = ConversationActivityEvent | AgentStreamActivityEvent;

// ── path abbreviation ──────────────────────────────────────────────────────

/** Collapse an absolute home path to `~/…` so `Read /Users/x/work/a.ts`
 *  reads as `Read ~/work/a.ts`. Best-effort, macOS + linux home shapes. */
export function abbreviateHome(p: string): string {
  return p.replace(/\/Users\/[^/\s]+\//g, "~/").replace(/\/home\/[^/\s]+\//g, "~/");
}

// ── tool_call presentation ─────────────────────────────────────────────────

export type ToolDetail =
  | { kind: "code"; text: string }
  | { kind: "json"; text: string }
  | { kind: "diff"; removed: string; added: string };

export interface ToolPresentation {
  /** Verb shown bold, e.g. `Bash`, `Read`, `Edit`, `silverhand task status`. */
  verb: string;
  /** Inline argument after the verb (monospace), e.g. `ls ~/work`. */
  arg: string | null;
  /** Optional richer body rendered below the title. */
  detail: ToolDetail | null;
}

function asRecord(input: unknown): Record<string, unknown> | null {
  return input !== null && typeof input === "object" && !Array.isArray(input)
    ? (input as Record<string, unknown>)
    : null;
}

function s(rec: Record<string, unknown> | null, key: string): string | null {
  const v = rec?.[key];
  return typeof v === "string" ? v : null;
}

function jsonDetail(input: unknown): ToolDetail | null {
  if (input === null || input === undefined) return null;
  if (typeof input === "string") {
    return input.length > 0 ? { kind: "code", text: input } : null;
  }
  try {
    const text = JSON.stringify(input, null, 2);
    if (!text || text === "{}" || text === "null" || text === "[]") return null;
    return { kind: "json", text };
  } catch {
    return null;
  }
}

/**
 * Strip a `(/path/)?{zsh,bash,sh} -…c <quoted>` launcher down to its inner
 * command, so a wrapped `silverhand …` is seen at command position. ONLY this
 * exact launcher shape is unwrapped — a quoted *argument* to any other program
 * (`rg "silverhand …"`) is returned unchanged, so it is never mistaken for a
 * launcher.
 *
 * The relaxed part: once the launcher head + opening quote is confirmed, the
 * inner command is everything to the end of the string — the closing quote is
 * dropped only if present. A truncated command (the 500-char broadcast cap can
 * cut mid-heredoc) has no closing quote, but we still unwrap so the inner
 * `silverhand …` is recognised. This relaxation is gated on the launcher head,
 * so it never re-opens the `rg "silverhand …"` false positive.
 */
function unwrapShellLauncher(command: string): string {
  const trimmed = command.trim();
  const m = /^(?:\S*\/)?(?:zsh|bash|sh)\s+-[a-z]*c[a-z]*\s+(['"])([\s\S]*)$/i.exec(trimmed);
  if (!m) return trimmed;
  let inner = m[2]!;
  if (inner.endsWith(m[1]!)) inner = inner.slice(0, -1);
  return inner.trim();
}

/**
 * Quote-aware end of the command starting at `start`: the index of the next
 * UNQUOTED shell separator (`;` `&` `|` `(`), else the string end. Shares the
 * exact separator + quote semantics of {@link findSilverhandCommand}, so a
 * command's `args` never bleed into a following `&& echo --task-number 5`
 * (which would otherwise fake a number / stale flag from the SUFFIX). Newlines
 * are not separators — a heredoc body line is part of the command, matching
 * the scanner; every metadata reader looks only before the body anyway.
 */
function commandEnd(inner: string, start: number): number {
  let quote: string | null = null;
  for (let i = start; i < inner.length; i++) {
    const c = inner[i]!;
    if (quote) {
      if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") {
      quote = c;
      continue;
    }
    if (c === ";" || c === "&" || c === "|" || c === "(") return i;
  }
  return inner.length;
}

/**
 * Scan an (unwrapped) shell command left-to-right, tracking `'` / `"` quote
 * state, and return the first `silverhand <noun> [verb]` that sits at a
 * COMMAND position — the command start or right after an UNQUOTED separator
 * (`;` `&` `|` `(`). A `silverhand` substring inside a quoted argument —
 * `rg "silverhand …"`, `rg "(silverhand …)"`, `echo '; silverhand …'` — is
 * never accepted, so the trace can't fabricate a fake action. Deliberately a
 * small scanner, not a full shell parser: newline is NOT a separator (so a
 * heredoc body line isn't mistaken for a command) and backticks aren't tracked.
 *
 * `args` is the text AFTER the matched `silverhand <noun> [verb]`, up to the
 * next UNQUOTED separator ({@link commandEnd}) — the identified command's OWN
 * operands/flags, nothing from a compound prefix or suffix. All secondary
 * metadata (task number, `--status`, stale suffix) reads `args`, so neither
 * `sleep 5 && silverhand task claim 9` (prefix) nor `silverhand task create &&
 * echo --task-number 5` (suffix) can leak a number/flag into it.
 */
function findSilverhandCommand(
  inner: string,
): { noun: string; verb?: string; args: string } | null {
  let quote: string | null = null;
  let atCmdPos = true;
  for (let i = 0; i < inner.length; i++) {
    const c = inner[i]!;
    if (quote) {
      if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") {
      quote = c;
      atCmdPos = false;
      continue;
    }
    if (c === " " || c === "\t") continue;
    if (c === ";" || c === "&" || c === "|" || c === "(") {
      atCmdPos = true;
      continue;
    }
    if (atCmdPos) {
      const slice = inner.slice(i);
      const m = /^silverhand\s+([a-z][a-z-]*)(?:\s+([a-z][a-z-]*))?/i.exec(slice);
      if (m) {
        const argsStart = m[0].length;
        return {
          noun: m[1]!.toLowerCase(),
          verb: m[2]?.toLowerCase(),
          args: slice.slice(argsStart, commandEnd(slice, argsStart)),
        };
      }
    }
    atCmdPos = false;
  }
  return null;
}

const TASK_NUMBER_VERBS = new Set(["detail", "status", "claim", "unclaim", "edit"]);

/**
 * Extract the task number a `silverhand task <verb> …` action targets, given
 * that command's `args` slice (everything AFTER `task <verb>`, from
 * {@link findSilverhandCommand} — never the wider shell text). Two forms:
 *
 *  - the retired `--task-number N` flag, anywhere in `args`, any verb (history
 *    rows + the surviving `task list --task-number` filter);
 *  - the new positional operand, which — since Activity rows are canonical CLI
 *    invocations — sits as the FIRST token right after the verb (`task claim
 *    42`). Gated to {@link TASK_NUMBER_VERBS}, so `list 5` / a `create` with no
 *    operand never yield a number. A batch claim (`task claim 42 43`) surfaces
 *    the first only.
 *
 * We deliberately do NOT re-implement CLI flag parsing (skipping flag values to
 * hunt a later positional): a non-canonical `task edit --clear-body 42` simply
 * yields no number rather than risk a wrong one — `--clear-body`/`--json` are
 * booleans and telling value-flags from boolean ones here would rebuild the
 * whole parser. Returns null when there is no operand. Pure — branch-tested.
 */
export function taskNumberFromCommand(verb: string | undefined, args: string): string | null {
  const flag = /--task-number[=\s]+(\d+)/.exec(args)?.[1];
  if (flag !== undefined) return flag;
  if (verb === undefined || !TASK_NUMBER_VERBS.has(verb)) return null;
  const first = args.trimStart().split(/\s+/)[0] ?? "";
  return /^\d+$/.test(first) ? first : null;
}

function presentSilverhandCli(command: string): ToolPresentation | null {
  const inner = unwrapShellLauncher(command);
  const found = findSilverhandCommand(inner);
  if (!found) return null;
  const { noun, verb: verbTok, args } = found;
  const cap = noun.charAt(0).toUpperCase() + noun.slice(1);
  let label = verbTok ? `${cap} ${verbTok.replace(/-/g, " ")}` : cap;

  let arg: string | null = null;
  if (noun === "task") {
    const num = taskNumberFromCommand(verbTok, args);
    const status =
      verbTok === "status" ? (/--status[=\s]+([a-z_]+)/.exec(args)?.[1] ?? null) : null;
    if (verbTok === "status" && status) label = "Task update";
    if (num && status) arg = `#${num} → ${status}`;
    else if (num) arg = `#${num}`;
    else if (status) arg = `→ ${status}`;
  } else if (noun === "message" && verbTok === "send") {
    // A send's destination is its one scannable operand — surface the `--target`
    // value after the label so the row reads `Message send → <target>`. The body
    // (heredoc / STDIN) is deliberately omitted: the summary is where it went,
    // not what it said. `--target` is quote- or bare-valued; take the first one.
    const m = /--target[=\s]+(?:'([^']*)'|"([^"]*)"|(\S+))/.exec(args);
    const target = m?.[1] ?? m?.[2] ?? m?.[3] ?? null;
    // Show the readable spelling: `#silverhand/UI%2FUX%20优化` reads as
    // `#silverhand/UI/UX 优化`. Decode is DISPLAY ONLY (`decodeTargetForDisplay`)
    // and the raw token is never used for anything else here.
    if (target) arg = `→ ${decodeTargetForDisplay(target)}`;
  }
  return { verb: label, arg, detail: null };
}

function commandPresentation(verb: string, command: string): ToolPresentation {
  const cli = presentSilverhandCli(command);
  if (cli) return cli;
  const abbr = abbreviateHome(command.trim());
  if (abbr.includes("\n")) return { verb, arg: null, detail: { kind: "code", text: abbr } };
  return { verb, arg: abbr.length > 0 ? abbr : null, detail: null };
}

function withPath(pattern: string, path: string | undefined): string {
  return `${pattern}${path ? ` in ${abbreviateHome(path)}` : ""}`;
}

/** Presentation of a runtime-independent tool description (`payload.tool`). */
export function presentTool(tool: Tool, name: string, input: unknown): ToolPresentation {
  switch (tool.type) {
    case "command":
      return commandPresentation("Bash", tool.command);
    case "read":
      return { verb: "Read", arg: abbreviateHome(tool.path), detail: null };
    case "write":
      return {
        verb: "Write",
        arg: abbreviateHome(tool.path),
        detail:
          tool.content !== undefined ? { kind: "diff", removed: "", added: tool.content } : null,
      };
    case "edit":
      return {
        verb: "Edit",
        arg: `${abbreviateHome(tool.path)}${tool.change ? ` (${tool.change})` : ""}`,
        detail: tool.edits?.length
          ? {
              kind: "diff",
              removed: tool.edits.map((edit) => edit.old).join("\n"),
              added: tool.edits.map((edit) => edit.new).join("\n"),
            }
          : null,
      };
    case "search":
      return {
        verb: "Grep",
        arg: `${withPath(tool.pattern, tool.path)}${tool.glob ? ` (${tool.glob})` : ""}`,
        detail: null,
      };
    case "find":
      return { verb: "Glob", arg: withPath(tool.pattern, tool.path), detail: null };
    case "list":
      return { verb: "List", arg: tool.path ? abbreviateHome(tool.path) : null, detail: null };
    case "web_search":
      return { verb: "Search", arg: tool.query, detail: null };
    case "web_fetch":
      return { verb: "Fetch", arg: tool.url, detail: null };
    case "subagent":
      return { verb: "Task", arg: tool.description, detail: null };
    case "skill":
      return { verb: "Skill", arg: tool.name, detail: null };
    case "mcp":
      return { verb: `${tool.server}.${tool.tool}`, arg: null, detail: jsonDetail(input) };
    case "other":
      return { verb: name, arg: null, detail: jsonDetail(input) };
  }
}

/** A tool_call row: `payload.tool` when the daemon sent one, else the native name table. */
export function presentActivityToolCall(payload: Record<string, unknown> | null): ToolPresentation {
  const name = typeof payload?.["name"] === "string" ? payload["name"] : "tool";
  const input = payload?.["input"] ?? null;
  const tool = ToolSchema.safeParse(payload?.["tool"]);
  return tool.success ? presentTool(tool.data, name, input) : presentToolCall(name, input);
}

/**
 * Legacy rows (and older daemons) carry no `tool`; this name table renders them
 * and is frozen — new runtimes describe their tools through `tool` instead.
 *
 * Map a `(name, input)` tool call to a legible presentation. Known tools get a
 * verb + inline arg (+ optional diff/code body); anything unrecognised or
 * malformed falls back to the raw name + pretty JSON — never a crash.
 *
 * Tool names span runtimes: claude/cursor use Claude-Code names (`Bash`,
 * `Read`, `Edit`, `Write`, `Grep`, `Glob`, `Task`, `Skill`, `WebFetch`,
 * `WebSearch`); codex uses `shell`, `file_change`, `web_search`, `mcp_*`.
 */
export function presentToolCall(name: string, input: unknown): ToolPresentation {
  const rec = asRecord(input);
  const n = name.toLowerCase();

  if (n === "bash" || n === "shell") {
    const verb = n === "shell" ? "Shell" : "Bash";
    const command = s(rec, "command") ?? (typeof input === "string" ? input : null);
    if (command !== null) return commandPresentation(verb, command);
    return { verb, arg: null, detail: jsonDetail(input) };
  }

  if (n === "read") {
    const fp = s(rec, "file_path") ?? s(rec, "path");
    if (fp) return { verb: "Read", arg: abbreviateHome(fp), detail: null };
  }

  if (n === "edit" || n === "multiedit") {
    const fp = s(rec, "file_path") ?? s(rec, "path");
    const oldS = s(rec, "old_string");
    const newS = s(rec, "new_string");
    if (fp && (oldS !== null || newS !== null)) {
      return {
        verb: "Edit",
        arg: abbreviateHome(fp),
        detail: { kind: "diff", removed: oldS ?? "", added: newS ?? "" },
      };
    }
    if (fp) return { verb: "Edit", arg: abbreviateHome(fp), detail: jsonDetail(input) };
  }

  if (n === "write") {
    const fp = s(rec, "file_path") ?? s(rec, "path");
    const content = s(rec, "content");
    if (fp) {
      return {
        verb: "Write",
        arg: abbreviateHome(fp),
        detail: content !== null ? { kind: "diff", removed: "", added: content } : null,
      };
    }
  }

  // codex file_change — `{ path, kind: 'add'|'modify'|'delete' }`
  if (n === "file_change") {
    const p = s(rec, "path");
    const kind = s(rec, "kind");
    if (p) {
      return {
        verb: "Edit",
        arg: `${abbreviateHome(p)}${kind ? ` (${kind})` : ""}`,
        detail: null,
      };
    }
  }

  if (n === "grep") {
    const pattern = s(rec, "pattern");
    const path = s(rec, "path");
    if (pattern !== null) {
      return {
        verb: "Grep",
        arg: `${pattern}${path ? ` in ${abbreviateHome(path)}` : ""}`,
        detail: null,
      };
    }
  }

  if (n === "glob") {
    const pattern = s(rec, "pattern");
    const path = s(rec, "path");
    if (pattern !== null) {
      return {
        verb: "Glob",
        arg: `${pattern}${path ? ` in ${abbreviateHome(path)}` : ""}`,
        detail: null,
      };
    }
  }

  if (n === "task") {
    const desc = s(rec, "description") ?? s(rec, "subagent_type");
    return { verb: "Task", arg: desc, detail: desc ? null : jsonDetail(input) };
  }

  if (n === "skill") {
    const cmd = s(rec, "command") ?? s(rec, "skill");
    return { verb: "Skill", arg: cmd, detail: cmd ? null : jsonDetail(input) };
  }

  if (n === "webfetch") {
    const url = s(rec, "url");
    if (url) return { verb: "Fetch", arg: url, detail: null };
  }

  if (n === "websearch" || n === "web_search") {
    const q = s(rec, "query");
    if (q !== null) return { verb: "Search", arg: q, detail: null };
  }

  // Unknown / MCP / malformed input → raw name + pretty JSON.
  return { verb: name, arg: null, detail: jsonDetail(input) };
}

// ── fragment coalescing (display fallback for codex token streams) ──────────

export type ActivityDisplayItem =
  | {
      type: "group";
      key: string;
      kind: "thinking" | "text";
      text: string;
      representative: ActivityEvent;
      count: number;
    }
  | {
      type: "event";
      key: string;
      event: ActivityEvent;
      /** "This operation finished." Set on a `tool_call` whose matching
       *  `tool_output` was folded in, and on a `compaction_started` whose
       *  matching `compaction_finished` was folded in — both render a
       *  completion marker (✓ / done icon) in place of the in-flight state. */
      completed?: boolean;
      /** The folded `tool_output`'s classified outcome, carried onto a completed
       *  `tool_call` so the row paints the honest marker: only `success` earns a
       *  green ✓; `failed` is an error mark, `cancelled` neutral, and `completed`
       *  (the wire default for old frames / historical rows that never classified)
       *  a neutral "done" — a finished call whose success we cannot prove is
       *  never upgraded to green. Absent while the call is still in flight. */
      outcome?: ToolOutcome;
    };

function textOf(e: ActivityEvent): string {
  const p = e.payload as Record<string, unknown> | null;
  const v = p?.["text"];
  return typeof v === "string" ? v : "";
}

function callIdOf(e: ActivityEvent): string | null {
  const v = (e.payload as Record<string, unknown> | null)?.["tool_call_id"];
  return typeof v === "string" ? v : null;
}

/** A `tool_output`'s classified outcome. Anything missing or unrecognised —
 *  old wire frames and historical rows persisted before the outcome contract —
 *  reads as the neutral `completed`: a finished call is never upgraded to a
 *  provable `success` it did not record. */
function outcomeOf(e: ActivityEvent): ToolOutcome {
  const v = (e.payload as Record<string, unknown> | null)?.["outcome"];
  return v === "success" || v === "failed" || v === "cancelled" ? v : "completed";
}

function sessionIdOf(e: ActivityEvent): string | null {
  const v = (e.payload as Record<string, unknown> | null)?.["session_id"];
  return typeof v === "string" && v.length > 0 ? v : null;
}

export function coalesceActivity(events: ReadonlyArray<ActivityEvent>): ActivityDisplayItem[] {
  // Call ids can recur across runs; only the nearest preceding call owns an outcome.
  const calls = new Map<string, number>();
  const outcomeByRow = new Map<number, ToolOutcome>();
  const foldedOutputs = new Set<number>();
  for (const e of events) {
    const id = callIdOf(e);
    if (id === null) continue;
    if (e.kind === "tool_call") calls.set(id, e.id);
    else if (e.kind === "tool_output") {
      const callRow = calls.get(id);
      if (callRow !== undefined) {
        outcomeByRow.set(callRow, outcomeOf(e));
        foldedOutputs.add(e.id);
      }
    }
  }

  const compactionDoneStartIds = new Set<number>();
  const foldedFinishedIds = new Set<number>();
  {
    const openStarts: number[] = [];
    for (const e of events) {
      if (e.kind === "compaction_started") {
        openStarts.push(e.id);
      } else if (e.kind === "compaction_finished") {
        const startId = openStarts.shift();
        if (startId !== undefined) {
          compactionDoneStartIds.add(startId);
          foldedFinishedIds.add(e.id);
        }
      }
    }
  }

  const out: ActivityDisplayItem[] = [];
  const seenSessionIds = new Set<string>();
  let run: ActivityEvent[] = [];
  let runKind: "thinking" | "text" | null = null;

  const flush = (): void => {
    if (run.length === 0) return;
    const first = run[0]!;
    const text = run.map(textOf).join("");
    // Codex ends a turn with an empty assistant `text` block; it used to render
    // as a bare "(message)" placeholder — pure noise. A whitespace-only text
    // group carries nothing, so drop it. `thinking` is kept even when empty: the
    // "Thinking" label is itself the fact that a reasoning pass happened.
    if (runKind === "text" && text.trim().length === 0) {
      run = [];
      runKind = null;
      return;
    }
    out.push({
      type: "group",
      key: `g${first.id}`,
      kind: runKind!,
      // Thinking renders whitespace-preserved, so a reasoning summary's trailing
      // newlines would show as blank lines. Inner paragraph breaks stay.
      text: runKind === "thinking" ? text.trim() : text,
      representative: first,
      count: run.length,
    });
    run = [];
    runKind = null;
  };

  for (const e of events) {
    if (e.kind === "thinking" || e.kind === "text") {
      if (runKind === e.kind) {
        run.push(e);
      } else {
        flush();
        run = [e];
        runKind = e.kind;
      }
      continue;
    }

    flush();

    if (e.kind === "tool_output") {
      // Folded into its tool_call → drop. Orphan (its call paged out by
      // retention) → light row that still carries the verdict: a failed/cancelled
      // orphan is the only surviving record of that outcome, so it must not
      // render as a neutral success.
      if (foldedOutputs.has(e.id)) continue;
      out.push({ type: "event", key: String(e.id), event: e, outcome: outcomeOf(e) });
      continue;
    }

    if (e.kind === "tool_call") {
      const outcome = outcomeByRow.get(e.id);
      const completed = outcome !== undefined;
      out.push({
        type: "event",
        key: String(e.id),
        event: e,
        completed,
        ...(outcome !== undefined ? { outcome } : {}),
      });
      continue;
    }

    if (e.kind === "compaction_started") {
      // Loading until a finished pairs in, then `completed` flips it to the
      // done marker (the paired finished is dropped below).
      out.push({
        type: "event",
        key: String(e.id),
        event: e,
        completed: compactionDoneStartIds.has(e.id),
      });
      continue;
    }

    if (e.kind === "compaction_finished") {
      // Folded into its started → drop. Orphan → standalone done row.
      if (foldedFinishedIds.has(e.id)) continue;
      out.push({ type: "event", key: String(e.id), event: e });
      continue;
    }

    if (e.kind === "turn_end") {
      // A turn boundary is not a user-facing activity fact — it rendered as a
      // "Turn ended" line that only added noise between real events. The turn's
      // real outcome shows on its tool calls (✓) and assistant text. Drop it.
      continue;
    }

    if (e.kind === "session_init") {
      // Some runtimes re-announce the SAME session on every turn (stream-json
      // emits one per `system/init` frame). Keep only the first row per distinct
      // session_id — a repeat carries no new fact. An id-less init can't be
      // deduped, so it passes through.
      const sid = sessionIdOf(e);
      if (sid !== null) {
        if (seenSessionIds.has(sid)) continue;
        seenSessionIds.add(sid);
      }
      out.push({ type: "event", key: String(e.id), event: e });
      continue;
    }

    out.push({ type: "event", key: String(e.id), event: e });
  }
  flush();
  return out;
}
