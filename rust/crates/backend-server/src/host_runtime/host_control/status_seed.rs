//! Process-local host status seed, isolated per host connection generation.
//!
//! Concurrent requests for one connection generation collapse into a single in-flight
//! attempt. A new connection reuses neither the previous one's success nor its
//! position in the retry ladder. Failure is never cached: a refused attempt leaves
//! nothing a later read could be served from, and the deployed ladder is
//! `[1, 2, 5, 10, 30]` seconds.
//!
//! This leaf's one change reason is *when to ask, how often, and what to keep* — not
//! what the question means, which is [`crate::host_runtime::host_control::status_snapshot`]'s. It
//! therefore holds no timer: the ladder is an admission gate on the next attempt
//! ([`SeedAttempt::Deferred`] names the delay the driver must wait), so the policy
//! stays here and the sleeping stays in whichever driver owns the wake. A driver
//! cannot invent a delay this leaf did not hand it, and a punctual driver is not
//! trusted either — an attempt that arrives early is suppressed rather than sent.
//!
//! Because the sleeping is external, the wake is named rather than assumed. Every
//! deferral hands out a [`RetryTicket`] that only this leaf can mint, and a retry
//! must present the exact one outstanding; the first admission consumes it under the
//! same lock that admits. So what is guaranteed here is *one valid wake identity and
//! one in-flight attempt per `(host, connection generation)`* — not one external sleeper. A driver
//! may hold any number of timers for the same ticket, and every wake but one settles
//! as [`SeedAttempt::StaleWake`] without a frame. Collapsing those timers is a
//! resource concern for whoever owns them; correctness does not rest on it, and this
//! leaf assumes nothing about a driver staying alive to honour a delay it accepted.
//!
//! Reading is synchronous and dispatches nothing, so the HTTP read surface performs
//! no daemon round trip. Three read arms, and the middle one is why they are three:
//! a host with no attached daemon is *absent* ([`SeedRead::Disconnected`], answered
//! from its own `last_seen_at` upstream), while an attached host whose seed has not
//! landed is [`SeedRead::NotSeeded`] — not a fault, and the deployed read surface
//! projects it as `unreachable` / `daemon_unreachable`. The probe's own failure
//! reason is deliberately not carried into either arm: by the time a read happens
//! that reason describes a past attempt, and keeping it would be exactly the
//! negative cache this contract forbids.
//!
//! The production driver owns sleeping and the post-seal invalidation. It consumes
//! every [`SeedAttempt::Deferred`] ticket through the bounded retry ladder, stops a
//! chain when its exact connection generation is replaced, and publishes
//! `host_connection_changed` only after [`SeedAttempt::Sealed`]. A publish failure
//! is post-seed residue: it cannot undo the already sealed memory fact.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use silverhand_backend_core::foundation::identity::{AgentId, DaemonInstanceId, HostId};
use tokio::time::Instant;

use crate::{
    foundation::daemon_transport::{HostDataPlaneCustody, OwningHostDataPlane},
    host_runtime::host_control::status_snapshot::{
        self, MachineFacts, StatusSnapshot, StatusSnapshotOutcome, StatusSnapshotRequest,
        UnreachableReason,
    },
};

impl DaemonCustody for OwningHostDataPlane {
    fn attached_custody(
        &self,
        host: &HostId,
    ) -> Result<Option<AttachedStatusCustody>, CustodyUnavailable> {
        Ok(self
            .resolve(host)
            .map_err(|_| CustodyUnavailable)?
            .map(|custody| AttachedStatusCustody {
                daemon_instance: custody.daemon_instance().clone(),
                connection_id: custody.connection_id().to_owned(),
            }))
    }

    fn probe_status<'a>(
        &'a self,
        host: HostId,
        custody: AttachedStatusCustody,
    ) -> Pin<Box<dyn Future<Output = StatusSnapshotOutcome> + Send + 'a>> {
        Box::pin(async move {
            let custody = HostDataPlaneCustody::captured(
                host,
                custody.daemon_instance,
                custody.connection_id,
            );
            status_snapshot::probe(self, StatusSnapshotRequest::new(custody)).await
        })
    }
}

/// The deployed ladder. Six attempts, five waits, ~48s of patience, and then this
/// custody stays unseeded for the rest of its connection generation: a bounded
/// schedule is the whole point, so there is no terminal repeat. A newly admitted
/// connection id is a new generation with its own ladder.
const SEED_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// What the seed layer needs of the daemon transport, declared here because this is
/// where it is consumed: the two facts move together, so a caller cannot hand this
/// leaf a probe and a custody answer that disagree, and the leaf itself cannot reach
/// past the port for a raw command.
pub(crate) trait DaemonCustody: Send + Sync {
    /// Which exact daemon connection holds this host right now — the *attached* custody, not
    /// a reserved slot. A socket that has not proven its protocol has no facts to be
    /// asked for, and a seed filed against it would be filed against nothing.
    fn attached_custody(
        &self,
        host: &HostId,
    ) -> Result<Option<AttachedStatusCustody>, CustodyUnavailable>;

    /// Ask one named custody for its status. What the answer means is the snapshot
    /// leaf's contract; this leaf only decides whether to ask and what to keep.
    ///
    /// Boxed rather than returned as an opaque future so the store can hold one
    /// erased custody. A type parameter here would buy nothing — there is one
    /// transport — while forcing this port into the public bound of a public type,
    /// which is to say publishing a port no caller outside this crate may use. The
    /// allocation costs one per frame, and a frame leaves at most six times per
    /// attachment.
    fn probe_status<'a>(
        &'a self,
        host: HostId,
        custody: AttachedStatusCustody,
    ) -> Pin<Box<dyn Future<Output = StatusSnapshotOutcome> + Send + 'a>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AttachedStatusCustody {
    pub(crate) daemon_instance: DaemonInstanceId,
    pub(crate) connection_id: String,
}

pub(crate) trait HostRuntimeActivity: Send + Sync {
    /// `None` means this exact connection has not completed hydration yet, so the
    /// seed answer remains the pre-hydrate fallback. `Some` is the current
    /// event-fed active set, including an intentionally empty set.
    fn active_agents(
        &self,
        host: &HostId,
        custody: &AttachedStatusCustody,
    ) -> Result<Option<HashSet<AgentId>>, CustodyUnavailable>;
}

/// The custody authority cannot be read right now.
#[derive(Debug)]
pub(crate) struct CustodyUnavailable;

/// Why a seed layer call produced no answer at all. Neither arm is a fact about a
/// daemon, so neither may be rendered as one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeedFailure {
    /// Process-local seed state, or the custody authority behind it, is unreadable.
    Unavailable,
    /// An answer does not describe the coordinate it was asked about. No daemon
    /// caused it and no operator can act on it: it is a defect in this crate, and
    /// the answer is dropped rather than filed under the key it was asked for.
    Misattributed,
    /// This process has minted every wake and attempt identity it can. Non-reuse is
    /// the whole guarantee those identities carry, so the store stops handing them out
    /// rather than reissuing one a dead generation already holds.
    IdentityExhausted,
}

impl fmt::Display for SeedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "host status seed state is unavailable",
            Self::Misattributed => "host status answer does not describe the host it was asked for",
            Self::IdentityExhausted => "host status seed identities are exhausted",
        })
    }
}

impl Error for SeedFailure {}

impl From<CustodyUnavailable> for SeedFailure {
    fn from(_: CustodyUnavailable) -> Self {
        Self::Unavailable
    }
}

/// Names the one wake a `(host, connection generation)` is currently waiting for.
///
/// Minted only by this leaf and opaque on purpose: a driver can present a ticket it
/// was handed, and nothing else. The value is unique across the whole store, so a
/// ticket minted for one daemon instance can never be the one a later instance is
/// waiting for — the identity is what distinguishes generations, without a generation
/// having to be spelled inside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryTicket(u64);

/// Identifies one in-flight attempt so that its claim can be released by whoever gets
/// there first without ever releasing a successor's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AttemptId(u64);

/// What one attempt settled. Only [`Self::Sealed`] is news; the rest say "nothing
/// more from this attempt" and differ only in why, which is what a test and an
/// operator reading a log can hold this leaf to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeedAttempt {
    /// Facts landed for this custody. A reader that saw [`SeedRead::NotSeeded`] has
    /// something to be told about.
    Sealed,
    /// This custody already had its seed, and no frame was sent.
    AlreadySealed,
    /// Another attempt for this exact custody is in flight, and no second frame was
    /// sent. Its outcome is the one that counts.
    AlreadyInFlight,
    /// This custody is not the one attached now, so the chain is over. Its successor
    /// starts its own on its own attach; nothing is inherited and nothing is left
    /// behind.
    Superseded,
    /// This wake named a ticket that is no longer the outstanding one — it was
    /// already consumed, or its instance has been replaced. No frame was sent and
    /// nothing was advanced. Duplicate wakes for one deferral are expected and land
    /// here; only the first is the continuation.
    StaleWake,
    /// No seed. The next attempt is admissible after `after` has passed and only to a
    /// caller presenting `ticket`, and nothing about the failure is readable in the
    /// meantime. Re-arming an existing deferral yields the remaining delay and the
    /// same ticket, so an attach can safely repeat this without adding a chain.
    Deferred {
        after: Duration,
        ticket: RetryTicket,
    },
    /// No seed and the ladder is spent. Reads stay unseeded until this host is held
    /// by a new connection generation.
    Exhausted,
}

