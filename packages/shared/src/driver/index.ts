/**
 * Driver-side type contracts shared between backend (prompt assembly,
 * sanitization) and host (parse fanout). The actual `Driver` interface
 * and concrete implementations live in `@silverhand/host/drivers` since
 * they use Node-only APIs (`crypto`, `child_process`, `NodeJS.ProcessEnv`).
 *
 * `ParsedEvent` is the unified event union a driver's `parseLine`
 * returns. It has two consumers:
 *
 *   - The host's RealSupervisor — emits each parsed event as a
 *     `runtime_parsed_event` frame on the daemon→backend wire (see
 *     `@silverhand/host` `protocol.ts`). This crossing is the reason
 *     the union needs a runtime zod schema: the host validates outbound
 *     frames (`ParsedEventSchema.parse(parsed)` before encoding); the
 *     backend validates inbound frames again on the other end.
 *
 *   - Backend's `sanitizeParsedEventForBroadcast` — strips fields not
 *     safe for renderer / DB projection (e.g. raw tool input).
 *
 * Cindy `OQCA05pivtB8nSTC` locked the zod requirement for this exact
 * boundary: the cross-process projection must validate end-to-end, not
 * trust the producer.
 */
import { z } from 'zod';
import { DAEMON_ADMISSION } from '../daemon-admission.generated.js';

/**
 * "Required but opaque" — a bare `z.unknown()` at an object property
 * is treated by zod as OPTIONAL (missing key parses fine), which
 * would silently downgrade fields the TS type marks required.
 * `.refine(v => v !== undefined, ...)` re-asserts presence while
 * keeping the value's *shape* opaque.
 *
 * Used on `ParsedEvent.tool_call.input` so the wire schema matches
 * the original TS contract (Cindy `YW2BGWryWjfymPkU` B1). Drivers
 * MUST supply the input field — even if it's `null` or an empty
 * object — and never omit it entirely.
 */
const RequiredUnknownSchema = z.unknown().refine((v) => v !== undefined, {
  message: 'field is required (opaque payload must be present)',
});

/**
 * Normalized terminal outcome of a tool call, projected from each runtime's
 * own completion signal (codex `status`+`exitCode`, Claude `tool_result.is_error`,
 * ACP terminal status). It is the SOLE input to the activity log's success ✓:
 * only `success` paints green; `failed`/`cancelled` must never masquerade as
 * success; `completed` is the neutral "finished but outcome not provable"
 * downgrade. A runtime that cannot prove success (missing/ambiguous status,
 * unknown exit-code semantics) emits `completed`, never a guessed `success`.
 */
export const TOOL_OUTCOMES = ['success', 'failed', 'cancelled', 'completed'] as const;
export const ToolOutcomeSchema = z.enum(TOOL_OUTCOMES);
export type ToolOutcome = (typeof TOOL_OUTCOMES)[number];

/**
 * Runtime-independent description of a tool call. Each driver maps its native
 * tool frames to this at its own boundary; the activity view renders only this.
 * `other` keeps the native `name`/`input` as the whole story. Bounded strings
 * may arrive truncated by the backend sanitizer, so consumers parse leniently.
 */
const ToolText = z.string();
export const ToolSchema = z.discriminatedUnion('type', [
  z.object({ type: z.literal('command'), command: ToolText }),
  z.object({ type: z.literal('read'), path: ToolText }),
  z.object({ type: z.literal('write'), path: ToolText, content: ToolText.optional() }),
  z.object({
    type: z.literal('edit'),
    path: ToolText,
    /** File-level change when the runtime reports one instead of text edits. */
    change: z.enum(['add', 'update', 'delete']).optional(),
    edits: z.array(z.object({ old: ToolText, new: ToolText })).optional(),
  }),
  z.object({
    type: z.literal('search'),
    pattern: ToolText,
    path: ToolText.optional(),
    glob: ToolText.optional(),
  }),
  z.object({ type: z.literal('find'), pattern: ToolText, path: ToolText.optional() }),
  z.object({ type: z.literal('list'), path: ToolText.optional() }),
  z.object({ type: z.literal('web_search'), query: ToolText }),
  z.object({ type: z.literal('web_fetch'), url: ToolText }),
  z.object({ type: z.literal('subagent'), description: ToolText }),
  z.object({ type: z.literal('skill'), name: ToolText }),
  z.object({ type: z.literal('mcp'), server: ToolText, tool: ToolText }),
  z.object({ type: z.literal('other') }),
]);
export type Tool = z.infer<typeof ToolSchema>;

