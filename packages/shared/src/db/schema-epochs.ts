import { BASELINE_MIGRATION_STEP } from './baseline-schema.js';
import { migrations, type Migration } from './migrations.js';

/**
 * Two axes, one ledger (#task:2103).
 *
 * A release tells users and operators ONE number: the **schema epoch**. It moves
 * at most once per shipped build, and it is what every product-facing surface
 * says — the `Migration applied` log line, the backup story, and the three
 * `DatabaseMigrationError` texts.
 *
 * A release IMPLEMENTS that move with any number of **migration steps**. Steps
 * keep everything they have today: one exclusive transaction each, their own
 * `precondition`, their own `foreignKeyEscape` mode, and crash-resume at step
 * granularity. Nothing here squashes them, and nothing here may tempt a future
 * change to: v84's correctness IS a cascade under `foreign_keys = ON` while ten
 * of its neighbours must run with foreign keys OFF, so "one release, one SQL
 * script" is not merely expensive, it is wrong.
 *
 * The two axes need NO new table. `_migrations` stays the only persisted ledger,
 * and this module is a pure projection of it: (applied step set, frozen manifest)
 * → epoch state. Persisting an epoch column would create a second writer for a
 * fact the ledger already determines, and a second writer is a second thing that
 * can be half-written by a crash.
 *
 * The mechanism also stays step-based on purpose. Step comparison is strictly
 * FINER than epoch comparison, so it decides everything epoch comparison could
 * plus one case epoch comparison cannot: an old binary meeting a database that a
 * newer binary upgraded only PARTWAY sees `max(step)` above its own known set and
 * refuses, without anyone having to publish a "partial epoch" state. Epoch is
 * therefore vocabulary and reporting, not a state machine.
 */

export type SchemaEpochEntry =
  | {
      /** Outward release epoch. Ascends by exactly 1. */
      readonly epoch: number;
      /** First migration step this epoch owns (inclusive). */
      readonly firstStep: number;
      /** Last migration step this epoch owns (inclusive). */
      readonly lastStep: number;
      /**
       * A build carrying this epoch has shipped, so its step set is CLOSED: the
       * next schema change opens the next epoch instead of extending this one.
       *
       * What is machine-checked and what is not (read this before trusting the
       * word "frozen"): `assertSchemaEpochManifest` proves the manifest covers
       * exactly this build's plan and that at most one epoch is unreleased — it
       * canNOT prove a released epoch was never extended, because nothing in the
       * tree knows where a released boundary used to be, and a second in-repo copy
       * of the boundary would just move that claim one file over. "Has this
       * shipped?" is a fact about the world OUTSIDE this tree, so the tree records
       * which published thing makes it true (`releasedIn`) and the release
       * checklist in `docs/server-env-config.md` owns the freeze.
       */
      readonly released: true;
      /**
       * Evidence for `released`, in the form the API-epoch lane already uses
       * (`desktop-server-api-epoch.ts`: "Precondition checked per §3, not
       * assumed"): the published commit/tag that shipped this epoch, verified when
       * the flag was set rather than assumed. A bare boolean would be a claim with
       * no way to audit it.
       */
      readonly releasedIn: string;
    }
  | {
      readonly epoch: number;
      readonly firstStep: number;
      readonly lastStep: number;
      /**
       * Not shipped yet, so this epoch is where new steps go. Exactly one entry
       * may be unreleased, and it must be the last — two open epochs would let one
       * unshipped build bump the outward number twice.
       */
      readonly released: false;
    };

/**
 * Steps at or below this were shipped one-per-release, so epoch n IS step n
 * there. That history is not rewritten (the ledger rows are real and a rename
 * would invalidate every operator's memory of "we were on 68"); it is simply
 * declared, which is why the manifest below only has to describe the region
 * where the two axes actually diverge.
 */
export const ONE_TO_ONE_EPOCH_CEILING_STEP = 68;