/// Result of an explicit refresh. Only `Refreshed` changed the store; a following
/// [`HostStatusSeeds::read`] serves the new facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    Refreshed,
    Disconnected,
    Unreachable(UnreachableReason),
}

/// What the synchronous read surface can be told without asking a daemon anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SeedRead {
    Seeded(SeededStatus),
    /// No daemon is attached. Nothing is discarded by the read itself; a later attach
    /// carries a new generation and replaces the old entry at admission.
    Disconnected,
    /// A daemon is attached and its facts have not landed yet — or its ladder ran
    /// out. Never a fabricated value, and never the reason a past attempt failed.
    NotSeeded,
}

/// One custody's facts as a reader may serve them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededStatus {
    /// The custody the facts came from, so a consumer holding its own per-custody
    /// state can tell whether the two describe the same daemon.
    pub daemon_instance: DaemonInstanceId,
    pub os: String,
    pub os_arch: Option<String>,
    pub daemon_pid: i64,
    pub daemon_version: String,
    pub started_at: i64,
    /// The daemon's reading at answer time plus how long the seed has been held,
    /// measured on a monotonic clock. The daemon's clock is therefore read once and
    /// never compared to this server's, so neither NTP jump can warp the figure, and
    /// a monotonic hold cannot run backwards.
    pub uptime_ms: i64,
    /// Machine facts; `system_uptime_ms_at_answer` is advanced by the hold like
    /// `uptime_ms`.
    pub machine: Box<MachineFacts>,
    /// The seed answer is the pre-hydrate fallback. Once the existing runtime-event
    /// owner has a current-generation hydrated set, [`HostStatusSeeds::read`]
    /// replaces this field with that set; it never unions two authorities.
    pub active_agents: HashSet<AgentId>,
}

struct Sealed {
    snapshot: StatusSnapshot,
    held_since: Instant,
}

/// A deferral that has been handed out: when the next attempt becomes admissible, and
/// the one ticket that may claim it. The two are one field because a deadline without
/// the ticket that satisfies it would be a wake nobody could own.
struct Schedule {
    not_before: Instant,
    ticket: RetryTicket,
}

/// One host's entry, for exactly one custody. The readable half and the retry half
/// live side by side and never trade: [`HostStatusSeeds::read`] touches `sealed` and
/// nothing else, so no count and no deadline can become a value a reader is served.
struct SeedState {
    instance: DaemonInstanceId,
    connection_id: String,
    sealed: Option<Sealed>,
    in_flight: Option<AttemptId>,
    failed: usize,
    next: Option<Schedule>,
}

impl SeedState {
    fn new(instance: DaemonInstanceId, connection_id: String) -> Self {
        Self {
            instance,
            connection_id,
            sealed: None,
            in_flight: None,
            failed: 0,
            next: None,
        }
    }

    fn exhausted(&self) -> bool {
        self.failed > SEED_RETRY_DELAYS.len()
    }
}

/// The entries and the single source of identity behind them.
///
/// One counter mints both tickets and attempt ids, which is what makes a ticket unique
/// across generations: a per-entry counter would restart with the entry, so the ticket
/// a replaced instance handed out would validate against its successor's first
/// deferral.
///
/// Its lock is also where every custody observation in this leaf happens, and that is a
/// weaker claim than it may look: it does not make a registry attach and a seed mutation
/// one atomic transaction — the registry is a separate authority with a lock of its own.
/// What it gives is a *total order over seed mutations*, with each mutation re-observing
/// the live authority inside its own critical section. Two orderings follow, and between
/// them a destructive replacement is always taken against a custody that is current:
///
///   * A mutation that reaches the critical section after a successor's has already
///     written observes that successor or a later one, so it cannot delete it.
///   * If custody changes after a mutation's observation, the successor's own mutation
///     can only be ordered after that one, and it then replaces the entry from the live
///     answer it observes.
///
/// No ordering of the attaches themselves is needed for this, which is why no generation
/// number has to travel with anything. Note what is *not* claimed: an observation is not
/// a lasting fact, so nothing here may be read once and then acted on later.
struct SeedStore {
    states: HashMap<String, SeedState>,
    next_id: u64,
}

/// The next identity, or none at all. Monotonic and never reused is the whole guarantee
/// both newtypes rest on, so exhaustion is a named failure rather than something the
/// build profile decides: an overflow would panic in debug and, in release, hand a live
/// generation a value a dead one is still holding.
fn take_id(next_id: &mut u64) -> Result<u64, SeedFailure> {
    let id = next_id
        .checked_add(1)
        .ok_or(SeedFailure::IdentityExhausted)?;
    *next_id = id;
    Ok(id)
}

/// The claim one attempt holds on a `(host, connection generation)` while its frame is out.
///
/// Released exactly once, by whichever comes first: the attempt settling, or the future
/// being dropped before it could. A cancelled attempt learned nothing, so dropping
/// advances no ladder and restores no ticket — it only stops standing in the way of the
/// next attach, which is the recovery path when the task that owned the frame is gone.
/// The release is conditional on the claim still being this attempt's, so a drop that
/// happens after a successor was admitted cannot free the successor.
struct AttemptGuard {
    store: Arc<Mutex<SeedStore>>,
    host: String,
    instance: DaemonInstanceId,
    connection_id: String,
    id: AttemptId,
    released: bool,
}

impl AttemptGuard {
    fn custody(&self) -> AttachedStatusCustody {
        AttachedStatusCustody {
            daemon_instance: self.instance.clone(),
            connection_id: self.connection_id.clone(),
        }
    }

    /// Clears this attempt's claim in an already-locked store and hands back the entry
    /// it belongs to, or nothing at all if the claim is no longer this attempt's.
    fn release_in<'a>(
        &mut self,
        states: &'a mut HashMap<String, SeedState>,
    ) -> Option<&'a mut SeedState> {
        self.released = true;
        let state = states.get_mut(&self.host)?;
        if state.instance != self.instance
            || state.connection_id != self.connection_id
            || state.in_flight != Some(self.id)
        {
            return None;
        }
        state.in_flight = None;
        Some(state)
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let store = Arc::clone(&self.store);
        if let Ok(mut store) = store.lock() {
            self.release_in(&mut store.states);
        }
    }
}

enum Admission {
    Probe(AttemptGuard),
    Settled(SeedAttempt),
}

/// The seeds, their retry schedule, and the rule for when a probe may be sent.
///
/// Cloning shares one store: every handle a composition holds reads and writes the
/// same seeds, so there is no arrangement in which a host is answered from a second,
/// emptier one. Two independent facts hold that, and each has its own gate below,
/// because either one alone would make the other look load-bearing when it is not.
///
/// ```
/// use silverhand_backend_core::foundation::identity::HostId;
/// use silverhand_backend_server::{
///     application_orchestration::daemon_delivery::DaemonDeliveryController,
///     host_runtime::host_control::status_seed::{HostStatusSeeds, SeedFailure, SeedRead},
/// };
///
/// fn current(seeds: &HostStatusSeeds, host: &HostId) -> Result<SeedRead, SeedFailure> {
///     let _ = DaemonDeliveryController::host_status_seeds;
///     seeds.read(host)
/// }
/// ```
///
/// First: the custody port is crate-internal, so the argument a store is built from
/// cannot be produced outside. The adapter is wired in this crate, never handed in.
///
/// ```compile_fail
/// use silverhand_backend_server::host_runtime::host_control::status_seed::DaemonCustody;
/// ```
///
/// Second, and what still holds if the line above is ever deliberately relaxed: the
/// constructor is crate-internal too. The type is public and the first doctest reads
/// through it; naming the constructor is the only thing asked for here, and a second
/// store is the only thing it would build.
///
/// ```compile_fail
/// use silverhand_backend_server::host_runtime::host_control::status_seed::HostStatusSeeds;
///
/// fn second_store() {
///     let _ = HostStatusSeeds::new;
/// }
/// ```
#[derive(Clone)]
pub struct HostStatusSeeds {
    store: Arc<Mutex<SeedStore>>,
    custody: Arc<dyn DaemonCustody>,
    activity: Arc<dyn HostRuntimeActivity>,
}

impl HostStatusSeeds {
    pub(crate) fn new(
        custody: Arc<dyn DaemonCustody>,
        activity: Arc<dyn HostRuntimeActivity>,
    ) -> Self {
        Self {
            store: Arc::new(Mutex::new(SeedStore {
                states: HashMap::new(),
                next_id: 0,
            })),
            custody,
            activity,
        }
    }

