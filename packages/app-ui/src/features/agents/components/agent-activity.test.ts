import { describe, expect, it } from "vitest";
import type {
  AgentRuntimeParticipation,
  AgentRuntimeSeatDto,
  ConversationActivityEvent,
  ConversationAgentStatus,
} from "@silverhand/shared";
import { partitionRoster } from "./agent-activity-pane";
import { mergeActivityById } from "./agent-activity-timeline";
import {
  coalesceActivity,
  presentActivityToolCall,
  presentToolCall,
  taskNumberFromCommand,
} from "../model/activity-presentation";

function conv(overrides: Partial<AgentRuntimeParticipation> = {}): AgentRuntimeParticipation {
  return {
    conversation_id: "c-1",
    workspace_id: "w-1",
    workspace_name: "silverhand",
    conversation_placement_kind: "workspace",
    conversation_roster_policy: "open",
    channel_id: "c-1",
    channel_title: "general",
    closed_at: null,
    ...overrides,
  };
}

function seat(conversationId: string, status: ConversationAgentStatus): AgentRuntimeSeatDto {
  return {
    scope: { kind: "conversation", conversation_id: conversationId },
    generation: 1,
    status,
    pid: null,
    cli_session_id: null,
    focus: { kind: "conversation", conversation_id: conversationId },
    queue_depth: 0,
  };
}

function ev(
  id: number,
  overrides: Partial<ConversationActivityEvent> = {},
): ConversationActivityEvent {
  return {
    id,
    conversation_id: "c-1",
    agent_id: "a-1",
    kind: "text",
    payload: null,
    created_at: id * 1000,
    ...overrides,
  };
}

describe("partitionRoster", () => {
  it("keeps every authoritative placement arm", () => {
    const rows = [
      conv({ conversation_id: "ch", conversation_roster_policy: "open" }),
      conv({ conversation_id: "dm", conversation_roster_policy: "fixed" }),
    ];
    const { active, recent } = partitionRoster(rows, []);
    const ids = [...active, ...recent].map((r) => r.conversation_id);
    expect(ids).toEqual(["ch", "dm"]);
  });

  it("splits active (running, not closed) from recent", () => {
    const rows = [
      conv({ conversation_id: "a" }),
      conv({ conversation_id: "s" }),
      conv({ conversation_id: "i" }),
      conv({ conversation_id: "cr" }),
      conv({ conversation_id: "cl", closed_at: 123 }),
    ];
    const seats = [
      seat("a", "active"),
      seat("s", "starting"),
      seat("i", "idle"),
      seat("cr", "crashed"),
      seat("cl", "active"),
    ];
    const { active, recent } = partitionRoster(rows, seats);
    expect(active.map((r) => r.conversation_id)).toEqual(["a", "s"]);
    // closed wins over status: an `active`-status but closed row is recent.
    expect(recent.map((r) => r.conversation_id)).toEqual(["i", "cr", "cl"]);
  });
});

describe("mergeActivityById", () => {
  it("sorts ascending by id and dedupes (backfill/live race)", () => {
    const existing = [ev(2), ev(3)];
    const incoming = [ev(1), ev(3)]; // id 3 already present
    const merged = mergeActivityById(existing, incoming);
    expect(merged.map((e) => e.id)).toEqual([1, 2, 3]);
  });

  it("orders by id even when created_at ties", () => {
    const a = ev(10, { created_at: 5000 });
    const b = ev(11, { created_at: 5000 });
    const merged = mergeActivityById([b], [a]);
    expect(merged.map((e) => e.id)).toEqual([10, 11]);
  });

  it("keeps the longer text on a same-id collision (no short-clobbers-long)", () => {
    // update-in-place backend grows a block; a reordered shorter frame must
    // not regress the already-longer one.
    const long = ev(5, { kind: "thinking", payload: { text: "abcdef" } });
    const short = ev(5, { kind: "thinking", payload: { text: "abc" } });
    expect(mergeActivityById([long], [short])[0]!.payload).toEqual({ text: "abcdef" });
    expect(mergeActivityById([short], [long])[0]!.payload).toEqual({ text: "abcdef" });
  });
});