/**
 * The frozen release manifest. One entry per epoch from the point the axes
 * diverge; historical epochs live in the 1:1 region above.
 *
 * To add a schema change:
 *   - epoch not yet released → extend its `lastStep` in the same commit as the
 *     new migration step;
 *   - epoch already released → append a new entry with `released: false`.
 * `assertSchemaEpochManifest` fails loudly if a change does neither, so a build
 * cannot ship a step no epoch claims, an epoch no step implements, or two open
 * epochs at once. Turning an entry to `released: true` is a RELEASE action, not a
 * code-change action — see `SchemaEpochEntry.released` for the line between what
 * the fence proves and what the release checklist owns.
 */
export const SCHEMA_EPOCH_MANIFEST: readonly SchemaEpochEntry[] = [
  {
    epoch: 69,
    firstStep: 69,
    lastStep: 98,
    released: true,
    releasedIn: 'ECS Rust deployment 2026-09-12, e5d5090e8',
  },
  {
    epoch: 70,
    firstStep: 99,
    lastStep: 99,
    released: true,
    releasedIn: 'ECS Rust deployment 2026-09-15, 5504ea577',
  },
  {
    epoch: 71,
    firstStep: 100,
    lastStep: 102,
    released: false,
  },
];

/** The epoch this build ships: the last manifest entry. */
export const CURRENT_SCHEMA_EPOCH: number =
  SCHEMA_EPOCH_MANIFEST[SCHEMA_EPOCH_MANIFEST.length - 1]?.epoch ?? ONE_TO_ONE_EPOCH_CEILING_STEP;

/** First migration step of an epoch, or `null` if no such epoch exists. */
export function firstStepOfEpoch(epoch: number): number | null {
  if (epoch <= 0) return null;
  if (epoch <= ONE_TO_ONE_EPOCH_CEILING_STEP) return epoch;
  return SCHEMA_EPOCH_MANIFEST.find((e) => e.epoch === epoch)?.firstStep ?? null;
}

/**
 * The two floors, kept apart on purpose (#task:2103).
 *
 * They answer different questions and today they happen to coincide at 44, which
 * is exactly why they must not share one constant: the first time support policy
 * moves into a multi-step epoch, a single number would silently publish an
 * internal step as the supported release.
 *
 * `MIN_SUPPORTED_SCHEMA_EPOCH` is the PRODUCT decision — the oldest release this
 * build promises to upgrade from. It is declared on the outward axis because that
 * is the axis the promise is made in ("we upgrade from epoch 68 onwards").
 *
 * `MIN_SUPPORTED_MIGRATION_STEP` is what the runner compares against, DERIVED
 * from that policy. Declaring it separately would make two writers for one
 * decision, and the pair that disagrees is unopenable-but-green.
 *
 * `BASELINE_MIGRATION_STEP` is a third, harder thing: a SAFETY invariant, not a
 * policy. The baseline step is a full CREATE, so a non-empty database below it
 * must be refused rather than rebuilt — no support policy may lower the floor
 * past it, which `assertSchemaEpochManifest` checks.
 */
export const MIN_SUPPORTED_SCHEMA_EPOCH = 44;

const minSupportedStep = firstStepOfEpoch(MIN_SUPPORTED_SCHEMA_EPOCH);
if (minSupportedStep === null) {
  // Fail closed at import: an unknown support epoch means we do not know which
  // databases this build may open, and guessing that is how a pre-baseline DB
  // gets the baseline CREATE run on top of it.
  throw new Error(
    `schema epoch manifest: MIN_SUPPORTED_SCHEMA_EPOCH ${MIN_SUPPORTED_SCHEMA_EPOCH} is not an epoch this build knows`,
  );
}

/** Step floor the runner enforces. Derived from the epoch policy above. */
export const MIN_SUPPORTED_MIGRATION_STEP: number = minSupportedStep;

/**
 * Structural invariants of the manifest, asserted by the static fence
 * (`packages/shared/tests/schema-epochs.test.ts`) and cheap enough to be called
 * anywhere. Throws with a message that says what to do, because the person who
 * trips it is adding a migration, not reading this file.
 *
 * It reads the whole migration plan, not just its last step: coverage has to be
 * an EQUALITY between the steps the manifest claims and the steps the build
 * defines. One-directional checks miss both interesting mistakes — a step no
 * epoch claims and an epoch range claiming a step nobody implements.
 *
 * What it does NOT prove: that a `released: true` epoch was never extended. See
 * `SchemaEpochEntry.released`.
 */