    /// Memory only, and synchronous so that it cannot grow a dispatch. A seed is
    /// served only for the custody attached at this moment; anything else is either
    /// an absent machine or one whose facts have not landed.
    ///
    /// Nothing is written here, not even a cleanup. A stored entry that belongs to a
    /// past generation is simply not this custody's seed, and saying so is the whole
    /// job — which leaves the admission edge as the only writer that ever replaces an
    /// entry, so what may displace a live custody's seed has one place to be proven
    /// rather than two.
    pub fn read(&self, host: &HostId) -> Result<SeedRead, SeedFailure> {
        let store = self.store.lock().map_err(|_| SeedFailure::Unavailable)?;
        // Observed from inside this critical section, like every other custody
        // observation in this leaf, so the answer a decision is taken from cannot have
        // been overtaken by a mutation that ran between the two.
        let Some(attached) = self.custody.attached_custody(host)? else {
            // Absence is not a reason to forget. The entry belongs to a daemon
            // instance, and this host has no attached one to answer for right now.
            return Ok(SeedRead::Disconnected);
        };
        let Some(sealed) = store
            .states
            .get(host.as_str())
            .filter(|state| {
                state.instance == attached.daemon_instance
                    && state.connection_id == attached.connection_id
            })
            .and_then(|state| state.sealed.as_ref())
        else {
            return Ok(SeedRead::NotSeeded);
        };
        // The stored coordinate is the gate, not the map key: a seed can only be
        // read by naming the host and custody it was answered on.
        let facts = sealed
            .snapshot
            .facts_for(host, &attached.daemon_instance)
            .map_err(|_| SeedFailure::Misattributed)?;
        let held = Instant::now().saturating_duration_since(sealed.held_since);
        let held_ms = i64::try_from(held.as_millis()).unwrap_or(i64::MAX);
        let mut machine = Box::new(facts.machine.clone());
        machine.system_uptime_ms_at_answer = machine
            .system_uptime_ms_at_answer
            .map(|uptime| uptime.saturating_add(held_ms));
        Ok(SeedRead::Seeded(SeededStatus {
            daemon_instance: attached.daemon_instance.clone(),
            os: facts.os.to_owned(),
            os_arch: facts.os_arch.map(str::to_owned),
            daemon_pid: facts.daemon_pid,
            daemon_version: facts.daemon_version.to_owned(),
            started_at: facts.started_at,
            uptime_ms: facts.uptime_ms_at_answer.saturating_add(held_ms),
            machine,
            active_agents: self
                .activity
                .active_agents(host, &attached)?
                .unwrap_or_else(|| facts.active_agents.clone()),
        }))
    }

    /// Ask the attached daemon now and, on an answer, replace its seed. A failure
    /// leaves the existing seed and ladder untouched (no negative cache).
    pub async fn refresh(&self, host: &HostId) -> Result<RefreshOutcome, SeedFailure> {
        let attached = {
            let _store = self.store.lock().map_err(|_| SeedFailure::Unavailable)?;
            self.custody.attached_custody(host)?
        };
        let Some(attached) = attached else {
            return Ok(RefreshOutcome::Disconnected);
        };
        let instance = attached.daemon_instance.clone();
        let snapshot = match self
            .custody
            .probe_status(host.clone(), attached.clone())
            .await
        {
            StatusSnapshotOutcome::Answered(snapshot) => snapshot,
            StatusSnapshotOutcome::Disconnected => return Ok(RefreshOutcome::Disconnected),
            StatusSnapshotOutcome::Unreachable(reason) => {
                return Ok(RefreshOutcome::Unreachable(reason));
            }
            StatusSnapshotOutcome::TransportContractViolation => {
                return Err(SeedFailure::Misattributed);
            }
        };
        if snapshot.facts_for(host, &instance).is_err() {
            return Err(SeedFailure::Misattributed);
        }
        let mut store = self.store.lock().map_err(|_| SeedFailure::Unavailable)?;
        // Re-observed under the lock: an answer from a replaced custody is dropped.
        if self.custody.attached_custody(host)?.as_ref() != Some(&attached) {
            return Ok(RefreshOutcome::Disconnected);
        }
        let state = store
            .states
            .entry(host.as_str().to_owned())
            .and_modify(|state| {
                if state.instance != attached.daemon_instance
                    || state.connection_id != attached.connection_id
                {
                    *state = SeedState::new(instance.clone(), attached.connection_id.clone());
                }
            })
            .or_insert_with(|| SeedState::new(instance.clone(), attached.connection_id.clone()));
        state.sealed = Some(Sealed {
            snapshot,
            held_since: Instant::now(),
        });
        state.failed = 0;
        state.next = None;
        Ok(RefreshOutcome::Refreshed)
    }

    /// A daemon just attached: seed it now, or re-arm whatever this custody was
    /// already waiting for. An attach carries no ticket because it is not the
    /// continuation of anything. A new connection id replaces the prior ladder and
    /// seal even when the daemon instance is unchanged.
    pub async fn seed_on_attach(
        &self,
        host: &HostId,
        instance: &DaemonInstanceId,
    ) -> Result<SeedAttempt, SeedFailure> {
        self.attempt(host, instance, None).await
    }

    /// The continuation of a deferral this leaf handed out. The exact ticket must come
    /// back, and only the first wake presenting it continues; arriving before the delay
    /// has passed is suppressed rather than sent.
    pub async fn retry(
        &self,
        host: &HostId,
        instance: &DaemonInstanceId,
        ticket: RetryTicket,
    ) -> Result<SeedAttempt, SeedFailure> {
        self.attempt(host, instance, Some(ticket)).await
    }

    async fn attempt(
        &self,
        host: &HostId,
        instance: &DaemonInstanceId,
        wake: Option<RetryTicket>,
    ) -> Result<SeedAttempt, SeedFailure> {
        // Held across the await on purpose: if this future is dropped instead of
        // polled to completion, the guard releases the claim it took.
        let guard = match self.admit(host, instance, wake)? {
            Admission::Settled(attempt) => return Ok(attempt),
            Admission::Probe(guard) => guard,
        };
        let admitted_custody = guard.custody();
        let outcome = self
            .custody
            .probe_status(host.clone(), admitted_custody)
            .await;
        self.settle(host, instance, guard, outcome)
    }

    fn admit(
        &self,
        host: &HostId,
        instance: &DaemonInstanceId,
        wake: Option<RetryTicket>,
    ) -> Result<Admission, SeedFailure> {
        let mut store = self.store.lock().map_err(|_| SeedFailure::Unavailable)?;
        // Observed here rather than before the lock, because what follows deletes a live
        // entry. The same question asked outside would be a snapshot, and a snapshot
        // needs the attaches to be ordered before it can license a deletion; asked from
        // inside the critical section that does the deleting, it needs no ordering at
        // all — it is the current custody by the time the entry moves.
        let Some(attached) = self.custody.attached_custody(host)? else {
            return Ok(Admission::Settled(SeedAttempt::Superseded));
        };
        if &attached.daemon_instance != instance {
            return Ok(Admission::Settled(SeedAttempt::Superseded));
        }
        let SeedStore { states, next_id } = &mut *store;
        let key = host.as_str();
        // An entry belongs to one custody. A different one means this host's daemon
        // process was replaced, and its successor inherits neither the facts nor the
        // position in the ladder. This is the only edge in this leaf that replaces an
        // entry, and the caller has just been confirmed as the custody that holds the
        // host, so what is cleared here is always a generation that is already gone.
        if wake.is_some()
            && states.get(key).is_some_and(|state| {
                state.instance != attached.daemon_instance
                    || state.connection_id != attached.connection_id
            })
        {
            return Ok(Admission::Settled(SeedAttempt::Superseded));
        }
        if wake.is_none()
            && states.get(key).is_some_and(|state| {
                state.instance != attached.daemon_instance
                    || state.connection_id != attached.connection_id
            })
        {
            states.remove(key);
        }
        let state = states.entry(key.to_owned()).or_insert_with(|| {
            SeedState::new(
                attached.daemon_instance.clone(),
                attached.connection_id.clone(),
            )
        });
        if let Some(ticket) = wake {
            // A wake speaks for one deferral. Anything else is a duplicate, a late
            // arrival, or a ticket from a generation that no longer exists.
            if state.next.as_ref().map(|schedule| schedule.ticket) != Some(ticket) {
                return Ok(Admission::Settled(SeedAttempt::StaleWake));
            }
        }
        if state.sealed.is_some() {
            return Ok(Admission::Settled(SeedAttempt::AlreadySealed));
        }
        if state.in_flight.is_some() {
            return Ok(Admission::Settled(SeedAttempt::AlreadyInFlight));
        }
        if state.exhausted() {
            return Ok(Admission::Settled(SeedAttempt::Exhausted));
        }
        if let Some(schedule) = &state.next {
            let now = Instant::now();
            if now < schedule.not_before {
                // The outstanding ticket is handed back rather than replaced, so an
                // attach re-arming this deferral cannot outbid the wake already owed.
                return Ok(Admission::Settled(SeedAttempt::Deferred {
                    after: schedule.not_before.saturating_duration_since(now),
                    ticket: schedule.ticket,
                }));
            }
        }
        // Minted before anything is written: a store that can no longer name an attempt
        // must leave the deferral it was about to consume exactly where it was.
        let id = AttemptId(take_id(next_id)?);
        // Admission consumes the ticket under the same lock that grants it, so a second
        // wake naming it finds nothing to continue.
        state.next = None;
        state.in_flight = Some(id);
        Ok(Admission::Probe(AttemptGuard {
            store: Arc::clone(&self.store),
            host: key.to_owned(),
            instance: instance.clone(),
            connection_id: attached.connection_id,
            id,
            released: false,
        }))
    }

