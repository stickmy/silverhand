//! Settlement policy: the durable write that settles a claimed fire.
//!
//! One layer above [`super::recurrence`]. A due schedule is claimed
//! (`pending → firing`) by the driver; its wake is then either delivered or
//! it fails. This module decides WHICH terminal/rearm write settles that
//! `firing` row — nothing else. It is pure and driver-free: it returns a
//! [`SettlementAction`]; performing the CAS write, delivering the wake, and
//! the sweep/timer are all separate seams the adapter/driver own.
//!
//! The success plan ([`Advancement`]) is computed by the driver BEFORE
//! delivery (the wake body needs its next-instant / missed-count) and is
//! reused verbatim on the delivered path. On the failed path the success plan
//! is irrelevant:
//!   - `once` has no future fire to retry → terminal [`SettlementAction::MarkFailed`].
//!   - `cron|interval` must not die on a single bad tick →
//!     [`plan_failure_rearm`] decides between natural plan-end (`until_at`
//!     overrun → [`SettlementAction::MarkPlanEndedAfterFailure`]: terminal
//!     `done`, but the failed fire is recorded honestly rather than counted as
//!     a success) and re-arm on the next cadence tick
//!     ([`SettlementAction::MarkFireFailed`]). `max_occurrences` is NOT
//!     consulted for a failed fire — the failed fire consumed no occurrence.
//!
//! The checked-in `settlement_ts_fixture.rs` pins these decisions.

use super::recurrence::{
    Advancement, CronNextOccurrence, FailureRearm, FailureRearmError, plan_failure_rearm,
};
use super::spec::{ScheduleCaps, ScheduleSpec};

/// The result of attempting the wake delivery for a claimed fire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    Delivered,
    Failed { reason: String },
}

/// The durable write that settles a claimed (`firing`) fire. Each maps 1:1 to
/// a `status='firing'`-predicated CAS in the schedule store:
///   - [`MarkDone`](Self::MarkDone)         → firing→done    (terminal SUCCESS: delivered final fire)
///   - [`Rearm`](Self::Rearm)               → firing→pending (repeat success; advance to `next_fire_at`)
///   - [`MarkFailed`](Self::MarkFailed)     → firing→failed  (one-shot delivery failure; no retry)
///   - [`MarkFireFailed`](Self::MarkFireFailed) → firing→pending (repeat delivery failure; record + re-arm)
///   - [`MarkPlanEndedAfterFailure`](Self::MarkPlanEndedAfterFailure) → firing→done
///     (repeat delivery FAILED and the next tick is past the plan window: the
///     plan is over so the terminal state is `done`, but the fire did not
///     succeed — recorded honestly, without counting an occurrence)
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementAction {
    MarkDone,
    Rearm { next_fire_at: i64 },
    MarkFailed { reason: String },
    MarkFireFailed { reason: String, next_fire_at: i64 },
    MarkPlanEndedAfterFailure { reason: String },
}