export const ParsedEventSchema = z.discriminatedUnion('kind', [
  z.object({
    kind: z.literal('session_init'),
    sessionId: z.string().min(1),
  }),
  z.object({
    kind: z.literal('thinking'),
    text: z.string(),
  }),
  z.object({
    kind: z.literal('text'),
    text: z.string(),
  }),
  z.object({
    kind: z.literal('tool_call'),
    name: z.string().min(1),
    /** Tool input is driver-specific; we keep it `unknown` here so
     *  the cross-process schema doesn't constrain individual driver
     *  payload shapes. Sanitization for renderer / DB is backend's
     *  responsibility (see `sanitizeParsedEventForBroadcast`).
     *
     *  REQUIRED but opaque (Cindy `YW2BGWryWjfymPkU` B1): a bare
     *  `z.unknown()` at an object property is treated by zod as
     *  optional — `{kind:'tool_call', name:'Bash'}` without an
     *  `input` field would parse fine, silently downgrading the
     *  contract. `RequiredUnknownSchema` re-asserts presence. */
    input: RequiredUnknownSchema,
    tool: ToolSchema.optional(),
    toolCallId: z.string().min(1).optional(),
  }),
  z.object({
    /** Late-arriving parameters for a tool call the driver ALREADY
     *  reported (#task:2101). ACP-family runtimes create a tool call
     *  and revise it: OpenCode's first `tool_call` frame carries no
     *  `rawInput` (we persist `{cwd}` alone), and the real command
     *  arrives in a `tool_call_update` for the same `toolCallId`.
     *
     *  This is a REFINEMENT of one existing activity row, never a new
     *  call: `tool_call` is what increments `outstandingToolUses` and
     *  `toolCallCounts` (supervisor), so re-emitting `tool_call` for a
     *  known id would double-count both and desync the outstanding
     *  gauge. Like `input_lifecycle`, this kind touches NO run state.
     *
     *  `toolCallId` is REQUIRED (unlike on `tool_call`): without an id
     *  there is no row to refine, and a driver that cannot correlate
     *  must simply not emit this. */
    kind: z.literal('tool_call_input'),
    toolCallId: z.string().min(1),
    input: RequiredUnknownSchema,
    tool: ToolSchema.optional(),
  }),
  z.object({
    kind: z.literal('tool_output'),
    name: z.string().min(1).optional(),
    toolCallId: z.string().min(1).optional(),
    /** Terminal outcome — REQUIRED on the emit type (`z.infer` output makes
     *  it non-optional, so every driver that builds a `tool_output` must
     *  classify it; a new producer cannot silently omit it). Wire-optional
     *  via `.default('completed')`: pre-outcome daemon frames and historical
     *  rows parse to the neutral `completed`, never rejected and never
     *  upgraded to `success`. */
    outcome: ToolOutcomeSchema.default('completed'),
  }),
  z.object({
    kind: z.literal('turn_end'),
    sessionId: z.string().min(1).optional(),
  }),
  z.object({
    kind: z.literal('compaction_started'),
  }),
  z.object({
    kind: z.literal('compaction_finished'),
  }),
  z.object({
    /** Per-payload input lifecycle reported by the runtime itself
     *  (Claude `command_lifecycle` frames, #task:1557 Phase 1
     *  instrument). `commandUuid` echoes the `uuid` the daemon put on
     *  the stdin user frame, so the supervisor can correlate which
     *  delivery/batch the runtime queued/admitted. Observation only:
     *  it never drives run-state or freshness in Phase 1. */
    kind: z.literal('input_lifecycle'),
    commandUuid: z.string().min(1),
    state: z.enum(['queued', 'started', 'completed']),
  }),
  z.object({
    /** Normalized provider/API response notice (#task:1582). Emitted
     *  ONLY for responses the driver recognizes and normalizes (e.g.
     *  Qoder `model_queue_status` queued/ready) — never raw payloads,
     *  request ids, or unknown shapes (those stay in diagnostics).
     *  `status: 'waiting'` puts the run state into the dedicated
     *  `waiting` status; `'ready'` restores `thinking` (only from a
     *  waiting-owned hold); `'notice'` is display-only — it records an
     *  Activity row and never touches the run state. Not
     *  generation-boundary evidence: it never starts a turn and never
     *  settles pending admission coverage. */
    kind: z.literal('api_response'),
    status: z.enum(['waiting', 'ready', 'notice']),
    message: z.string().min(1),
  }),
  z.object({
    kind: z.literal('error'),
    message: z.string(),
    code: z.string().optional(),
    fatal: z.boolean().optional(),
  }),
  z.object({
    kind: z.literal('unknown'),
    /** The original line bytes. Host MUST NOT leak this onto any
     *  `runtime_parsed_event_dropped` frame (Cindy
     *  `C2BZGDbQWfnq-bKJ`): the dropped frame is metadata-only. */
    raw: z.string(),
  }),
]);

export type ParsedEvent = z.infer<typeof ParsedEventSchema>;

/**
 * Enumeration of every `ParsedEvent.kind`. Useful for switch
 * exhaustiveness checks and for typing the `parsed_kind` field on
 * `runtime_parsed_event_dropped` frames (where we keep the kind
 * marker but strip the payload).
 */
export const PARSED_EVENT_KINDS = DAEMON_ADMISSION.parsedEventKinds satisfies ReadonlyArray<
  ParsedEvent['kind']
>;

export const ParsedEventKindSchema = z.enum(PARSED_EVENT_KINDS);
export type ParsedEventKind = ParsedEvent['kind'];