describe("presentActivityToolCall", () => {
  it("renders payload.tool regardless of the native tool name", () => {
    expect(
      presentActivityToolCall({
        name: "edit",
        input: { path: "/Users/shiki/a.ts", edits: [{ oldText: "a", newText: "b" }] },
        tool: { type: "edit", path: "/Users/shiki/a.ts", edits: [{ old: "a", new: "b" }] },
      }),
    ).toEqual({ verb: "Edit", arg: "~/a.ts", detail: { kind: "diff", removed: "a", added: "b" } });
    expect(
      presentActivityToolCall({
        name: "echo hi",
        input: { CommandLine: "silverhand task claim 42" },
        tool: { type: "command", command: "silverhand task claim 42" },
      }).arg,
    ).toBe("#42");
  });

  it("falls back to the name table without a valid tool", () => {
    expect(presentActivityToolCall({ name: "Bash", input: { command: "ls" } })).toEqual({
      verb: "Bash",
      arg: "ls",
      detail: null,
    });
    expect(
      presentActivityToolCall({ name: "Bash", input: { command: "ls" }, tool: { type: "future" } })
        .arg,
    ).toBe("ls");
  });

  it("keeps the native name and input for other tools", () => {
    expect(
      presentActivityToolCall({ name: "todowrite", input: { todos: [1] }, tool: { type: "other" } })
        .verb,
    ).toBe("todowrite");
  });
});