    fn settle(
        &self,
        host: &HostId,
        instance: &DaemonInstanceId,
        mut guard: AttemptGuard,
        outcome: StatusSnapshotOutcome,
    ) -> Result<SeedAttempt, SeedFailure> {
        let mut store = self.store.lock().map_err(|_| SeedFailure::Unavailable)?;
        let SeedStore { states, next_id } = &mut *store;
        // An attempt writes only into the entry that admitted it, and only while that
        // entry still holds this attempt's claim. Otherwise the entry is gone, a
        // successor took the host, or this attempt was already released — and this
        // answer has nowhere it belongs.
        let Some(state) = guard.release_in(states) else {
            return Ok(SeedAttempt::Superseded);
        };
        // Observed here, after the claim is given up and before anything is written: an
        // answer from a custody that no longer holds the host may neither be stored nor
        // restore a ladder, and asking from inside this critical section is what makes
        // the answer current rather than a snapshot taken while the lock was open.
        let Some(attached) = self.custody.attached_custody(host)? else {
            return Ok(SeedAttempt::Superseded);
        };
        if &attached.daemon_instance != instance || attached.connection_id != guard.connection_id {
            return Ok(SeedAttempt::Superseded);
        }
        match outcome {
            StatusSnapshotOutcome::Answered(snapshot) => {
                // What is about to be kept is checked, rather than the port being
                // trusted to have answered about what it was asked.
                if snapshot.facts_for(host, instance).is_err() {
                    return Err(SeedFailure::Misattributed);
                }
                state.sealed = Some(Sealed {
                    snapshot,
                    held_since: Instant::now(),
                });
                state.failed = 0;
                state.next = None;
                Ok(SeedAttempt::Sealed)
            }
            // Which failure it was is consumed here and not kept. The ladder is the
            // only thing a failure may leave behind, and the ladder is not readable.
            StatusSnapshotOutcome::Disconnected
            | StatusSnapshotOutcome::Unreachable(_)
            | StatusSnapshotOutcome::TransportContractViolation => {
                let attempts = state.failed + 1;
                match SEED_RETRY_DELAYS.get(attempts - 1) {
                    Some(delay) => {
                        // Minted before the rung is taken: a wake nobody could name is
                        // not a rung this daemon has spent.
                        let ticket = RetryTicket(take_id(next_id)?);
                        state.failed = attempts;
                        state.next = Some(Schedule {
                            not_before: Instant::now() + *delay,
                            ticket,
                        });
                        Ok(SeedAttempt::Deferred {
                            after: *delay,
                            ticket,
                        })
                    }
                    None => {
                        state.failed = attempts;
                        state.next = None;
                        // The agreed post-rollout observable for the residual risk
                        // "the ladder ran out while the daemon stayed attached":
                        // count these before deciding whether a slow recovery is
                        // warranted.
                        tracing::warn!(
                            host_id =
                                crate::foundation::safe_log_field::safe_display(&(host.as_str())),
                            daemon_instance = crate::foundation::safe_log_field::safe_display(
                                &(instance.as_str())
                            ),
                            attempts,
                            "host status seed retries exhausted; the custody stays unseeded for the rest of this connection generation"
                        );
                        Ok(SeedAttempt::Exhausted)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use serde_json::{Value, json};

    use super::*;
    use crate::host_runtime::host_control::status_snapshot::{UnreachableReason, decode_for_tests};

    /// How long a withheld frame may yield before the test calls it a defect instead
    /// of waiting for it.
    const HELD_YIELD_BUDGET: u32 = 1_000;

    /// What the custody does at the moment an answer is served, standing in for a
    /// daemon or an authority that changed while a frame was out.
    enum WhenServed {
        Unchanged,
        Replaced(&'static str),
        /// The authority stops answering. An attempt then cannot learn whether its own
        /// custody is still there, which is not the same as learning that it is gone.
        Unreadable,
    }

    /// The transport authority, driven directly. One host: the real registry answers
    /// per host and is exercised over real sockets in `daemon_delivery_e2e`; this
    /// stands in only for this leaf's own schedule and seal arms.
    struct Daemon {
        attached: Mutex<Option<DaemonInstanceId>>,
        connection: Mutex<Option<String>>,
        live_agents: Mutex<Option<HashSet<AgentId>>>,
        /// Answers per custody, so a test with two frames out is not also writing down
        /// the order in which they happen to settle.
        answers: Mutex<HashMap<String, VecDeque<(StatusSnapshotOutcome, WhenServed)>>>,
        frames: Mutex<Vec<(HostId, DaemonInstanceId)>>,
        /// Custodies whose answer is withheld, so a test can place work into the window
        /// between a frame leaving and its answer settling.
        held: Mutex<HashSet<String>>,
        broken: AtomicBool,
        /// The store this authority is driven behind, registered once it exists. Trying
        /// its lock from in here is how "the caller is holding the store" becomes a
        /// mechanical statement rather than a reading of the source. Weak, because the
        /// store owns this authority and this is only a test's view back into it.
        store: Mutex<Option<std::sync::Weak<Mutex<SeedStore>>>>,
        /// A successor whose own admission lands inside whatever window an observation
        /// leaves open. Armed only where the cost of an open window is what the test
        /// measures; elsewhere an open window is the defect itself.
        window: Mutex<Option<DaemonInstanceId>>,
        opened: AtomicUsize,
        tripwire: AtomicBool,
    }

    impl DaemonCustody for Daemon {
        fn attached_custody(
            &self,
            _host: &HostId,
        ) -> Result<Option<AttachedStatusCustody>, CustodyUnavailable> {
            let current = self.attached.lock().unwrap().clone();
            let connection = self.connection.lock().unwrap().clone();
            self.observe();
            if self.broken.load(Ordering::SeqCst) {
                return Err(CustodyUnavailable);
            }
            Ok(current
                .zip(connection)
                .map(|(daemon_instance, connection_id)| AttachedStatusCustody {
                    connection_id,
                    daemon_instance,
                }))
        }

        fn probe_status<'a>(
            &'a self,
            host: HostId,
            custody: AttachedStatusCustody,
        ) -> Pin<Box<dyn Future<Output = StatusSnapshotOutcome> + Send + 'a>> {
            Box::pin(async move {
                let instance = custody.daemon_instance;
                if self.attached.lock().unwrap().as_ref() != Some(&instance)
                    || self.connection.lock().unwrap().as_ref() != Some(&custody.connection_id)
                {
                    return StatusSnapshotOutcome::Disconnected;
                }
                self.frames.lock().unwrap().push((host, instance.clone()));
                // Bounded on purpose. A frame that should never have left is sent at a
                // custody whose hold no test releases, and an unbounded spin would
                // report that as a hung suite rather than as the defect it is. The
                // runtime is current-thread and paused, so a waiting frame yields only
                // while the test body walks to its next `release` — a handful of times,
                // never near this budget.
                let mut yields = 0;
                while self.held.lock().unwrap().contains(instance.as_str()) {
                    yields += 1;
                    assert!(
                        yields < HELD_YIELD_BUDGET,
                        "a frame for {} waited on a hold that nothing released",
                        instance.as_str()
                    );
                    tokio::task::yield_now().await;
                }
                let (outcome, when_served) = self
                    .answers
                    .lock()
                    .unwrap()
                    .get_mut(instance.as_str())
                    .and_then(VecDeque::pop_front)
                    .expect("a probe was sent that the test has no answer for");
                match when_served {
                    WhenServed::Unchanged => {}
                    WhenServed::Replaced(instance) => self.set_attached(Some(instance)),
                    WhenServed::Unreadable => self.broken.store(true, Ordering::SeqCst),
                }
                outcome
            })
        }
    }

    impl HostRuntimeActivity for Daemon {
        fn active_agents(
            &self,
            _host: &HostId,
            _custody: &AttachedStatusCustody,
        ) -> Result<Option<HashSet<AgentId>>, CustodyUnavailable> {
            Ok(self.live_agents.lock().unwrap().clone())
        }
    }

    impl Daemon {
        /// Every custody observation in this leaf must be taken while its caller holds
        /// the seed store: that is the linearization point, and an answer taken outside
        /// it is a snapshot that a mutation can be ordered between.
        ///
        /// Resident and on by default, so moving any observation back outside the lock
        /// fails wherever a custody is read at all. Where a test arms a successor, an
        /// open window is exercised instead of reported: the successor's whole admission
        /// lands inside it, and the caller carries out the stale answer it came for.
        fn observe(&self) {
            let registered = self.store.lock().unwrap();
            let store = registered
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .expect("the store under test is registered before it is driven");
            let Ok(mut open) = store.try_lock() else {
                return;
            };
            self.opened.fetch_add(1, Ordering::SeqCst);
            let successor = self.window.lock().unwrap().take();
            if let Some(successor) = successor {
                let SeedStore { states, next_id } = &mut *open;
                let id =
                    AttemptId(take_id(next_id).expect("a test does not exhaust identities here"));
                let mut state = SeedState::new(
                    successor.clone(),
                    format!("connection:{}", successor.as_str()),
                );
                state.in_flight = Some(id);
                states.insert(host().as_str().to_owned(), state);
                drop(open);
                *self.attached.lock().unwrap() = Some(successor);
                *self.connection.lock().unwrap() = self
                    .attached
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|instance| format!("connection:{}", instance.as_str()));
                return;
            }
            assert!(
                !self.tripwire.load(Ordering::SeqCst),
                "a custody was observed while the seed store was not held"
            );
        }

        /// Registered after construction, which is the only ordering available: the
        /// store is built around this authority.
        fn watching(&self, seeds: &HostStatusSeeds) {
            *self.store.lock().unwrap() = Some(Arc::downgrade(&seeds.store));
        }

        /// Who holds the host, read without going through the observation gate — a test
        /// asserting on the authority is not one of the callers that gate is about.
        fn attached(&self) -> Option<DaemonInstanceId> {
            self.attached.lock().unwrap().clone()
        }

        /// Stop reporting an open window and use it: one successor attaches and its own
        /// admission lands, inside whatever window the next observation leaves open.
        fn arm_window(&self, successor: &str) {
            self.tripwire.store(false, Ordering::SeqCst);
            *self.window.lock().unwrap() = Some(instance_id(successor));
        }

        fn windows_opened(&self) -> usize {
            self.opened.load(Ordering::SeqCst)
        }

        fn set_attached(&self, instance: Option<&str>) {
            *self.attached.lock().unwrap() = instance.map(instance_id);
            *self.connection.lock().unwrap() =
                instance.map(|instance| format!("connection:{instance}"));
        }

        fn set_generation(&self, instance: &str, connection: &str) {
            *self.attached.lock().unwrap() = Some(instance_id(instance));
            *self.connection.lock().unwrap() = Some(connection.to_owned());
        }

        fn set_live_agents(&self, agents: Option<HashSet<AgentId>>) {
            *self.live_agents.lock().unwrap() = agents;
        }

        fn answer(&self, instance: &str, outcome: StatusSnapshotOutcome, when_served: WhenServed) {
            self.answers
                .lock()
                .unwrap()
                .entry(instance.to_owned())
                .or_default()
                .push_back((outcome, when_served));
        }

        fn hold(&self, instance: &str) {
            self.held.lock().unwrap().insert(instance.to_owned());
        }

        fn release(&self, instance: &str) {
            self.held.lock().unwrap().remove(instance);
        }

        fn repair(&self) {
            self.broken.store(false, Ordering::SeqCst);
        }

        fn frames(&self) -> usize {
            self.frames.lock().unwrap().len()
        }

        /// Wait for a frame that is expected to leave, bounded like a withheld answer.
        /// A frame that never arrives is a defect to report rather than something to keep
        /// waiting on: an unbounded spin would turn a regression — or a panic inside the
        /// attempt being waited on — into a suite that hangs instead of one that fails.
        async fn frames_reach(&self, frames: usize) {
            for _ in 0..HELD_YIELD_BUDGET {
                if self.frames() >= frames {
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!(
                "expected {frames} frames to have left, and {} did",
                self.frames()
            );
        }

        fn asked(&self) -> Vec<(HostId, DaemonInstanceId)> {
            self.frames.lock().unwrap().clone()
        }
    }

    /// The store under test, and the authority behind it, which the test drives.
    fn seeds(attached: Option<&str>) -> (HostStatusSeeds, Arc<Daemon>) {
        let daemon = Arc::new(Daemon {
            attached: Mutex::new(attached.map(instance_id)),
            connection: Mutex::new(attached.map(|instance| format!("connection:{instance}"))),
            live_agents: Mutex::new(None),
            answers: Mutex::new(HashMap::new()),
            frames: Mutex::new(Vec::new()),
            held: Mutex::new(HashSet::new()),
            broken: AtomicBool::new(false),
            store: Mutex::new(None),
            window: Mutex::new(None),
            opened: AtomicUsize::new(0),
            tripwire: AtomicBool::new(true),
        });
        let seeds = HostStatusSeeds::new(daemon.clone(), daemon.clone());
        daemon.watching(&seeds);
        (seeds, daemon)
    }

    fn host() -> HostId {
        HostId::parse("host-1").unwrap()
    }

    fn instance_id(id: &str) -> DaemonInstanceId {
        DaemonInstanceId::parse(id).unwrap()
    }

    fn status(overrides: Value) -> Value {
        let mut status = json!({
            "os": "macOS 14.5",
            "os_arch": "arm64",
            "daemon_pid": 4_242,
            "daemon_version": "1.2.3",
            "started_at": 1_700_000_000_000_i64,
            "uptime_ms": 90_000,
            "active_agent_ids": ["agent-1"],
        });
        for (key, value) in overrides.as_object().unwrap() {
            status[key] = value.clone();
        }
        status
    }

    /// A real decoded snapshot, so no test holds facts shaped differently from what
    /// the wire produces.
    fn answered(instance: &str) -> StatusSnapshotOutcome {
        answered_about(&host(), instance)
    }

    fn answered_about(host: &HostId, instance: &str) -> StatusSnapshotOutcome {
        StatusSnapshotOutcome::Answered(
            decode_for_tests(status(json!({})), host.clone(), instance_id(instance))
                .expect("the fixture status decodes"),
        )
    }

    fn answered_with(instance: &str, overrides: Value) -> StatusSnapshotOutcome {
        StatusSnapshotOutcome::Answered(
            decode_for_tests(status(overrides), host(), instance_id(instance))
                .expect("the fixture status decodes"),
        )
    }

    fn unreachable() -> StatusSnapshotOutcome {
        StatusSnapshotOutcome::Unreachable(UnreachableReason::Timeout)
    }

    fn seeded(read: SeedRead) -> SeededStatus {
        match read {
            SeedRead::Seeded(status) => status,
            other => panic!("expected seeded facts, got {other:?}"),
        }
    }

    /// A deferral's two halves: how long the driver must wait, and the one wake that
    /// will be honoured when it does.
    fn deferral(attempt: Result<SeedAttempt, SeedFailure>) -> (Duration, RetryTicket) {
        match attempt {
            Ok(SeedAttempt::Deferred { after, ticket }) => (after, ticket),
            other => panic!("expected a deferral, got {other:?}"),
        }
    }

    fn admitted(admission: Admission) -> AttemptGuard {
        match admission {
            Admission::Probe(guard) => guard,
            Admission::Settled(attempt) => panic!("expected an admission, got {attempt:?}"),
        }
    }

    #[tokio::test]
    async fn same_instance_reconnect_is_a_new_seed_generation() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer(
            "instance-1",
            answered_with("instance-1", json!({ "os": "generation-a" })),
            WhenServed::Unchanged,
        );
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(seeded(seeds.read(&host()).unwrap()).os, "generation-a");

        daemon.set_generation("instance-1", "connection-b");
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);
        daemon.answer(
            "instance-1",
            answered_with("instance-1", json!({ "os": "generation-b" })),
            WhenServed::Unchanged,
        );
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(seeded(seeds.read(&host()).unwrap()).os, "generation-b");
    }

    #[tokio::test]
    async fn old_generation_late_answer_cannot_overwrite_its_same_instance_successor() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer(
            "instance-1",
            answered_with("instance-1", json!({ "os": "late-a" })),
            WhenServed::Unchanged,
        );
        daemon.answer(
            "instance-1",
            answered_with("instance-1", json!({ "os": "current-b" })),
            WhenServed::Unchanged,
        );
        daemon.hold("instance-1");
        let old = tokio::spawn({
            let seeds = seeds.clone();
            let instance = instance.clone();
            async move { seeds.seed_on_attach(&host(), &instance).await }
        });
        daemon.frames_reach(1).await;

        daemon.set_generation("instance-1", "connection-b");
        let current = tokio::spawn({
            let seeds = seeds.clone();
            let instance = instance.clone();
            async move { seeds.seed_on_attach(&host(), &instance).await }
        });
        daemon.frames_reach(2).await;
        daemon.release("instance-1");
        assert_eq!(old.await.unwrap(), Ok(SeedAttempt::Superseded));
        assert_eq!(current.await.unwrap(), Ok(SeedAttempt::Sealed));
        assert_eq!(
            seeds
                .store
                .lock()
                .unwrap()
                .states
                .get(host().as_str())
                .unwrap()
                .connection_id,
            "connection-b"
        );
    }

    #[tokio::test]
    async fn hydrated_event_activity_overrides_seed_fallback_and_can_remove_online_agents() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).active_agents,
            HashSet::from([AgentId::parse("agent-1").unwrap()])
        );

