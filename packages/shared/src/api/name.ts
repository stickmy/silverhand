import { z } from 'zod';
import {
  canonicalizeStoredWorkspaceName,
  parseWorkspaceName,
  WORKSPACE_NAME_MAX_CODE_POINTS,
} from '../workspace-name.js';

/** Max length for user-chosen entity names (agents, workspaces). The QUANTITY
 *  is shared; the unit is per-schema (see each schema's docblock). */
export const ENTITY_NAME_MAX_LENGTH = 64;

/**
 * The participant-handle CHARACTER grammar, as a regex source fragment with no
 * anchors and no capture group.
 *
 * Exported because a handle is spelled in more than one syntax and every one of
 * them must admit exactly the same set of names: the stored value
 * ({@link ParticipantNameSchema} below), the plain `@name` display alias
 * (`PLAIN_MENTION_PATTERN` in `api/message.ts`), and the handle segment of a
 * canonical reference destination (`reference.ts`). A second copy of the
 * character class is how one syntax comes to accept a name another rejects —
 * which, for the destination form, means a chip that never delivers. The LENGTH
 * half of the domain is {@link ENTITY_NAME_MAX_LENGTH}; a consumer that needs
 * the full stored domain must apply both.
 */
export const PARTICIPANT_HANDLE_PATTERN = '[a-zA-Z][a-zA-Z0-9_-]*';

/**
 * Stored participant-name grammar. A participant name is a durable mention
 * handle, so every name admitted here must also be representable by the
 * message and renderer `@mention` parsers. The optional display `@` belongs to
 * command inputs (`HandleSchema`), never to the stored value.
 *
 * Agent AND user input paths validate against this schema directly: password
 * signup takes the handle from the user (`AuthSignupInputSchema.name`,
 * #task:1830), and `UserRepo.create` re-asserts it at the persistence boundary
 * (#task:1828). The remaining system-generated user names — embedded bootstrap's
 * `local` and the legacy `user-<4hex>` registration suggestion — satisfy it by
 * construction.
 *
 * Kept separate from {@link WorkspaceNameSchema}: workspace names are display
 * labels and must not inherit participant-addressing constraints.
 *
 * Length counts UTF-16 code units, which is exact here because the grammar is
 * ASCII-only. Workspace names admit astral characters and therefore count code
 * points instead — the max is shared, the unit is not.
 */
export const ParticipantNameSchema = z
  .string()
  .min(1)
  .max(ENTITY_NAME_MAX_LENGTH)
  .regex(
    new RegExp(`^${PARTICIPANT_HANDLE_PATTERN}$`),
    'participant name must start with a letter and contain only letters, numbers, dash, or underscore',
  );

/**
 * Suggest a participant name from an email address, for PREFILLING a signup
 * handle field. Advisory only: the returned value is a starting point the user
 * may overwrite, and it says nothing about uniqueness.
 *
 * This is the retired half of the old signup deriver (#task:1830). The stored
 * `users.name` used to be whatever that deriver computed, which is how a
 * permanent, non-renamable handle came to be decided by the shape of an email
 * address. Now the user chooses and this only seeds the field — so it lives next
 * to the grammar it must respect, with no second copy in the renderer.
 *
 * Output is either **empty** or admissible by {@link ParticipantNameSchema}:
 * after the leading non-letters are dropped the first character is a letter,
 * every surviving character is in the allowed class, and slicing a prefix cannot
 * break either property. Empty is deliberate rather than a `user` placeholder
 * (@Norman, #task:1830): when nothing of the local-part survives — a fully
 * non-ASCII address, say — a plausible-looking default on the one field that can
 * never be changed invites being accepted unread. An empty required field asks
 * the question instead.
 */
export function suggestParticipantNameFromEmail(email: string): string {
  const localPart = email.trim().toLowerCase().split('@')[0] ?? '';
  const cleaned = localPart
    .replace(/[^a-z0-9_-]+/g, '-')
    .replace(/^[^a-z]+/, '')
    .replace(/[-_]+$/g, '');
  return cleaned.slice(0, ENTITY_NAME_MAX_LENGTH);
}

/**
 * Canonical form for a stored workspace name.
 *
 * A regular workspace name is display text AND the root segment of its
 * semantic address (#task:2027). It carries a folded uniqueness key and is
 * resolved by the shared DeliveryTarget grammar. Unicode
 * lets one visible name be spelled with different code-point sequences, so
 * without a normal form two identical-looking names compare unequal — the user
 * reads a name off the screen and cannot select it. NFC is an equivalence
 * relation over spellings of the SAME characters, so normalizing costs no name.
 *
 * Deliberately NOT folded: full-width/half-width and simplified/traditional
 * variants. Those are lossy product policies rather than equivalences (not 1:1,
 * region-dependent); folding them would report a legal name as taken with no
 * recourse, whereas merely looking alike is visible and fixable by renaming.
 */