describe("presentToolCall", () => {
  it("Bash → verb + command, abbreviating $HOME", () => {
    expect(presentToolCall("Bash", { command: "ls ~/work" })).toEqual({
      verb: "Bash",
      arg: "ls ~/work",
      detail: null,
    });
    expect(presentToolCall("Read", { file_path: "/Users/shiki/work/home.ts" })).toEqual({
      verb: "Read",
      arg: "~/work/home.ts",
      detail: null,
    });
  });

  it("Edit → diff detail from old/new", () => {
    const p = presentToolCall("Edit", {
      file_path: "/x/a.ts",
      old_string: "a",
      new_string: "b",
    });
    expect(p.verb).toBe("Edit");
    expect(p.arg).toBe("/x/a.ts");
    expect(p.detail).toEqual({ kind: "diff", removed: "a", added: "b" });
  });

  it('Grep → "pattern in path"', () => {
    expect(presentToolCall("Grep", { pattern: "foo", path: "/Users/shiki/src" }).arg).toBe(
      "foo in ~/src",
    );
  });

  it("recognises a silverhand CLI call as a product action (not raw Bash), incl. shell-wrapped", () => {
    // A `task status` call is recognised as an action (NOT surfaced as raw Bash),
    // and carries id + status change as arg metadata. The arg is extracted data,
    // not copy — kept exact; the verb label face is not pinned.
    const taskStatus = presentToolCall("Bash", {
      command: "silverhand task status --task-number 1187 --status in_review",
    });
    expect(taskStatus.verb).not.toBe("Bash");
    expect(taskStatus.arg).toBe("#1187 → in_review");
    expect(taskStatus.detail).toBeNull();
    // recognised even when wrapped in a `zsh -lc "…"` launcher (heredoc dropped)
    const wrapped = presentToolCall("Bash", {
      command: `/bin/zsh -lc "silverhand message send <<'SILVERHAND_EOF'\nhi\nSILVERHAND_EOF"`,
    });
    expect(wrapped.verb).not.toBe("Bash");
    expect(wrapped.arg).toBeNull();
    // …and even when the broadcast cap truncated the launcher mid-heredoc, so
    // the closing quote is gone (relaxed unwrap is gated on the launcher head).
    const truncated = presentToolCall("Bash", {
      command: `/bin/zsh -lc "silverhand message send <<'SILVERHAND_EOF'\n@Norman body body body...<truncated 456 chars>`,
    });
    // plain / wrapped / truncated sends all resolve to the same action.
    expect(truncated.verb).toBe(wrapped.verb);
    // #task:2099: freshness flags are NOT a staleness oracle. This reader used
    // to promote `--stale-resolution` / `--anyway` to a distinct "stale" action;
    // that guess was wrong three ways (a flagless retry after digesting the
    // conflict looks plain, a pre-emptive `--anyway` false-positives with no 409
    // at all, and a runtime whose tool_call capture carries no command — e.g.
    // OpenCode via ACP — shows nothing). The authoritative fact now arrives as
    // its own `message_stale` activity row. So EVERY spelling of a send is the
    // same action here: the flags describe the request, not its outcome.
    const staleResolution = presentToolCall("Bash", {
      command: "silverhand message send --stale-resolution unaffected",
    });
    expect(staleResolution.verb).toBe(wrapped.verb);
    const anyway = presentToolCall("Bash", {
      command: "silverhand message send --anyway",
    });
    expect(anyway.verb).toBe(wrapped.verb);
    const suffixSend = presentToolCall("Bash", {
      command: "silverhand message send && echo --anyway",
    });
    expect(suffixSend.verb).toBe(wrapped.verb);
    // The real CLI verbs for digesting a conflict stay ordinary actions — they
    // are commands the agent ran, not evidence a refusal happened.
    expect(presentToolCall("Bash", { command: "silverhand message stale-receive" }).verb).not.toBe(
      "Bash",
    );
  });

  it("message send surfaces its --target as the operand, decoded, never the body", () => {
    // Quoted target after the label: `Message send → <target>`.
    const quoted = presentToolCall("Bash", {
      command: "silverhand message send --target '#silverhand/ui polish'",
    });
    expect(quoted.arg).toBe("→ #silverhand/ui polish");
    // `--target=value` form.
    expect(
      presentToolCall("Bash", { command: "silverhand message send --target=dm:@shiki" }).arg,
    ).toBe("→ dm:@shiki");
    // The encoded name form is shown readably: `%2F` is an in-segment slash and
    // `%20` a space, while the structural `/` between segments stays a separator.
    expect(
      presentToolCall("Bash", {
        command: "silverhand message send --target '#silverhand/UI%2FUX%20优化'",
      }).arg,
    ).toBe("→ #silverhand/UI/UX 优化");
    // A non-canonical escape (lowercase hex) is not decoded — the raw token shows
    // exactly as sent rather than a best-effort guess.
    expect(
      presentToolCall("Bash", {
        command: "silverhand message send --target '#silverhand/a%2fb'",
      }).arg,
    ).toBe("→ #silverhand/a%2fb");
    // The heredoc/STDIN body is never mixed into the summary — only the target.
    const withBody = presentToolCall("Bash", {
      command: "silverhand message send --target '#conv:abc' <<'EOF'\nhello --target sneaky\nEOF",
    });
    expect(withBody.arg).toBe("→ #conv:abc");
    // No target → the label stands alone, no fabricated arrow.
    expect(presentToolCall("Bash", { command: "silverhand message send" }).arg).toBeNull();
  });

  it("task actions surface task id + status change; never fabricate an id", () => {
    const claim = presentToolCall("Bash", { command: "silverhand task claim --task-number 9" });
    expect(claim.verb).not.toBe("Bash");
    expect(claim.arg).toBe("#9");
    expect(claim.detail).toBeNull();
    const update = presentToolCall("Bash", {
      command: "silverhand task status --task-number 42 --status done",
    });
    expect(update.arg).toBe("#42 → done");
    // claim and status-update are distinguishable actions.
    expect(update.verb).not.toBe(claim.verb);
    // promote-and-claim by message id → no task number, so no fabricated `#…`
    const byMessage = presentToolCall("Bash", {
      command: "silverhand task claim --message-id abc123",
    });
    expect(byMessage.verb).not.toBe("Bash");
    expect(byMessage.arg).toBeNull();
    // #task:1945 CLI migration: point verbs take the number POSITIONALLY. The
    // reader recognises the new form identically to the retired flag, since
    // activity rows are immutable historical command text (both forms coexist).
    expect(presentToolCall("Bash", { command: "silverhand task claim 9" }).arg).toBe("#9");
    expect(
      presentToolCall("Bash", { command: "silverhand task status 42 --status done" }).arg,
    ).toBe("#42 → done");
    // create takes a source-message, never a task-number operand → no `#…`
    const create = presentToolCall("Bash", {
      command: "silverhand task create --source-message-id abc --claim",
    });
    expect(create.verb).not.toBe("Bash");
    expect(create.arg).toBeNull();
    // MUST NOT leak a number from a compound prefix — only the identified
    // `silverhand task …` command's own args are read, not the wider shell
    // text (#task:1945, Cindy: `sleep 5 && …` must show #9, never #5).
    expect(presentToolCall("Bash", { command: "sleep 5 && silverhand task claim 9" }).arg).toBe(
      "#9",
    );
    // …nor from a compound SUFFIX: args stop at the next unquoted separator, so
    // a trailing `&& echo --task-number 5` can't fake `#5` on a create.
    const createSuffix = presentToolCall("Bash", {
      command: "silverhand task create && echo --task-number 5",
    });
    expect(createSuffix.verb).not.toBe("Bash");
    expect(createSuffix.arg).toBeNull();
  });

  it("reads task status as an update only when it writes a status (#task:2149)", () => {
    // A `--status` write reads as "Task update"; a bare `task status` (a help/read,
    // e.g. `--help`) is not a mutation and stays "Task status" rather than posing
    // as one. Outcome, not verb spelling, still carries success/failure.
    expect(
      presentToolCall("Bash", { command: "silverhand task status 42 --status done" }).verb,
    ).toBe("Task update");
    const help = presentToolCall("Bash", { command: "silverhand task status --help" });
    expect(help.verb).toBe("Task status");
    expect(help.arg).toBeNull();
  });

  it('does NOT mislabel a quoted "silverhand …" argument as a real action', () => {
    // silverhand only counts at command position — a search/echo argument that
    // merely contains the string must stay Bash/Shell (no fabricated action).
    expect(presentToolCall("Bash", { command: 'rg "silverhand message send"' }).verb).toBe("Bash");
    expect(presentToolCall("Bash", { command: "echo 'silverhand task status'" }).verb).toBe("Bash");
    expect(presentToolCall("shell", { command: "grep -r silverhand src/" }).verb).toBe("Shell");
    // separators INSIDE a quoted argument must not count as command boundaries
    // (the scanner tracks quote state, not just the char class).
    expect(presentToolCall("Bash", { command: 'rg "(silverhand message send)"' }).verb).toBe(
      "Bash",
    );
    expect(presentToolCall("Bash", { command: "echo '; silverhand task status'" }).verb).toBe(
      "Bash",
    );
    // but a real call after an UNQUOTED separator IS recognised (not raw Bash)
    expect(presentToolCall("Bash", { command: "cd /x && silverhand task claim" }).verb).not.toBe(
      "Bash",
    );
  });

  it("codex shell labels as Shell; unknown tool falls back to JSON", () => {
    expect(presentToolCall("shell", { command: "echo hi" }).verb).toBe("Shell");
    const fb = presentToolCall("MysteryTool", { foo: "bar" });
    expect(fb.verb).toBe("MysteryTool");
    expect(fb.detail?.kind).toBe("json");
  });
});

