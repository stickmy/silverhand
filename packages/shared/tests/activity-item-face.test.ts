import { describe, expect, it } from 'vitest';
import {
  ActivityItemDtoSchema,
  ActivityLocationSchema,
  AgentRuntimeParticipationSchema,
} from '../src/api/index.js';

/**
 * The Activity face answers row-1 identity by PLACEMENT and never by roster
 * (#task:2076, extended by #task:2115).
 *
 * WHY THIS NEEDS A CELL. The mis-keying it forbids is not a wrong branch, it is a
 * FIELD: while `ActivityItemDto` carried `conversation_roster_policy` — with a
 * docblock instructing the renderer to derive a "Channel / DM" label from it — the
 * only way to draw the row-1 glyph was to classify the conversation by its roster,
 * so an agent home's DM channel and its Onboarding channel (workspace-placed,
 * fixed roster) were drawn as private lines. Removing the field is what makes the
 * wrong glyph unrepresentable; this cell is what stops it from coming back, in
 * either spelling.
 *
 * WHAT #task:2115 CHANGED, AND WHAT IT DID NOT. Activity now admits BOTH placements
 * (a workspace-less `direct_pair`'s unread enters the global list), so the face does
 * carry a placement discriminant today — inside `location`, where it is a live
 * two-arm branch rather than the constant `'workspace'` a top-level
 * `conversation_placement_kind` would have been before. The rule is untouched:
 * identity is placement's question, roster's live role for Activity stays
 * SERVER-SIDE (the §5.1 salience grade — a fixed-roster stream's ambient unread is
 * directed-grade — decided in `read-model/activity.ts` off the SQL row), and a
 * fixed-roster channel inside an agent home is still a channel.
 *
 * The keying rule itself is written once, on
 * {@link ConversationPlacementKindSchema}; this file is its enforcement for one
 * face. The route-level counterpart (`backend/tests/routes/activity.test.ts`)
 * asserts the absence on a PARSED HTTP BODY and fails differently: nothing
 * re-validates the response against this schema on the way out, so that cell
 * catches EMITTING a forbidden field while this one catches DECLARING it.
 */
describe('ActivityItemDto keys identity on placement, never on roster (#task:2076)', () => {
  // Written out rather than derived, for the same reason as the workspace key
  // ledger: adding a field to this DTO becomes a decision recorded HERE. An
  // exhaustive equality is also the only form that catches BOTH forbidden
  // spellings plus any third one someone invents — a pair of `not.toContain`
  // assertions on two known names would go green on `conversation_kind`.
  const ACTIVITY_ITEM_KEYS = [
    'id',
    'kind',
    'salience',
    // ONE field carries the whole placement answer. A top-level `workspace_id` +
    // `title` pair is what this replaced (#task:2115): those two are independently
    // defaultable, so "a DM with a workspace" and "a channel with no title" were
    // representable states the renderer had to rule out by inspecting NULLs.
    'location',
    'conversation_id',
    'thread_root_id',
    'message_id',
    'notification_id',
    'thread_root_preview',
    'preview',
    'preview_sender',
    'timestamp',
    'unread_count',
    'mention_count',
  ];

  it('publishes exactly this key set', () => {
    expect(Object.keys(ActivityItemDtoSchema.shape).sort()).toEqual([...ACTIVITY_ITEM_KEYS].sort());
  });

  // The two arms' key sets, exhaustively — the point of the union is that neither
  // arm can carry the other's coordinate. Asserted by PARSING, because that is the
  // property the wire has: zod strips unknown keys, so a parsed DM location that
  // still answered `workspace_id` could only mean the arm declared one.
  it('gives each placement exactly its own coordinates', () => {
    const channel = ActivityLocationSchema.parse({
      kind: 'workspace_channel',
      workspace_id: 'ws-1',
      title: '#general',
      counterpart: {
        status: 'active',
        kind: 'agent',
        id: 'a-1',
        name: 'a-1',
        display_name: 'A',
        avatar_url: null,
        runtime: 'claude',
      },
    });
    expect(Object.keys(channel).sort()).toEqual(['kind', 'title', 'workspace_id']);

    const pair = ActivityLocationSchema.parse({
      kind: 'direct_pair',
      workspace_id: 'ws-1',
      title: 'Should not survive',
      counterpart: {
        status: 'active',
        kind: 'agent',
        id: 'a-1',
        name: 'a-1',
        display_name: 'A',
        avatar_url: null,
        runtime: 'claude',
      },
    });
    // No workspace axis and NO title: a pair holds no title, and the renderer owns
    // the counterpart's wording (`presentCounterpart`), so a server-synthesized
    // display string would fork a second label vocabulary.
    expect(Object.keys(pair).sort()).toEqual(['counterpart', 'kind']);
  });

  // Each arm's own coordinate is REQUIRED, not merely typed. An optional
  // `workspace_id` on the channel arm would let a channel row through with no
  // coordinate to navigate to or to clean up on workspace removal — and the
  // renderer's placement dispatch would still compile.
  it('requires each arm to carry its own coordinate', () => {
    expect(
      ActivityLocationSchema.safeParse({ kind: 'workspace_channel', title: '#g' }).success,
    ).toBe(false);
    expect(
      ActivityLocationSchema.safeParse({ kind: 'workspace_channel', workspace_id: 'w' }).success,
    ).toBe(false);
    expect(ActivityLocationSchema.safeParse({ kind: 'direct_pair' }).success).toBe(false);
    // And the discriminant itself is closed: a third placement word is not a
    // location, so no consumer has to defend against one.
    expect(
      ActivityLocationSchema.safeParse({ kind: 'workspace', workspace_id: 'w', title: 't' })
        .success,
    ).toBe(false);
  });

  // The DIFFERENTIAL control. Without it this file would also pass if the
  // placement word were deleted from the wire everywhere — i.e. if the fix had
  // been "stop publishing placement" rather than "publish it where the face
  // genuinely admits more than one placement". The agent-participation face reads
  // roster ∪ inbox arc ∪ direct_pair arc, so there the word is a live
  // discriminator; Activity's own face now admits both placements too, but says so
  // in its OWN vocabulary (`workspace_channel`, the #task:1373 conclusion) rather
  // than by re-exporting the raw placement enum. The asymmetry is the rule.
  it('keeps the placement word on the face that genuinely admits every placement', () => {
    const participation = {
      conversation_id: 'conversation-1',
      workspace_id: null,
      workspace_name: null,
      conversation_roster_policy: 'fixed',
      channel_id: 'conversation-1',
      channel_title: '',
      closed_at: null,
    };
    for (const placement of ['workspace', 'direct_pair']) {
      expect(
        AgentRuntimeParticipationSchema.parse({
          ...participation,
          conversation_placement_kind: placement,
        }).conversation_placement_kind,
      ).toBe(placement);
    }
    expect(AgentRuntimeParticipationSchema.safeParse(participation).success).toBe(false);
    expect(
      AgentRuntimeParticipationSchema.safeParse({
        ...participation,
        conversation_placement_kind: 'private',
      }).success,
    ).toBe(false);
  });
});