        daemon.set_live_agents(Some(HashSet::from([AgentId::parse("agent-2").unwrap()])));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).active_agents,
            HashSet::from([AgentId::parse("agent-2").unwrap()])
        );
        daemon.set_live_agents(Some(HashSet::new()));
        assert!(
            seeded(seeds.read(&host()).unwrap())
                .active_agents
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn old_generation_retry_ticket_never_sends_on_same_instance_successor() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        let (_, ticket) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        assert_eq!(daemon.frames(), 1);

        daemon.set_generation("instance-1", "connection-b");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Ok(SeedAttempt::Superseded)
        );
        assert_eq!(daemon.frames(), 1);
    }

    /// Which attempt the host is currently kept by, read directly: no outcome names an
    /// attempt, because nothing outside this module may hold one.
    fn claimed_by(seeds: &HostStatusSeeds) -> Option<AttemptId> {
        seeds
            .store
            .lock()
            .unwrap()
            .states
            .get(host().as_str())
            .and_then(|state| state.in_flight)
    }

    /// Which custody the host's entry belongs to, read directly for the same reason. Not
    /// the same question as [`HostStatusSeeds::read`]: an entry can stand for a custody
    /// that has already been replaced, and that read would answer `NotSeeded` for it.
    fn entry_instance(seeds: &HostStatusSeeds) -> Option<DaemonInstanceId> {
        seeds
            .store
            .lock()
            .unwrap()
            .states
            .get(host().as_str())
            .map(|state| state.instance.clone())
    }

    #[tokio::test(start_paused = true)]
    async fn a_custody_is_asked_once_and_then_served_from_memory() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);

        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Sealed)
        );
        // The same daemon re-attaching does not re-ask: the facts belong to its
        // process, and its process did not restart.
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::AlreadySealed)
        );
        assert_eq!(daemon.frames(), 1);
        assert_eq!(daemon.asked(), vec![(host(), instance_id("instance-1"))]);

        let facts = seeded(seeds.read(&host()).unwrap());
        assert_eq!(facts.daemon_instance, instance_id("instance-1"));
        assert_eq!(facts.os, "macOS 14.5");
        assert_eq!(facts.os_arch.as_deref(), Some("arm64"));
        assert_eq!(facts.daemon_pid, 4_242);
        assert_eq!(facts.daemon_version, "1.2.3");
        assert_eq!(facts.started_at, 1_700_000_000_000);
        assert_eq!(facts.uptime_ms, 90_000);
        assert_eq!(
            facts.active_agents,
            HashSet::from([AgentId::parse("agent-1").unwrap()])
        );
        // Reading is memory only. Nothing else would have an answer to serve.
        assert_eq!(daemon.frames(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_attempt_keeps_nothing_and_walks_the_deployed_ladder() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        for _ in 0..6 {
            daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        }
        let instance = instance_id("instance-1");

        let (after, ticket) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        assert_eq!(after, Duration::from_secs(1));
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);

        // Punctuality is not assumed: an early continuation is suppressed, the
        // suppression sends no frame, and the wake it presented is still the one owed
        // rather than being replaced by a fresh deadline.
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Ok(SeedAttempt::Deferred {
                after: Duration::from_secs(1),
                ticket
            })
        );
        assert_eq!(daemon.frames(), 1);

        let mut ticket = ticket;
        for (waited, next) in [(1, 2), (2, 5), (5, 10), (10, 30)] {
            tokio::time::advance(Duration::from_secs(waited)).await;
            let (after, minted) = deferral(seeds.retry(&host(), &instance, ticket).await);
            assert_eq!(after, Duration::from_secs(next));
            // Every rung is its own wake, so the wake just spent cannot be spent again.
            assert_ne!(minted, ticket);
            ticket = minted;
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Ok(SeedAttempt::Exhausted)
        );
        assert_eq!(daemon.frames(), 6);

        // The schedule is bounded: nothing keeps probing, and nothing was cached to
        // serve in its place. There is no wake left to present, and an attach is told
        // why rather than starting over.
        tokio::time::advance(Duration::from_secs(3_600)).await;
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Ok(SeedAttempt::StaleWake)
        );
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Ok(SeedAttempt::Exhausted)
        );
        assert_eq!(daemon.frames(), 6);
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);
    }

    /// A transient unreadable observation alone does not mint a new generation. The
    /// exact same custody keeps the ladder it earned until registry admission says a
    /// different connection holds the host.
    #[tokio::test(start_paused = true)]
    async fn transient_absence_of_the_same_generation_preserves_its_ladder() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        let (after, ticket) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        assert_eq!(after, Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        let (after, ticket) = deferral(seeds.retry(&host(), &instance, ticket).await);
        assert_eq!(after, Duration::from_secs(2));
        assert_eq!(daemon.frames(), 2);

        // The fake authority briefly cannot observe the exact same generation. Once
        // that same token is visible again, the deferral stands: no frame and no new
        // ticket.
        daemon.set_attached(None);
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::Disconnected);
        daemon.set_attached(Some("instance-1"));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Ok(SeedAttempt::Deferred {
                after: Duration::from_secs(1),
                ticket
            })
        );
        assert_eq!(daemon.frames(), 2);

        // Only that delay lets the next frame out, and a landed seed leaves no
        // position behind. Nothing readable depends on the position, so the state
        // itself is the assertion.
        tokio::time::advance(Duration::from_secs(1)).await;
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 3);
        let store = seeds.store.lock().unwrap();
        let state = store
            .states
            .get(host().as_str())
            .expect("the host has an entry");
        assert_eq!(state.failed, 0);
        assert!(state.next.is_none());
        assert!(state.in_flight.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_replaced_daemon_inherits_neither_the_facts_nor_the_backoff() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Sealed)
        );
        tokio::time::advance(Duration::from_secs(1)).await;

        // The daemon process restarted. Its predecessor's facts describe a process
        // that is gone, so they are not served for it.
        daemon.set_attached(Some("instance-2"));
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);

        // And its predecessor's backoff is not its own: asked at once, from the start
        // of the ladder.
        daemon.answer("instance-2", unreachable(), WhenServed::Unchanged);
        let (after, ticket) = deferral(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-2"))
                .await,
        );
        assert_eq!(after, Duration::from_secs(1));
        assert_eq!(daemon.frames(), 2);

        // The predecessor cannot be asked again either, and cannot displace the entry
        // of the daemon that replaced it.
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Superseded)
        );
        assert_eq!(daemon.frames(), 2);
        daemon.answer("instance-2", answered("instance-2"), WhenServed::Unchanged);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            seeds
                .retry(&host(), &instance_id("instance-2"), ticket)
                .await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-2")
        );
    }

    /// Identity is what separates generations. A ticket is unique across the store, so
    /// a wake left over from a daemon that restarted matches nothing — which is why the
    /// counter cannot live on the entry, where it would restart with it.
    #[tokio::test(start_paused = true)]
    async fn a_ticket_from_a_replaced_daemon_neither_continues_nor_drives_its_successor() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        let (_, stale) = deferral(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
        );

        // The daemon restarted, so the host is held by a new instance with a schedule
        // of its own.
        daemon.set_attached(Some("instance-2"));
        daemon.answer("instance-2", unreachable(), WhenServed::Unchanged);
        let (_, live) = deferral(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-2"))
                .await,
        );
        assert_ne!(stale, live);
        assert_eq!(daemon.frames(), 2);
        tokio::time::advance(Duration::from_secs(1)).await;

        // The leftover wake is stale twice over: it names a custody that is gone, and
        // offered under the live one it is still not the wake that custody waits for.
        assert_eq!(
            seeds
                .retry(&host(), &instance_id("instance-1"), stale)
                .await,
            Ok(SeedAttempt::Superseded)
        );
        assert_eq!(
            seeds
                .retry(&host(), &instance_id("instance-2"), stale)
                .await,
            Ok(SeedAttempt::StaleWake)
        );
        assert_eq!(daemon.frames(), 2);

        // The live wake still works, which is what makes the two refusals above about
        // identity rather than about a store that stopped admitting anything.
        daemon.answer("instance-2", answered("instance-2"), WhenServed::Unchanged);
        assert_eq!(
            seeds.retry(&host(), &instance_id("instance-2"), live).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 3);
    }

    /// A wake is spent, not merely presented. Two drivers holding the same wake — a
    /// duplicate timer, a replayed one — cannot both turn it into a frame, which is
    /// what makes this leaf sufficient on its own without assuming anything about who
    /// is sleeping outside it.
    #[tokio::test(start_paused = true)]
    async fn only_the_first_wake_naming_a_deferral_continues_it() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        let (_, spent) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        tokio::time::advance(Duration::from_secs(1)).await;

        let (after, minted) = deferral(seeds.retry(&host(), &instance, spent).await);
        assert_eq!(after, Duration::from_secs(2));
        assert_ne!(minted, spent);
        assert_eq!(daemon.frames(), 2);

        // The second holder of that wake arrives to a schedule that no longer answers
        // to it. Being late is harmless rather than a second frame, and it neither
        // consumes nor disturbs the wake the schedule is now waiting for.
        assert_eq!(
            seeds.retry(&host(), &instance, spent).await,
            Ok(SeedAttempt::StaleWake)
        );
        assert_eq!(daemon.frames(), 2);
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            seeds.retry(&host(), &instance, minted).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 3);
    }

    /// An attempt whose owner is cancelled learned nothing, so it advances no rung and
    /// hands back no wake — but it must not keep the host either. Recovery is the next
    /// attach of the same daemon: there is no state in which a host has neither a live
    /// attempt nor a way to get one.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_attempt_frees_the_host_and_the_next_attach_continues_the_ladder() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        // Three answers for four frames: the cancelled attempt never reaches its own.
        for _ in 0..3 {
            daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        }
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);

        let (_, ticket) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        tokio::time::advance(Duration::from_secs(1)).await;
        let (after, ticket) = deferral(seeds.retry(&host(), &instance, ticket).await);
        assert_eq!(after, Duration::from_secs(2));
        tokio::time::advance(Duration::from_secs(2)).await;

        // The third attempt is admitted, its frame leaves, and its owner is cancelled
        // while the answer is still out.
        daemon.hold("instance-1");
        let owner = tokio::spawn({
            let seeds = seeds.clone();
            let instance = instance.clone();
            async move { seeds.retry(&host(), &instance, ticket).await }
        });
        daemon.frames_reach(3).await;
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        daemon.release("instance-1");

        // The host is admissible again, and the ladder is exactly where the answers
        // left it: a cancellation is not a failure to have learned from.
        assert_eq!(
            {
                let store = seeds.store.lock().unwrap();
                let state = store.states.get(host().as_str()).expect("the entry stands");
                (state.in_flight, state.next.is_none(), state.failed)
            },
            (None, true, 2)
        );

        // So the next attach is asked at once — no wake is owed to anyone — and the
        // rung it lands on is the one the daemon had earned, not the start.
        let (after, revived) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        assert_eq!(after, Duration::from_secs(5));
        assert_ne!(revived, ticket);
        assert_eq!(daemon.frames(), 4);

        // And that wake is a real one, which is what separates recovery from a store
        // that merely stopped refusing.
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            seeds.retry(&host(), &instance, revived).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 5);
    }

    /// Releasing is scoped to the attempt doing it. A claim freed late — a dropped task
    /// whose future is only reaped after the host was admitted again — must not hand
    /// away a successor's hold, or a host would be probed twice at once.
    #[tokio::test(start_paused = true)]
    async fn a_stale_attempt_dropped_late_cannot_release_the_one_that_replaced_it() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        let stale = admitted(seeds.admit(&host(), &instance, None).unwrap());
        // Stand in for that attempt having already given the host up — the release its
        // own drop is about to repeat.
        seeds
            .store
            .lock()
            .unwrap()
            .states
            .get_mut(host().as_str())
            .unwrap()
            .in_flight = None;

        let live = admitted(seeds.admit(&host(), &instance, None).unwrap());
        assert_eq!(claimed_by(&seeds), Some(live.id));
        drop(stale);
        assert_eq!(claimed_by(&seeds), Some(live.id));
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Ok(SeedAttempt::AlreadyInFlight)
        );

        // The live attempt still frees it, so the refusal above is about whose claim it
        // was rather than about a hold that nothing can clear.
        drop(live);
        assert_eq!(claimed_by(&seeds), None);
        assert_eq!(daemon.frames(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_from_a_custody_that_moved_is_neither_written_nor_restored() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        // The answer is good, and by the time it lands another daemon holds the host.
        daemon.answer(
            "instance-1",
            answered("instance-1"),
            WhenServed::Replaced("instance-2"),
        );
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Superseded)
        );
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);

        // The successor seeds itself, and the answer that was dropped cannot come
        // back to overwrite it.
        daemon.answer("instance-2", answered("instance-2"), WhenServed::Unchanged);
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-2"))
                .await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-2")
        );
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Superseded)
        );
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-2")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_survives_a_transient_absence_of_its_exact_generation() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.hold("instance-1");
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        let seeding = tokio::spawn({
            let seeds = seeds.clone();
            async move {
                seeds
                    .seed_on_attach(&host(), &instance_id("instance-1"))
                    .await
            }
        });
        // Let the frame leave and stop short of its answer.
        daemon.frames_reach(1).await;

        // The authority temporarily reports absence, then restores the exact same
        // generation while its answer is still in flight.
        daemon.set_attached(None);
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::Disconnected);
        daemon.set_attached(Some("instance-1"));

        daemon.release("instance-1");
        // The answer still names the current generation, so it is filed.
        assert_eq!(seeding.await.unwrap(), Ok(SeedAttempt::Sealed));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-1")
        );
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::AlreadySealed)
        );
        assert_eq!(daemon.frames(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_answer_cannot_release_the_single_flight_of_the_custody_that_replaced_it() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.hold("instance-1");
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        let first = tokio::spawn({
            let seeds = seeds.clone();
            async move {
                seeds
                    .seed_on_attach(&host(), &instance_id("instance-1"))
                    .await
            }
        });
        daemon.frames_reach(1).await;

        // Its successor takes the host and has a frame of its own out.
        daemon.set_attached(Some("instance-2"));
        daemon.hold("instance-2");
        daemon.answer("instance-2", answered("instance-2"), WhenServed::Unchanged);
        let second = tokio::spawn({
            let seeds = seeds.clone();
            async move {
                seeds
                    .seed_on_attach(&host(), &instance_id("instance-2"))
                    .await
            }
        });
        daemon.frames_reach(2).await;

        // The predecessor's answer now settles against an entry that is not its own.
        daemon.release("instance-1");
        assert_eq!(first.await.unwrap(), Ok(SeedAttempt::Superseded));
        // The successor's frame is still the one that counts. Had the late answer
        // cleared the admission it found, this would send a second frame for a custody
        // that already has one out.
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-2"))
                .await,
            Ok(SeedAttempt::AlreadyInFlight)
        );
        assert_eq!(daemon.frames(), 2);

        daemon.release("instance-2");
        assert_eq!(second.await.unwrap(), Ok(SeedAttempt::Sealed));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-2")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_authority_that_stops_answering_releases_the_attempt_rather_than_stranding_the_host()
    {
        let (seeds, daemon) = seeds(Some("instance-1"));
        // The answer arrives and the authority goes dark before it can be filed, so
        // this attempt never learns whether its own custody is still there.
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unreadable);
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Err(SeedFailure::Unavailable)
        );
        assert_eq!(seeds.read(&host()), Err(SeedFailure::Unavailable));

        // Nothing was kept, and nothing was left holding the host's admission: the
        // authority coming back is enough for the next attach to be asked again.
        daemon.repair();
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 2);
    }

    /// Read-only absence does not itself mutate state. A new admitted connection
    /// generation is what replaces a seal.
    #[tokio::test(start_paused = true)]
    async fn a_seal_survives_transient_absence_but_not_a_new_generation() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Sealed)
        );

        // The fake authority reports absence then restores the same generation; the
        // read itself neither deletes nor reseeds it.
        daemon.set_attached(None);
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::Disconnected);
        daemon.set_attached(Some("instance-1"));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-1")
        );
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::AlreadySealed)
        );
        assert_eq!(daemon.frames(), 1);

        // A different generation is different evidence and must be asked again.
        daemon.set_attached(Some("instance-2"));
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);
        daemon.answer("instance-2", answered("instance-2"), WhenServed::Unchanged);
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-2"))
                .await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 2);
    }

    #[tokio::test]
    async fn refresh_replaces_the_seed_and_a_failed_one_keeps_it() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer(
            "instance-1",
            answered_with(
                "instance-1",
                json!({ "memory_total_bytes": 100, "memory_used_bytes": 10 }),
            ),
            WhenServed::Unchanged,
        );
        seeds.seed_on_attach(&host(), &instance).await.unwrap();

        daemon.answer(
            "instance-1",
            answered_with(
                "instance-1",
                json!({ "memory_total_bytes": 100, "memory_used_bytes": 70 }),
            ),
            WhenServed::Unchanged,
        );
        assert_eq!(seeds.refresh(&host()).await, Ok(RefreshOutcome::Refreshed));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap())
                .machine
                .memory
                .map(|m| m.used_bytes),
            Some(70)
        );

        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        assert_eq!(
            seeds.refresh(&host()).await,
            Ok(RefreshOutcome::Unreachable(UnreachableReason::Timeout))
        );
        assert_eq!(
            seeded(seeds.read(&host()).unwrap())
                .machine
                .memory
                .map(|m| m.used_bytes),
            Some(70)
        );
        assert_eq!(daemon.frames(), 3);
    }

    #[tokio::test]
    async fn refresh_seals_an_unseeded_custody_and_sends_nothing_when_absent() {
        let (seeds, daemon) = seeds(None);
        assert_eq!(
            seeds.refresh(&host()).await,
            Ok(RefreshOutcome::Disconnected)
        );
        assert_eq!(daemon.frames(), 0);

        daemon.set_attached(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(seeds.refresh(&host()).await, Ok(RefreshOutcome::Refreshed));
        assert_eq!(seeded(seeds.read(&host()).unwrap()).os, "macOS 14.5");
    }

    #[tokio::test]
    async fn refresh_drops_an_answer_from_a_replaced_custody() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer(
            "instance-1",
            answered("instance-1"),
            WhenServed::Replaced("instance-2"),
        );
        assert_eq!(
            seeds.refresh(&host()).await,
            Ok(RefreshOutcome::Disconnected)
        );
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);
    }

    #[tokio::test(start_paused = true)]
    async fn system_uptime_grows_with_the_hold_like_daemon_uptime() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer(
            "instance-1",
            answered_with("instance-1", json!({ "system_uptime_ms": 1_000 })),
            WhenServed::Unchanged,
        );
        seeds
            .seed_on_attach(&host(), &instance_id("instance-1"))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            seeded(seeds.read(&host()).unwrap())
                .machine
                .system_uptime_ms_at_answer,
            Some(6_000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn uptime_grows_with_the_hold_and_the_daemon_clock_is_read_once() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        seeds
            .seed_on_attach(&host(), &instance_id("instance-1"))
            .await
            .unwrap();
        assert_eq!(seeded(seeds.read(&host()).unwrap()).uptime_ms, 90_000);

        tokio::time::advance(Duration::from_secs(5)).await;
        let facts = seeded(seeds.read(&host()).unwrap());
        assert_eq!(facts.uptime_ms, 95_000);
        // The daemon's own clock is not re-read, and nothing else moved.
        assert_eq!(facts.started_at, 1_700_000_000_000);
        assert_eq!(daemon.frames(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_about_another_host_is_dropped_rather_than_filed() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer(
            "instance-1",
            answered_about(&HostId::parse("host-2").unwrap(), "instance-1"),
            WhenServed::Unchanged,
        );
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Err(SeedFailure::Misattributed)
        );
        assert_eq!(seeds.read(&host()).unwrap(), SeedRead::NotSeeded);
        assert_eq!(daemon.frames(), 1);
    }

    /// The interleaving that makes an observation taken outside the store a liveness
    /// defect rather than a stale answer: the successor's whole admission — its entry and
    /// its claim — lands inside the window, so what a predecessor would delete is the
    /// live daemon's only seed, at a point where its attach edge has already been spent
    /// and nothing will bring it back.
    ///
    /// Armed so that any window is used rather than merely reported, which gives this
    /// case a behavioural failure of its own next to the resident one.
    #[tokio::test(start_paused = true)]
    async fn a_predecessor_cannot_replace_the_entry_of_the_custody_holding_the_host() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        daemon.arm_window("instance-2");

        // The predecessor is still the attached custody when it asks, so nothing about
        // this call is late — the successor only arrives if this leaf leaves it room to.
        let outcome = seeds
            .seed_on_attach(&host(), &instance_id("instance-1"))
            .await;

        // Behaviourally: the entry that stands belongs to the custody that holds the
        // host, and it is that custody's own attempt which sealed it.
        assert_eq!(entry_instance(&seeds), daemon.attached());
        // And mechanically: there was never a window for that admission to land in.
        assert_eq!(daemon.windows_opened(), 0);
        assert_eq!(outcome, Ok(SeedAttempt::Sealed));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-1")
        );
        assert_eq!(daemon.frames(), 1);
    }

    /// The same axis with no window at all, which is the shape a driver actually
    /// produces: a predecessor's wake fires entirely after its successor has attached and
    /// taken the host. It must not take the entry and must not disturb the claim, or the
    /// successor's frame — already out — would have nowhere to settle.
    #[tokio::test(start_paused = true)]
    async fn a_wake_arriving_after_its_successor_attached_leaves_the_successor_alone() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        let (_, stale) = deferral(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
        );

        // The daemon restarts. Its successor attaches, is admitted, and its frame is
        // still out — so this call begins, and has not finished, before the predecessor's
        // wake begins at all.
        daemon.set_attached(Some("instance-2"));
        daemon.hold("instance-2");
        daemon.answer("instance-2", answered("instance-2"), WhenServed::Unchanged);
        let successor = tokio::spawn({
            let seeds = seeds.clone();
            async move {
                seeds
                    .seed_on_attach(&host(), &instance_id("instance-2"))
                    .await
            }
        });
        daemon.frames_reach(2).await;
        let claim = claimed_by(&seeds).expect("the successor holds a claim of its own");
        assert_eq!(entry_instance(&seeds), Some(instance_id("instance-2")));

        // Only now is the predecessor's wake due, and it is refused for the one reason
        // that is true: the custody it speaks for no longer holds the host.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            seeds
                .retry(&host(), &instance_id("instance-1"), stale)
                .await,
            Ok(SeedAttempt::Superseded)
        );
        assert_eq!(entry_instance(&seeds), Some(instance_id("instance-2")));
        assert_eq!(claimed_by(&seeds), Some(claim));
        assert_eq!(daemon.frames(), 2);

        // Which is what the refusal has to be worth: the successor still settles the
        // frame it had out, and its seed is readable.
        daemon.release("instance-2");
        assert_eq!(successor.await.unwrap(), Ok(SeedAttempt::Sealed));
        assert_eq!(
            seeded(seeds.read(&host()).unwrap()).daemon_instance,
            instance_id("instance-2")
        );
    }

    /// Non-reuse is the only thing separating a dead generation's wake from a live one's,
    /// so running out of identities fails closed. Which build this is compiled for may
    /// not decide that: an overflow would panic in debug and reissue in release.
    #[tokio::test(start_paused = true)]
    async fn identities_run_out_rather_than_being_reused() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        seeds.store.lock().unwrap().next_id = u64::MAX;
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Err(SeedFailure::IdentityExhausted)
        );
        // An attempt that could not be named was not made: no frame, and no claim on the
        // host to be cleared by anyone.
        assert_eq!(daemon.frames(), 0);
        assert_eq!(claimed_by(&seeds), None);

        // The control, so that the refusal above is about the last identity rather than
        // about a store that stopped admitting: one left is enough.
        seeds.store.lock().unwrap().next_id = u64::MAX - 1;
        assert_eq!(
            seeds
                .seed_on_attach(&host(), &instance_id("instance-1"))
                .await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 1);
    }

    /// The other minting site. A rung is spent by handing out the wake that continues it,
    /// so a wake that cannot be named is not a rung this daemon has spent — proven by the
    /// ladder still starting where it did.
    #[tokio::test(start_paused = true)]
    async fn a_rung_is_not_taken_when_its_wake_cannot_be_named() {
        let (seeds, daemon) = seeds(Some("instance-1"));
        let instance = instance_id("instance-1");
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        daemon.answer("instance-1", unreachable(), WhenServed::Unchanged);
        // One identity left: enough to admit the attempt, not enough to defer it.
        seeds.store.lock().unwrap().next_id = u64::MAX - 1;
        assert_eq!(
            seeds.seed_on_attach(&host(), &instance).await,
            Err(SeedFailure::IdentityExhausted)
        );
        assert_eq!(daemon.frames(), 1);
        assert_eq!(claimed_by(&seeds), None);

        // The ladder is where it was, which is what says the rung was not taken: the
        // next attempt lands on the first delay, not the second.
        seeds.store.lock().unwrap().next_id = 0;
        let (after, ticket) = deferral(seeds.seed_on_attach(&host(), &instance).await);
        assert_eq!(after, Duration::from_secs(1));
        assert_eq!(daemon.frames(), 2);

        // And the admitting side of the same ordering: an attempt that cannot be named
        // must not have spent the wake it came with, or that deferral would be owed to
        // nobody.
        seeds.store.lock().unwrap().next_id = u64::MAX;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Err(SeedFailure::IdentityExhausted)
        );
        assert_eq!(daemon.frames(), 2);
        seeds.store.lock().unwrap().next_id = 0;
        daemon.answer("instance-1", answered("instance-1"), WhenServed::Unchanged);
        assert_eq!(
            seeds.retry(&host(), &instance, ticket).await,
            Ok(SeedAttempt::Sealed)
        );
        assert_eq!(daemon.frames(), 3);
    }
}