describe("taskNumberFromCommand", () => {
  // Behaviour-determining branches (flag ∪ canonical positional, verb
  // whitelist, first of a batch) live in a pure exported fn so a later
  // "simplify back to one regex" can't silently drop the positional form. The
  // arg is the command's `args` slice (after `task <verb>`), never the wider
  // shell text — compound-prefix leakage is covered end-to-end via presentToolCall.
  it("reads the retired --task-number flag for any verb (history compat)", () => {
    expect(taskNumberFromCommand("claim", "--task-number 9")).toBe("9");
    expect(taskNumberFromCommand("status", "--task-number=42 --status done")).toBe("42");
    // list keeps --task-number as a filter → still surfaced via the flag form.
    expect(taskNumberFromCommand("list", "--task-number 5")).toBe("5");
  });

  it("reads the new positional operand only for by-number verbs", () => {
    expect(taskNumberFromCommand("claim", "9")).toBe("9");
    expect(taskNumberFromCommand("detail", "1187")).toBe("1187");
    expect(taskNumberFromCommand("status", "42 --status done")).toBe("42");
    // non-by-number verbs never take a positional operand.
    expect(taskNumberFromCommand("list", "5")).toBeNull();
    expect(taskNumberFromCommand("create", "--source-message-id abc --claim")).toBeNull();
    expect(taskNumberFromCommand(undefined, "5")).toBeNull();
  });

  it("shows the first of a batch; relies on canonical operand-first shape", () => {
    expect(taskNumberFromCommand("claim", "42 43")).toBe("42");
    // no operand at all (message-id claim) → null, never fabricated.
    expect(taskNumberFromCommand("claim", "--message-id abc123")).toBeNull();
    // NON-canonical flags-before-operand yields null rather than a wrong guess:
    // we don't rebuild CLI flag parsing (`--clear-body`/`--json` are booleans).
    expect(taskNumberFromCommand("edit", "--clear-body 42")).toBeNull();
  });
});