/// Decide the settlement write for a claimed fire. Pure: no store, no clock.
///
/// `advancement` is the pre-computed success plan (see module docs). Errors
/// only on the failed-repeating path, propagated from [`plan_failure_rearm`]
/// (a cadence fault routes to fire-failure; a policy fault to a safe-500) — the
/// delivered path never computes cadence and so never errors here.
pub fn plan_settlement<C: CronNextOccurrence>(
    spec: &ScheduleSpec,
    caps: ScheduleCaps,
    actual_fire_at: i64,
    advancement: &Advancement,
    outcome: DeliveryOutcome,
    cron: &C,
) -> Result<SettlementAction, FailureRearmError> {
    let reason = match outcome {
        DeliveryOutcome::Delivered => {
            return Ok(match advancement {
                Advancement::Terminal { .. } => SettlementAction::MarkDone,
                Advancement::Rearm { next_fire_at, .. } => SettlementAction::Rearm {
                    next_fire_at: *next_fire_at,
                },
            });
        }
        DeliveryOutcome::Failed { reason } => reason,
    };
    if !spec.is_repeating() {
        return Ok(SettlementAction::MarkFailed { reason });
    }
    match plan_failure_rearm(spec, caps, actual_fire_at, cron)? {
        FailureRearm::Terminal => Ok(SettlementAction::MarkPlanEndedAfterFailure { reason }),
        FailureRearm::Rearm { next_fire_at } => Ok(SettlementAction::MarkFireFailed {
            reason,
            next_fire_at,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::recurrence::{CronNextOccurrence, CronOutcome, CronQuery};
    use super::super::spec::CronSpec;
    use super::*;

    struct FixedCron(CronOutcome);
    impl CronNextOccurrence for FixedCron {
        fn next_after(&self, _query: CronQuery<'_>) -> CronOutcome {
            self.0
        }
    }

    fn cron() -> ScheduleSpec {
        ScheduleSpec::Cron(CronSpec::new("*/5 * * * *".into(), "UTC".into()))
    }

    #[test]
    fn delivered_rearm_carries_the_planned_next_instant() {
        struct UnreachableCron;
        impl CronNextOccurrence for UnreachableCron {
            fn next_after(&self, _query: CronQuery<'_>) -> CronOutcome {
                panic!("a delivered fire must reuse its pre-delivery plan");
            }
        }
        let advancement = Advancement::Rearm {
            next_fire_at: 777_000,
            missed_count: 0,
        };
        let action = plan_settlement(
            &cron(),
            ScheduleCaps::none(),
            100_000,
            &advancement,
            DeliveryOutcome::Delivered,
            &UnreachableCron,
        )
        .unwrap();
        assert_eq!(
            action,
            SettlementAction::Rearm {
                next_fire_at: 777_000
            }
        );
    }

    #[test]
    fn failed_once_marks_failed_and_never_consults_cadence() {
        // `once` failure is terminal; the cron port must not be reached (a
        // faulting evaluator would surface as Err if it were consulted).
        let advancement = Advancement::Terminal { missed_count: 0 };
        let action = plan_settlement(
            &ScheduleSpec::Once { fire_at: 1_000 },
            ScheduleCaps::none(),
            5_000,
            &advancement,
            DeliveryOutcome::Failed {
                reason: "boom".into(),
            },
            &FixedCron(CronOutcome::InvalidExpression),
        )
        .unwrap();
        assert_eq!(
            action,
            SettlementAction::MarkFailed {
                reason: "boom".into()
            }
        );
    }

    #[test]
    fn failed_repeating_cron_fault_propagates() {
        let advancement = Advancement::Rearm {
            next_fire_at: 0,
            missed_count: 0,
        };
        let err = plan_settlement(
            &cron(),
            ScheduleCaps::none(),
            10_000,
            &advancement,
            DeliveryOutcome::Failed {
                reason: "boom".into(),
            },
            &FixedCron(CronOutcome::InvalidExpression),
        )
        .unwrap_err();
        assert!(matches!(err, FailureRearmError::Advance(_)));
    }
}

/// Frozen cross-language settlement contract, generated by the production
/// TypeScript decision before its retirement in af0e2b6b6. Rust computes the
/// pre-delivery plan and settlement against those independent expected actions;
/// there is no live TypeScript signer in this repository.
#[cfg(test)]
mod ts_parity {
    use super::super::recurrence::plan_advancement;
    use super::super::recurrence::{CronNextOccurrence, CronOutcome, CronQuery};
    use super::super::spec::{CronSpec, FiredCount, IntervalSeconds};
    use super::*;

    #[derive(Clone, Copy)]
    enum Kind {
        Once { fire_at: i64 },
        Interval { every_seconds: i64 },
        Cron { step: i64 },
    }

    #[derive(Clone, Copy)]
    enum ExpAction {
        MarkDone,
        Rearm { next: i64 },
        MarkFailed { reason: &'static str },
        MarkFireFailed { reason: &'static str, next: i64 },
        MarkPlanEndedAfterFailure { reason: &'static str },
    }

    struct Case {
        name: &'static str,
        kind: Kind,
        max_occ: Option<i64>,
        until_at: Option<i64>,
        fired_count: i64,
        scheduled_fire_at: i64,
        actual_fire_at: i64,
        delivered: bool,
        reason: &'static str,
        exp_action: ExpAction,
    }

    struct SteppingCron {
        step_ms: i64,
    }
    impl CronNextOccurrence for SteppingCron {
        fn next_after(&self, query: CronQuery<'_>) -> CronOutcome {
            CronOutcome::NextInstant((query.after / self.step_ms + 1) * self.step_ms)
        }
    }

    const CASES: &[Case] = include!("settlement_ts_fixture.rs");

    #[test]
    fn matches_ts_signed_settlement_fixture() {
        for c in CASES {
            let (spec, step) = match c.kind {
                Kind::Once { fire_at } => (ScheduleSpec::Once { fire_at }, 1),
                Kind::Interval { every_seconds } => (
                    ScheduleSpec::Interval {
                        every: IntervalSeconds::new(every_seconds).unwrap(),
                        start_at: None,
                    },
                    1,
                ),
                Kind::Cron { step } => (
                    ScheduleSpec::Cron(CronSpec::new("*/5 * * * *".into(), "UTC".into())),
                    step,
                ),
            };
            let caps = ScheduleCaps::new(c.max_occ, c.until_at).unwrap();
            let cron = SteppingCron { step_ms: step };

            // The driver computes the success plan before delivery; mirror that.
            let advancement = plan_advancement(
                &spec,
                caps,
                FiredCount::new(c.fired_count).unwrap(),
                c.scheduled_fire_at,
                c.actual_fire_at,
                &cron,
            )
            .unwrap_or_else(|e| panic!("{}: advancement errored: {e:?}", c.name));

            let outcome = if c.delivered {
                DeliveryOutcome::Delivered
            } else {
                DeliveryOutcome::Failed {
                    reason: c.reason.to_string(),
                }
            };
            let action =
                plan_settlement(&spec, caps, c.actual_fire_at, &advancement, outcome, &cron)
                    .unwrap_or_else(|e| panic!("{}: settlement errored: {e:?}", c.name));

            let expected = match c.exp_action {
                ExpAction::MarkDone => SettlementAction::MarkDone,
                ExpAction::Rearm { next } => SettlementAction::Rearm { next_fire_at: next },
                ExpAction::MarkFailed { reason } => SettlementAction::MarkFailed {
                    reason: reason.to_string(),
                },
                ExpAction::MarkFireFailed { reason, next } => SettlementAction::MarkFireFailed {
                    reason: reason.to_string(),
                    next_fire_at: next,
                },
                ExpAction::MarkPlanEndedAfterFailure { reason } => {
                    SettlementAction::MarkPlanEndedAfterFailure {
                        reason: reason.to_string(),
                    }
                }
            };
            assert_eq!(action, expected, "{}: settlement action", c.name);
        }
    }
}
