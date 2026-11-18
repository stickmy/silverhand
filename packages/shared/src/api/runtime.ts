// AUTO-SPLIT from the former monolithic api/dto.ts (#task:963).
// Bounded context: runtime. Re-exported via api/index.ts → @silverhand/shared.
import { z } from 'zod';
import { ReasoningEffortTokenSchema, RuntimeKindSchema } from '../db/schema.js';

// Ephemeral per-agent run state used by `agent_activity` realtime events and
// reconnect snapshots. These rows are not persisted to the messages DB.
export const AgentRunStatusSchema = z.enum([
  'starting',
  'thinking',
  'tool_running',
  // Provider/API-level hold (#task:1582): the runtime is queued or
  // rate-limited upstream — no inference is running. Entered by a
  // driver-normalized `api_response(status:'waiting')`; a subsequent
  // `'ready'` (or any real activity) returns to `thinking`.
  'waiting',
  'idle',
  'failed',
]);
export type AgentRunStatus = z.infer<typeof AgentRunStatusSchema>;

export const AgentRunTerminalReasonSchema = z.enum([
  'replied',
  'turn_ended',
  'replaced',
  'stopped',
  'crashed',
  'timeout',
  'conversation_closed',
]);
export type AgentRunTerminalReason = z.infer<typeof AgentRunTerminalReasonSchema>;

export const AgentActivitySnippetKindSchema = z.enum([
  'thinking',
  'text',
  // Normalized provider/API notice (#task:1582) — its own display lane,
  // never blended into the thinking/text streaming semantics.
  'api_response',
]);
export type AgentActivitySnippetKind = z.infer<typeof AgentActivitySnippetKindSchema>;

export const AgentRunStateSchema = z.object({
  runId: z.string(),
  agentId: z.string(),
  conversationId: z.string(),
  status: AgentRunStatusSchema,
  startedAt: z.number(),
  updatedAt: z.number(),
  lastActivityAt: z.number(),
  terminalReason: AgentRunTerminalReasonSchema.optional(),
  activitySnippet: z.string().optional(),
  activitySnippetKind: AgentActivitySnippetKindSchema.optional(),
  toolCallCounts: z.record(z.string(), z.number()).optional(),
  compacting: z.boolean().optional(),
  compactingSince: z.number().optional(),
  compactionStale: z.boolean().optional(),
  detail: z.string().optional(),
});
export type AgentRunState = z.infer<typeof AgentRunStateSchema>;

export const AgentActivityEventSchema = AgentRunStateSchema;
export type AgentActivityEvent = AgentRunState;

export const AgentRunStatesDtoSchema = z.object({
  states: z.array(AgentRunStateSchema),
});
export type AgentRunStatesDto = z.infer<typeof AgentRunStatesDtoSchema>;

export const RuntimeAvailabilityStateSchema = z.enum([
  'present',
  'present_unverified',
  'absent',
  'unknown',
]);
export type RuntimeAvailabilityState = z.infer<typeof RuntimeAvailabilityStateSchema>;

export const RuntimeDetectionReasonSchema = z.enum([
  'resolved',
  'version_unavailable',
  'not_found',
  'probe_failed',
]);
export type RuntimeDetectionReason = z.infer<typeof RuntimeDetectionReasonSchema>;

export const RuntimeInfoSchema = z.object({
  kind: RuntimeKindSchema,
  state: RuntimeAvailabilityStateSchema,
  reason: RuntimeDetectionReasonSchema,
  installed: z.boolean(),
  binary: z.string().nullable(),
  version: z.string().nullable(),
});
export type RuntimeInfo = z.infer<typeof RuntimeInfoSchema>;

const ModelReasoningSchema = z
  .object({
    supported_efforts: z.array(ReasoningEffortTokenSchema).max(64),
    default_effort: ReasoningEffortTokenSchema.optional(),
  })
  .refine(
    (value) =>
      value.default_effort === undefined || value.supported_efforts.includes(value.default_effort),
  );

/** Suggestions from the selected host's CLI, never an allowlist for model IDs. */
export const RuntimeModelDiscoverySchema = z
  .object({
    runtime: RuntimeKindSchema,
    status: z.enum(['ok', 'unsupported', 'error']),
    models: z
      .array(
        z
          .string()
          .min(1)
          .max(512)
          .refine((id) => !/[\u0000-\u001f\u007f-\u009f]/u.test(id)),
      )
      .max(5000),
    reasoning_by_model: z.record(z.string(), ModelReasoningSchema).optional(),
    default_model: z.string().optional(),
    detected_at: z.number(),
    cached: z.boolean(),
  })
  .refine(
    (value) =>
      (value.status === 'ok' || value.models.length === 0) &&
      Object.keys(value.reasoning_by_model ?? {}).every((id) => value.models.includes(id)) &&
      (value.default_model === undefined || value.models.includes(value.default_model)),
  );
export type RuntimeModelDiscovery = z.infer<typeof RuntimeModelDiscoverySchema>;

export const RuntimeCatalogResponseSchema = z.object({
  runtimes: z.array(RuntimeInfoSchema),
  detected_at: z.number(),
  model_discovery: RuntimeModelDiscoverySchema.optional(),
});
export type RuntimeCatalogResponse = z.infer<typeof RuntimeCatalogResponseSchema>;