describe("coalesceActivity", () => {
  function tev(id: number, kind: "thinking" | "text", text: string) {
    return ev(id, { kind, payload: { text } });
  }

  it("folds consecutive same-kind fragments, joining with empty string", () => {
    const items = coalesceActivity([tev(1, "thinking", "fin"), tev(2, "thinking", "ding")]);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ type: "group", kind: "thinking", text: "finding", count: 2 });
  });

  it("trims a thinking group's outer whitespace but keeps its paragraph breaks", () => {
    const items = coalesceActivity([
      tev(1, "thinking", "\nfirst\n\nsec"),
      tev(2, "thinking", "ond\n\n"),
    ]);
    expect(items[0]).toMatchObject({ kind: "thinking", text: "first\n\nsecond" });
  });

  it("breaks a run on a non-fragment event and on a thinking↔text switch", () => {
    const items = coalesceActivity([
      tev(1, "thinking", "a"),
      ev(2, { kind: "tool_call", payload: { name: "Bash" } }),
      tev(3, "thinking", "b"),
      tev(4, "text", "c"),
    ]);
    expect(items.map((i) => (i.type === "group" ? `g:${i.kind}` : `e:${i.event.kind}`))).toEqual([
      "g:thinking",
      "e:tool_call",
      "g:thinking",
      "g:text",
    ]);
  });

  it("folds a tool_output into its tool_call (✓ marker) by tool_call_id", () => {
    const items = coalesceActivity([
      ev(1, { kind: "tool_call", payload: { name: "Bash", tool_call_id: "tc1", input: {} } }),
      ev(2, { kind: "tool_output", payload: { name: "Bash", tool_call_id: "tc1" } }),
    ]);
    expect(items).toHaveLength(1);
    const item = items[0]!;
    expect(item).toMatchObject({ type: "event", completed: true });
    expect(item.type === "event" && item.event.kind).toBe("tool_call");
  });

  it("pairs reused call ids by preceding row without completing a later running call", () => {
    const call = (id: number) =>
      ev(id, { kind: "tool_call", payload: { name: "Bash", tool_call_id: "repeat" } });
    const result = (id: number, outcome: string) =>
      ev(id, { kind: "tool_output", payload: { tool_call_id: "repeat", outcome } });
    const items = coalesceActivity([
      call(1),
      result(2, "failed"),
      call(3),
      result(4, "success"),
      call(5),
    ]);
    expect(items).toHaveLength(3);
    expect(items[0]).toMatchObject({ key: "1", completed: true, outcome: "failed" });
    expect(items[1]).toMatchObject({ key: "3", completed: true, outcome: "success" });
    expect(items[2]).toMatchObject({ key: "5", completed: false });
    expect(items[2]).not.toHaveProperty("outcome");
    for (const item of items) expect(item).not.toHaveProperty("output");
    const orphan = coalesceActivity([result(1, "cancelled"), call(2)]);
    expect(orphan[0]).toMatchObject({ key: "1", outcome: "cancelled" });
    expect(orphan[1]).toMatchObject({ key: "2", completed: false });
  });

  it("keeps an orphan tool_output (no matching call) as a single light row", () => {
    const items = coalesceActivity([ev(1, { kind: "tool_output", payload: { name: "X" } })]);
    expect(items).toHaveLength(1);
    const item = items[0]!;
    expect(item).toMatchObject({ type: "event" });
    expect(item.type === "event" && item.event.kind).toBe("tool_output");
  });

  it("carries the normalized outcome onto an orphan tool_output (4-cell matrix)", () => {
    // A retention/pagination orphan is the ONLY surviving record of the call's
    // verdict, so each outcome must ride through honestly — a failed orphan must
    // never collapse to a neutral success. Unknown/missing degrades to completed.
    const cell = (payload: Record<string, unknown>) => {
      const items = coalesceActivity([ev(1, { kind: "tool_output", payload })]);
      const item = items[0]!;
      return item.type === "event" ? item.outcome : undefined;
    };
    expect(cell({ name: "shell", outcome: "success" })).toBe("success");
    expect(cell({ name: "shell", outcome: "failed" })).toBe("failed");
    expect(cell({ name: "shell", outcome: "cancelled" })).toBe("cancelled");
    expect(cell({ name: "shell", outcome: "completed" })).toBe("completed");
    // no outcome on the row (historical no-id Codex output) → neutral completed
    expect(cell({ name: "shell" })).toBe("completed");
  });

  it("folds compaction_finished into its compaction_started (loading → done)", () => {
    const items = coalesceActivity([
      ev(1, { kind: "compaction_started" }),
      ev(2, { kind: "compaction_finished" }),
    ]);
    expect(items).toHaveLength(1);
    const item = items[0]!;
    // the started row carries the completion flag (rendered as the done marker)
    expect(item).toMatchObject({ type: "event", completed: true });
    expect(item.type === "event" && item.event.kind).toBe("compaction_started");
  });

  it("leaves an orphan compaction_started as a loading row (no fabricated done)", () => {
    const items = coalesceActivity([ev(1, { kind: "compaction_started" })]);
    expect(items).toHaveLength(1);
    const item = items[0]!;
    expect(item.type === "event" && item.event.kind).toBe("compaction_started");
    expect(item.type === "event" && item.completed).toBeFalsy();
  });

  it("keeps an orphan compaction_finished as a standalone done row", () => {
    const items = coalesceActivity([ev(1, { kind: "compaction_finished" })]);
    expect(items).toHaveLength(1);
    const item = items[0]!;
    expect(item.type === "event" && item.event.kind).toBe("compaction_finished");
  });

  it("pairs multiple compactions FIFO (each finished closes the earliest open start)", () => {
    const items = coalesceActivity([
      ev(1, { kind: "compaction_started" }),
      ev(2, { kind: "compaction_finished" }),
      ev(3, { kind: "compaction_started" }),
      ev(4, { kind: "compaction_finished" }),
    ]);
    // both finishes fold into their starts → two done rows, no leftover finishes
    expect(items).toHaveLength(2);
    expect(
      items.map((i) =>
        i.type === "event" ? `${i.event.kind}:${i.completed ? "done" : "loading"}` : "group",
      ),
    ).toEqual(["compaction_started:done", "compaction_started:done"]);
  });

  it('drops a whitespace-only text group (codex empty end-of-turn "(message)")', () => {
    const items = coalesceActivity([tev(1, "text", "  "), tev(2, "text", "\n")]);
    expect(items).toHaveLength(0);
  });

  it("keeps a text group with real content and a thinking group even when empty", () => {
    const items = coalesceActivity([tev(1, "text", "hello"), tev(2, "thinking", "  ")]);
    // the "Thinking" label is itself a fact; only empty TEXT is noise.
    expect(items.map((i) => (i.type === "group" ? `g:${i.kind}` : "e"))).toEqual([
      "g:text",
      "g:thinking",
    ]);
  });

  it("drops a turn_end row (a turn boundary is not a user-facing fact)", () => {
    const items = coalesceActivity([tev(1, "text", "done"), ev(2, { kind: "turn_end" })]);
    expect(items.map((i) => (i.type === "group" ? `g:${i.kind}` : `e:${i.event.kind}`))).toEqual([
      "g:text",
    ]);
  });

  it("carries the tool_output outcome onto the completed tool_call", () => {
    const items = coalesceActivity([
      ev(1, { kind: "tool_call", payload: { name: "Bash", tool_call_id: "tc1" } }),
      ev(2, {
        kind: "tool_output",
        payload: { name: "Bash", tool_call_id: "tc1", outcome: "failed" },
      }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ type: "event", completed: true, outcome: "failed" });
  });

  it("defaults a completed tool_call to neutral outcome when the output never classified", () => {
    const items = coalesceActivity([
      ev(1, { kind: "tool_call", payload: { name: "Bash", tool_call_id: "tc1" } }),
      ev(2, { kind: "tool_output", payload: { name: "Bash", tool_call_id: "tc1" } }),
    ]);
    // Old wire / historical rows never carry `outcome` → neutral `completed`,
    // never upgraded to a green `success`.
    expect(items[0]).toMatchObject({ type: "event", completed: true, outcome: "completed" });
  });

  it("collapses repeated session_init for the same session_id, keeps distinct ones", () => {
    const items = coalesceActivity([
      ev(1, { kind: "session_init", payload: { session_id: "s1" } }),
      ev(2, { kind: "session_init", payload: { session_id: "s1" } }),
      ev(3, { kind: "session_init", payload: { session_id: "s2" } }),
    ]);
    expect(items).toHaveLength(2);
    expect(items.map((i) => (i.type === "event" ? i.event.id : null))).toEqual([1, 3]);
  });

  it("keeps an id-less session_init (nothing to dedupe on)", () => {
    const items = coalesceActivity([
      ev(1, { kind: "session_init" }),
      ev(2, { kind: "session_init" }),
    ]);
    expect(items).toHaveLength(2);
  });
});
