// Bounded context: the WIRE shape of a message reference and of the sender-only
// "addressed nobody" report (#task:2021 P4-5-b). Re-exported via api/index.ts →
// @silverhand/shared.
//
// Its own module, and deliberately a LEAF (zod + the reference domain + the name
// grammar, nothing else): both send surfaces need this one declaration, and
// `api/message.ts` ⇄ `api/task.ts` ⇄ `api/internal-agent.ts` is a live import
// cycle — a shared value declared inside that cycle is read before it is
// initialized. A leaf module is what makes "one declaration for both surfaces"
// actually loadable.
import { z } from 'zod';
import { REFERENCE_KINDS, type Reference } from '../reference.js';
import { ParticipantNameSchema } from './name.js';

/**
 * The wire shape of ONE reference — annotated as `z.ZodType<Reference>` so it is
 * the DOMAIN shape by compile-time obligation, not by resemblance. A second
 * reference grammar is precisely how a producer and a consumer come to disagree
 * about what a token names, so the schema owes its fields to `reference.ts` and
 * the stored-name domain.
 *
 * `ParticipantNameSchema`, not `HandleSchema`: this is an OUTPUT, and the value
 * is a stored name. `HandleSchema` tolerates a display `@` for command
 * ARGUMENTS; accepting one here would mean the server had emitted a handle no
 * resolver may see.
 */
export const MessageReferenceSchema: z.ZodType<Reference> = z.object({
  kind: z.enum(REFERENCE_KINDS),
  handle: ParticipantNameSchema,
});

/**
 * The references in the sent body that addressed NOBODY: the handle names no
 * principal, or it names one that does not participate in the conversation the
 * message landed in. Absent when every reference was addressed — and for a body
 * that carried none.
 *
 * Deliberately reason-free and deliberately sender-only. The two cases above are
 * ONE outcome because splitting them would let anyone probe whether a name
 * exists anywhere in the deployment by mentioning it; and it rides the SENDER's
 * response rather than the message, because a reference that landed nowhere is a
 * fact about this attempt, not about the message everyone else reads.
 *
 * The send still SUCCEEDED. An unresolved reference never withholds delivery
 * from the recipients who were addressed — it is feedback, not a refusal, and
 * the author decides whether to resend.
 *
 * ONE declaration for BOTH send surfaces — the agent's `AgentSendMessageOutput`
 * and the human's `MessageSendAck`. "Did my mention land?" is one question, and
 * the actor's kind is no part of it: a per-surface copy is how one of them would
 * quietly lose the answer.
 */
export const UnresolvedReferencesSchema = z.array(MessageReferenceSchema).nonempty().optional();