export function normalizeWorkspaceName(name: string): string {
  return canonicalizeStoredWorkspaceName(name).name;
}

/**
 * Stored workspace-name FORMAT rule. Parse output is the branded
 * `{name,key}` terminal pair, so a persistence writer cannot receive display
 * bytes without the key derived from the same parse.
 *
 * A workspace name is a LABEL, not a handle: shiki's 2026-08-05 ruling is that
 * it must accept Chinese, and it deliberately does not inherit the participant
 * addressing grammar ({@link ParticipantNameSchema}). `@`, `#` and `/` are
 * legal: the semantic address uses the shared reversible percent codec rather
 * than shrinking the display-name domain. `/` is structural only after
 * encoding (`%2F` inside this segment), and `@` / `#` are ordinary data here.
 *
 * Length is 64 CODE POINTS (`Array.from`), not UTF-16 units, so an astral
 * character costs one. Grapheme clusters are explicitly not promised: a
 * base+combining-mark pair that NFC cannot compose (a Devanagari or Thai
 * cluster, say) costs more than one, which `Intl.Segmenter` would be needed to
 * avoid — a name is an identifier-shaped field, not prose, so the precision is
 * not worth the dependency. The overflow issue is raised as `too_big` on
 * purpose: the renderer's advisory validator classifies length-vs-charset by
 * issue code, and a plain `.refine` would report `custom` and be shown as a
 * charset error.
 *
 * The invisible-character rule is no longer spelled here: it comes from
 * {@link INVISIBLE_CODE_POINTS}, the character domain shared with conversation
 * titles (#task:2016). Both fields are name-shaped identifiers a person retypes
 * off the screen, so one class is the point — a private-use or default-ignorable
 * code point that a title refuses must not survive in a workspace name that
 * addresses the same coordinate.
 *
 * FORMAT only — current regular-workspace uniqueness is a separate DB-backed
 * concern over the returned folded key (see the name-availability endpoint).
 * Agent-home workspaces never carry that key and do not enter the namespace.
 */
export const WorkspaceNameSchema = z.string().transform((raw, ctx) => {
  const parsed = parseWorkspaceName(raw);
  if (parsed.ok) return parsed.value;
  if (parsed.issue.kind === 'empty') {
    ctx.addIssue({
      code: z.ZodIssueCode.too_small,
      minimum: 1,
      type: 'string',
      inclusive: true,
      message: 'workspace name must not be empty',
    });
    return z.NEVER;
  }
  if (parsed.issue.kind === 'too_long') {
    ctx.addIssue({
      code: z.ZodIssueCode.too_big,
      maximum: WORKSPACE_NAME_MAX_CODE_POINTS,
      type: 'string',
      inclusive: true,
      message: `workspace name must be at most ${WORKSPACE_NAME_MAX_CODE_POINTS} characters`,
    });
    return z.NEVER;
  }
  ctx.addIssue({
    code: z.ZodIssueCode.custom,
    message: 'workspace name contains a character that is not addressable',
  });
  return z.NEVER;
});

/** Why a proposed name is / isn't usable. Invariant: a name is usable iff
 *  `reason === 'available'`. `taken` = uniqueness collision; `invalid` = fails
 *  the queried entity kind's authoritative name schema. */
export const NameAvailabilityReasonSchema = z.enum(['available', 'invalid', 'taken']);
export type NameAvailabilityReason = z.infer<typeof NameAvailabilityReasonSchema>;

/** Response of `GET /api/{agents,workspaces}/name-availability?name=`.
 *  A create-form onBlur probe: the endpoint shares ONE uniqueness predicate
 *  with the matching create path, so the form can never report `available`
 *  for a name that create would reject.
 *
 *  The `available ⇔ reason === 'available'` invariant is enforced here in the
 *  shared schema, not left to route discipline: as a wire DTO this is the
 *  boundary both producer and consumer trust, so a contradictory pair like
 *  `{ available: true, reason: 'taken' }` must be structurally unrepresentable
 *  rather than merely "not produced today". */
export const NameAvailabilityDtoSchema = z
  .object({
    available: z.boolean(),
    reason: NameAvailabilityReasonSchema,
  })
  .refine((dto) => dto.available === (dto.reason === 'available'), {
    message: "available must be true iff reason === 'available'",
  });
export type NameAvailabilityDto = z.infer<typeof NameAvailabilityDtoSchema>;