export function assertSchemaEpochManifest(
  opts: {
    readonly manifest?: readonly SchemaEpochEntry[];
    readonly plan?: readonly Pick<Migration, 'version'>[];
  } = {},
): void {
  const manifest = opts.manifest ?? SCHEMA_EPOCH_MANIFEST;
  const plan = opts.plan ?? migrations;
  let previousLastStep = ONE_TO_ONE_EPOCH_CEILING_STEP;
  let previousEpoch = ONE_TO_ONE_EPOCH_CEILING_STEP;
  let unreleased = 0;
  const claimed = new Set<number>();
  for (const entry of manifest) {
    if (entry.epoch !== previousEpoch + 1) {
      throw new Error(
        `schema epoch manifest: epoch ${entry.epoch} must follow ${previousEpoch} without a gap`,
      );
    }
    if (entry.firstStep !== previousLastStep + 1) {
      throw new Error(
        `schema epoch manifest: epoch ${entry.epoch} must start at step ${previousLastStep + 1}, not ${entry.firstStep}`,
      );
    }
    if (entry.lastStep < entry.firstStep) {
      throw new Error(
        `schema epoch manifest: epoch ${entry.epoch} owns no step (firstStep ${entry.firstStep} > lastStep ${entry.lastStep})`,
      );
    }
    if (unreleased > 0 && entry.released) {
      throw new Error(
        `schema epoch manifest: released epoch ${entry.epoch} follows an unreleased one; released epochs are a prefix`,
      );
    }
    if (!entry.released) unreleased += 1;
    if (unreleased > 1) {
      throw new Error(
        `schema epoch manifest: epoch ${entry.epoch} is a second unreleased epoch. ` +
          'One unshipped build may bump the outward epoch at most once, so new steps extend the open epoch instead of opening another.',
      );
    }
    for (let step = entry.firstStep; step <= entry.lastStep; step += 1) {
      claimed.add(step);
    }
    previousEpoch = entry.epoch;
    previousLastStep = entry.lastStep;
  }

  const planSteps = [...new Set(plan.map((m) => m.version))].sort((a, b) => a - b);
  const divergent = planSteps.filter((s) => s > ONE_TO_ONE_EPOCH_CEILING_STEP);
  const unclaimed = divergent.filter((s) => !claimed.has(s));
  if (unclaimed.length > 0) {
    throw new Error(
      `schema epoch manifest: migration step(s) ${unclaimed.join(', ')} belong to no epoch. ` +
        "Extend the unreleased epoch's lastStep in the same commit as the new step, or — if that epoch has shipped — append a new epoch for it.",
    );
  }
  const planSet = new Set(divergent);
  const phantom = [...claimed].filter((s) => !planSet.has(s)).sort((a, b) => a - b);
  if (phantom.length > 0) {
    throw new Error(
      `schema epoch manifest: epoch range(s) claim step(s) ${phantom.join(', ')} that this build does not define. ` +
        'An epoch may only claim steps that exist in the migration plan.',
    );
  }
}

/**
 * Invariants BETWEEN the two floors and the manifest (#task:2103). Separate from
 * `assertSchemaEpochManifest` because these judge the shipped constants, not a
 * caller-supplied shape, and the fence asserts both.
 */
export function assertSupportPolicyFloors(): void {
  if (MIN_SUPPORTED_SCHEMA_EPOCH > CURRENT_SCHEMA_EPOCH) {
    throw new Error(
      `schema epoch support policy: MIN_SUPPORTED_SCHEMA_EPOCH ${MIN_SUPPORTED_SCHEMA_EPOCH} is newer than the epoch this build ships (${CURRENT_SCHEMA_EPOCH})`,
    );
  }
  if (MIN_SUPPORTED_MIGRATION_STEP < BASELINE_MIGRATION_STEP) {
    throw new Error(
      `schema epoch support policy: epoch ${MIN_SUPPORTED_SCHEMA_EPOCH} derives step floor ${MIN_SUPPORTED_MIGRATION_STEP}, ` +
        `below the baseline step ${BASELINE_MIGRATION_STEP}. The baseline migration is a full CREATE, so no support policy may promise to upgrade from under it.`,
    );
  }
}
