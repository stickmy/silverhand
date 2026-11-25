/**
 * Presentation-only metadata for a single runtime inbox delivery.
 *
 * Custody is tracked by delivery id, but the agent-visible payload is released
 * as an atomic batch.  Keeping the small, already-formatted body fragment and
 * its instruction signal lets the daemon rebuild a canonical payload for the
 * novel remainder of a response-loss retry without turning every normal batch
 * into singleton runtime turns.
 */
import { resolveAttachmentLabels, type AttachmentSnapshot } from './api/attachment.js';
import type { MessageAnswerDto, MessageOptions } from './api/message-options.js';
import type { Reference } from './reference.js';

export const RUNTIME_DELIVERY_INSTRUCTION_SIGNALS = [
  'user_for_recipient',
  'user_for_other',
  'non_system_mention',
  'system',
  'other',
] as const;

export type RuntimeDeliveryInstructionSignal =
  (typeof RUNTIME_DELIVERY_INSTRUCTION_SIGNALS)[number];

export interface RuntimeDeliveryPresentation {
  /** Canonical message body fragment: meta header + content + attachments. */
  body: string;
  /** Minimal input needed to derive the batch-level reply instruction. */
  instruction: RuntimeDeliveryInstructionSignal;
}

export type RuntimeDeliveryFromKind = 'agent' | 'user' | 'system';

export interface RuntimeDeliveryHeaderInput {
  readonly messageId: string;
  readonly sender:
    | { readonly kind: 'agent'; readonly handle: string | null; readonly computer: string | null }
    | { readonly kind: 'human'; readonly handle: string | null }
    | { readonly kind: 'system' };
  readonly target: string;
  readonly timestamp: string;
  readonly task: { readonly number: number; readonly status: string } | null;
}

/** Format the fixed-order header once sender, address and timestamp authorities answered. */
export function formatRuntimeDeliveryHeader(input: RuntimeDeliveryHeaderInput): string {
  const task = input.task === null ? '' : ` task=#${input.task.number} status=${input.task.status}`;
  switch (input.sender.kind) {
    case 'agent': {
      const sender = input.sender.handle === null ? 'agent' : `@${input.sender.handle}`;
      const computer = input.sender.computer === null ? '' : ` computer=${input.sender.computer}`;
      return `[msg_id=${input.messageId} sender=${sender} type=agent${computer} target=${input.target} time=${input.timestamp}${task}]`;
    }
    case 'human': {
      const sender = input.sender.handle === null ? 'user' : `@${input.sender.handle}`;
      return `[msg_id=${input.messageId} sender=${sender} type=human target=${input.target} time=${input.timestamp}${task}]`;
    }
    case 'system':
      return `[msg_id=${input.messageId} sender=system type=system target=${input.target} time=${input.timestamp}${task}]`;
  }
}

/** Deterministic attachment suffix shared by every runtime delivery producer. */
export function formatRuntimeDeliveryAttachmentSuffix(
  attachments: readonly AttachmentSnapshot[] | null | undefined,
): string {
  if (!attachments || attachments.length === 0) return '';
  const labels = resolveAttachmentLabels(attachments);
  const lines = attachments.map(
    (attachment, index) =>
      `  - ${labels[index]} — id=${attachment.id} filename=${JSON.stringify(attachment.filename)} ` +
      `content_type=${attachment.content_type} size=${attachment.size}`,
  );
  const header =
    attachments.length === 1
      ? '[1 attachment — the label (e.g. `Image #1`) is how the sender refers to it; use `silverhand file view --attachment-id <id>` with the id below to read it]'
      : `[${attachments.length} attachments — the label (e.g. \`Image #1\`) is how the sender refers to each; use \`silverhand file view --attachment-id <id>\` with each id below to read them]`;
  return `\n\n${header}\n${lines.join('\n')}`;
}

/** Options block under a question; mirrors the Rust delivery projector. */
export function formatRuntimeDeliveryOptionsSuffix(
  messageId: string,
  options: MessageOptions | null | undefined,
  answer: MessageAnswerDto | null | undefined,
): string {
  if (!options) return '';
  const mode = options.mode === 'single' ? 'single choice' : 'multiple choice';
  const header = answer
    ? `[Options (${mode}), answered by @${answer.by.name}]`
    : `[Options (${mode}), unanswered — answer with \`silverhand message answer --message-id ${messageId} --option <id>\` or \`--other <text>\`]`;
  const lines = options.items.map(
    (item) =>
      `  - ${item.id}: ${item.label}${answer?.option_ids.includes(item.id) ? '  ← answer' : ''}`,
  );
  if (answer?.other != null) lines.push(`  - other: ${answer.other}  ← answer`);
  return `\n\n${header}\n${lines.join('\n')}`;
}

/** Classify the reply instruction from the one derived reference set. */
export function runtimeDeliveryInstructionSignal(input: {
  readonly fromKind: RuntimeDeliveryFromKind;
  readonly references: readonly Reference[];
  readonly recipientAgentName: string | undefined;
}): RuntimeDeliveryInstructionSignal {
  const addressesRecipient =
    input.recipientAgentName !== undefined &&
    input.references.some(
      (reference) => reference.kind === 'agent' && reference.handle === input.recipientAgentName,
    );
  if (input.fromKind === 'user') {
    return input.references.length === 0 || addressesRecipient
      ? 'user_for_recipient'
      : 'user_for_other';
  }
  if (input.fromKind !== 'system' && addressesRecipient) {
    return 'non_system_mention';
  }
  return input.fromKind === 'system' ? 'system' : 'other';
}

/** Compose one canonical inbox envelope from a selected delivery group. */
export function composeRuntimeDeliveryPresentation(
  entries: readonly RuntimeDeliveryPresentation[],
  sendMessageTool: string,
): string {
  if (entries.length === 0) {
    throw new Error('runtime delivery presentation requires a non-empty batch');
  }
  const header =
    entries.length === 1
      ? 'You have one message waiting since you were last active:'
      : `You have ${entries.length} messages waiting since you were last active (oldest first):`;
  const body = entries.map((entry) => entry.body).join('\n\n---\n\n');
  return `${header}\n\n${body}\n\n${composeRuntimeDeliveryInstruction(entries, sendMessageTool)}`;
}

/** Derive the one batch-level instruction from per-message signals. */
export function composeRuntimeDeliveryInstruction(
  entries: readonly RuntimeDeliveryPresentation[],
  sendMessageTool: string,
): string {
  const signals = new Set(entries.map((entry) => entry.instruction));
  if (signals.has('user_for_recipient')) {
    return (
      `Respond as appropriate — call \`${sendMessageTool}\` with a brief reply, ` +
      `progress update, blocker question, or final summary before stopping. ` +
      `Complete ALL your work before stopping.`
    );
  }
  if (signals.has('user_for_other')) {
    return (
      `The user message appears addressed to another agent. Do not call ` +
      `\`${sendMessageTool}\` unless you own or were assigned the relevant ` +
      `task, or must flag a concrete blocker. Complete ALL work you choose ` +
      `to take before stopping.`
    );
  }
  if (signals.has('non_system_mention')) {
    return (
      `Respond as appropriate — call \`${sendMessageTool}\` before stopping. ` +
      `Complete ALL your work before stopping.`
    );
  }
  if (entries.every((entry) => entry.instruction === 'system')) {
    return (
      `Respond as appropriate. System messages do not require a reply ` +
      `unless they explicitly ask for one.`
    );
  }
  return 'Respond as appropriate.';
}
