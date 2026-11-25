// Bounded context: message answer options (#task:3143).
// A message may carry a closed set of options; readers answer by picking one
// (single) or several (multi), optionally with free "other" text.
import { z } from 'zod';

export const MESSAGE_OPTION_ID_PATTERN = /^[a-z0-9][a-z0-9_-]{0,15}$/;
export const MIN_MESSAGE_OPTIONS = 2;
export const MAX_MESSAGE_OPTIONS = 10;
export const MAX_MESSAGE_OPTION_LABEL_LENGTH = 200;
export const MAX_MESSAGE_ANSWER_OTHER_LENGTH = 500;

export const MessageOptionModeSchema = z.enum(['single', 'multi']);
export type MessageOptionMode = z.infer<typeof MessageOptionModeSchema>;

export const MessageOptionItemSchema = z.object({
  id: z.string().regex(MESSAGE_OPTION_ID_PATTERN),
  label: z.string().trim().min(1).max(MAX_MESSAGE_OPTION_LABEL_LENGTH),
});
export type MessageOptionItem = z.infer<typeof MessageOptionItemSchema>;

/** The options a message author declares. Ids are author-chosen and unique
 *  within the message; "Other" is never declared here — clients always offer it. */
export const MessageOptionsSchema = z
  .object({
    mode: MessageOptionModeSchema,
    items: z.array(MessageOptionItemSchema).min(MIN_MESSAGE_OPTIONS).max(MAX_MESSAGE_OPTIONS),
  })
  .refine(
    (options) => new Set(options.items.map((item) => item.id)).size === options.items.length,
    {
      message: 'option ids must be unique within a message',
      path: ['items'],
    },
  );
export type MessageOptions = z.infer<typeof MessageOptionsSchema>;

export const MessageAnswererSchema = z.object({
  kind: z.enum(['user', 'agent']),
  id: z.string(),
  name: z.string(),
  display_name: z.string(),
});
export type MessageAnswerer = z.infer<typeof MessageAnswererSchema>;

/** The one current answer on a message; a re-answer replaces it. */
export const MessageAnswerDtoSchema = z.object({
  by: MessageAnswererSchema,
  option_ids: z.array(z.string()),
  other: z.string().nullable(),
  answered_at: z.number(),
});
export type MessageAnswerDto = z.infer<typeof MessageAnswerDtoSchema>;

/** Answer body, shared by the human and agent doors. Single mode takes exactly
 *  one of `option_ids` (one id) or `other`; multi mode takes at least one of the
 *  two. Mode-dependent checks run server-side against the stored options. */
export const MessageAnswerInputSchema = z
  .object({
    option_ids: z.array(z.string().min(1)).max(MAX_MESSAGE_OPTIONS).optional(),
    other: z.string().trim().min(1).max(MAX_MESSAGE_ANSWER_OTHER_LENGTH).optional(),
  })
  .refine((input) => (input.option_ids?.length ?? 0) > 0 || input.other !== undefined, {
    message: 'answer must pick at least one option or give other text',
    path: ['option_ids'],
  });
export type MessageAnswerInput = z.infer<typeof MessageAnswerInputSchema>;

export const AgentAnswerMessageInputSchema = z
  .object({
    message_id: z.string().min(1),
    option_ids: z.array(z.string().min(1)).max(MAX_MESSAGE_OPTIONS).optional(),
    other: z.string().trim().min(1).max(MAX_MESSAGE_ANSWER_OTHER_LENGTH).optional(),
  })
  .refine((input) => (input.option_ids?.length ?? 0) > 0 || input.other !== undefined, {
    message: 'answer must pick at least one option or give other text',
    path: ['option_ids'],
  });
export type AgentAnswerMessageInput = z.infer<typeof AgentAnswerMessageInputSchema>;

export const AgentUpdateMessageOptionsInputSchema = z.object({
  message_id: z.string().min(1),
  options: MessageOptionsSchema,
});
export type AgentUpdateMessageOptionsInput = z.infer<typeof AgentUpdateMessageOptionsInputSchema>;

/** ACK of an answer write (human and agent doors). */
export const MessageAnswerAckSchema = z.object({
  message_id: z.string(),
  answer: MessageAnswerDtoSchema,
  /** The system event message this answer posted. */
  event_message_id: z.string(),
});
export type MessageAnswerAck = z.infer<typeof MessageAnswerAckSchema>;

/** ACK of an options update (agent door; only agents author options). */
export const MessageOptionsUpdateAckSchema = z.object({
  message_id: z.string(),
  options: MessageOptionsSchema,
});
export type MessageOptionsUpdateAck = z.infer<typeof MessageOptionsUpdateAckSchema>;

/** WS `message_options_changed` payload (scope `conversation:<cid>`): the
 *  absolute post-change options and answer of one message, emitted on every
 *  answer, re-answer and options update. Clients replace, never merge. */
export const MessageOptionsChangedEventSchema = z.object({
  message_id: z.string(),
  conversation_id: z.string(),
  thread_root_id: z.string().nullable(),
  options: MessageOptionsSchema,
  answer: MessageAnswerDtoSchema.nullable(),
});
export type MessageOptionsChangedEvent = z.infer<typeof MessageOptionsChangedEventSchema>;

export const MESSAGE_OPTIONS_CHANGED_EVENT = 'message_options_changed';

/** Refusal codes of the answer / options-update doors. */
export const MESSAGE_OPTIONS_ERROR_CODES = {
  /** The message carries no options. */
  noOptions: 'no_options',
  /** An option id is not in the message's current options. */
  unknownOption: 'unknown_option',
  /** The answer shape does not fit the mode (single: exactly one pick). */
  invalidAnswer: 'invalid_answer',
  /** The question's author tried to answer it. */
  ownQuestion: 'own_question',
  /** Options can no longer change once answered. */
  optionsAnswered: 'options_answered',
  /** Only the author may update options. */
  notAuthor: 'not_author',
} as const;
