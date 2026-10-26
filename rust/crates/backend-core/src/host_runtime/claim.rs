//! Claiming the computer an installation runs on, and handing its daemon a token.
//!
//! An installation announces itself with a coordinate it owns — its installation id —
//! rather than naming a computer, so this is the one operation whose subject may not
//! exist yet. Three answers are possible and the ORDER between them is the rule:
//! an active computer this installation already holds is reused, otherwise an
//! unclaimed local placeholder is adopted, otherwise a computer is registered. Getting
//! that order wrong does not fail — it silently gives a machine a second computer
//! record, and everything placed on the first one stops being reachable.
//!
//! The order is decided by [`LocalClaimAttempt::decide`], from facts read inside the
//! write transaction, because all three branches are conditioned on rows another
//! request can create between a read and a write.
//!
//! The credential half is not this module's rule: it mints through
//! [`super::custody`] like every other issuing operation, and only decides WHETHER to
//! mint.

use std::sync::{Arc, OnceLock};

use thiserror::Error;

use super::{
    computer::{ComputerDisplayLabel, ComputerRecord},
    custody::{
        ConnectToken, CredentialIssueRefusal, CredentialIssueUnitOfWork, HostCredentialInventory,
        mint_credential,
    },
    environment::{
        Clock, ClockFailure, EntropyFailure, EntropySource, now_ms, random_base64url, random_hex,
    },
};
use crate::{
    foundation::identity::{HostId, UserId},
    foundation::persistence::PersistenceFailure,
};

/// The name an unclaimed placeholder carries. Also the fallback for a computer whose
/// label normalizes to nothing.
pub const LOCAL_COMPUTER_NAME: &str = "local";

const COMPUTER_ID_BYTES: usize = 12;
const NAME_SUFFIX_BYTES: usize = 3;
const NAME_SUFFIX_ATTEMPTS: usize = 8;
const INSTALLATION_PREFIX_CHARS: usize = 8;
const COMPUTER_NAME_MAX_CHARS: usize = 48;

/// What a claim's request carries. The installation id is a coordinate the caller
/// owns, not an identity this port issues, so it stays a string the schema at the
/// boundary bounds.
pub struct LocalClaimRequest {
    pub installation_id: String,
    /// What the user typed for this machine, when the caller sent one. Applied at
    /// CLAIM time only — see [`LocalClaimWrite`].
    pub label: Option<ComputerDisplayLabel>,
    /// The computer the caller believes it is already attached to. A hint about
    /// whether a token needs re-issuing, never about which computer it gets.
    pub current_computer: Option<HostId>,
}

/// The two coordinates the branch depends on, and only those.
///
/// Narrow on purpose: the branch is a choice between rows, so the whole records are
/// not an input to it. The adapter reads both inside the write transaction and owns
/// each query's predicate:
///
/// - `claimed` — the computer for this `(owner, installation_id)` that is NOT revoked.
///   Active-only is load-bearing: after a revoke the pair must fall through to a fresh
///   registration, which is what the active-only partial-unique index on the pair
///   allows.
/// - `unclaimed_local` — a computer named [`LOCAL_COMPUTER_NAME`], local, unrevoked,
///   with no owner AND no installation. All four clauses matter: a bootstrap left it
///   there for the first installation that shows up, and adopting a row that any of
///   them fails would hand one user's computer to another.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalClaimFacts {
    pub claimed: Option<HostId>,
    pub unclaimed_local: Option<HostId>,
}

/// The computer id a registration would need, minted lazily and at most once.
///
/// Minting draws OS entropy, which can fail. Only the register branch needs an id —
/// reuse touches nothing but a stamp, and adoption binds an existing row — so drawing it
/// up front would make a touch-only claim fail when entropy is down, even though that
/// claim writes no new computer and no credential. The draw is deferred into
/// [`LocalClaimAttempt::decide`]'s register arm, leaving the reuse and adopt paths
/// entropy-free.
///
/// Memoized because a registration that walks the name ladder retries under ONE identity:
/// the first register attempt mints the id, and every later candidate reuses it rather
/// than spending a second computer's worth of entropy. Attempts run sequentially within a
/// claim, so the mint runs once; the [`OnceLock`] only makes that structural.
#[derive(Clone)]
struct LazyComputerId {
    mint: Arc<dyn Fn() -> Result<String, EntropyFailure> + Send + Sync>,
    cache: Arc<OnceLock<String>>,
}

impl LazyComputerId {
    fn new(mint: impl Fn() -> Result<String, EntropyFailure> + Send + Sync + 'static) -> Self {
        Self {
            mint: Arc::new(mint),
            cache: Arc::new(OnceLock::new()),
        }
    }

    /// The id, minting it on first resolve and caching it for the rest of the claim.
    fn resolve(&self) -> Result<String, EntropyFailure> {
        if let Some(id) = self.cache.get() {
            return Ok(id.clone());
        }
        let minted = (self.mint)()?;
        Ok(self.cache.get_or_init(|| minted).clone())
    }
}

/// One attempt at a claim: everything the write needs, under one candidate name.
///
/// Private fields, and the only source is [`ClaimLocalComputer`]: the computer's id,
/// its name, the instant it is stamped with and the label it may store are all this
/// module's to mint, and an adapter that could name them would be a second writer with
/// its own idea of what a fresh computer looks like.
///
/// Held across the transaction rather than consumed by it, because the branch is not
/// known until the facts are read: the adapter reads them with [`Self::owner`] and
/// [`Self::installation_id`], then asks [`Self::decide`].
pub struct LocalClaimAttempt {
    owner: UserId,
    installation_id: String,
    at: i64,
    label: Option<String>,
    fresh_id: LazyComputerId,
    fresh_name: String,
}

impl LocalClaimAttempt {
    /// Whose claim this is. The requester, decided at the perimeter — never read back
    /// from a row.
    pub fn owner(&self) -> &UserId {
        &self.owner
    }

    /// The installation making the claim: the coordinate `claimed` is looked up by,
    /// and the one every branch writes onto the computer it answers with.
    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    /// The one instant this claim is stamped with. Every stamp a branch writes —
    /// last-seen, activation, finalization, creation — is this number, so a computer
    /// cannot be created before it was seen.
    pub fn at(&self) -> i64 {
        self.at
    }

    /// Which of the three answers these facts add up to.
    ///
    /// Reuse and adoption are pure — the branch a claim takes is testable without a
    /// database — but the register arm mints the fresh computer's id, the one fallible
    /// step, so it can be spent by exactly the path that needs it and no other. A failure
    /// here is an entropy failure, and only a claim that finds nothing to reuse or adopt
    /// can raise it.
    pub fn decide(&self, facts: &LocalClaimFacts) -> Result<LocalClaimWrite, EntropyFailure> {
        if let Some(claimed) = &facts.claimed {
            return Ok(LocalClaimWrite::TouchLastSeen(TouchLocalComputer {
                computer: claimed.clone(),
            }));
        }
        if let Some(unclaimed) = &facts.unclaimed_local {
            return Ok(LocalClaimWrite::Adopt(AdoptLocalComputer {
                computer: unclaimed.clone(),
                display_label: self.label.clone(),
            }));
        }
        Ok(LocalClaimWrite::Register(RegisterLocalComputer {
            id: self.fresh_id.resolve()?,
            name: self.fresh_name.clone(),
            display_label: self.label.clone(),
        }))
    }
}

/// What a claim writes, in the three shapes it can take.
///
/// The label's meaning differs between the two arms that carry it, and the difference
/// is a product rule rather than an accident:
///
/// - adopting, `None` means LEAVE THE COLUMN ALONE — a re-claim that sends no label
///   must not blank a name the user chose;
/// - registering, `None` means STORE NOTHING — there is no earlier name to keep.
///
/// The reused branch carries no label at all, structurally: renaming a computer the
/// installation already holds is `PATCH /api/hosts/:id`'s operation, and letting a
/// re-claim do it too would give one column two writers with different validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalClaimWrite {
    /// This installation already holds an active computer. Only its last-seen stamp
    /// moves; nothing about its custody changes.
    TouchLastSeen(TouchLocalComputer),
    /// An unclaimed placeholder is bound to this owner and installation.
    Adopt(AdoptLocalComputer),
    /// Nothing to reuse: a computer is created under the attempted name.
    Register(RegisterLocalComputer),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TouchLocalComputer {
    computer: HostId,
}

impl TouchLocalComputer {
    pub fn computer(&self) -> &HostId {
        &self.computer
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptLocalComputer {
    computer: HostId,
    display_label: Option<String>,
}

impl AdoptLocalComputer {
    pub fn computer(&self) -> &HostId {
        &self.computer
    }

    /// The label to store, or `None` to leave the stored one alone.
    pub fn display_label(&self) -> Option<&str> {
        self.display_label.as_deref()
    }
}

/// A computer to bring into existence.
///
/// Assembled from parts, an adapter would be choosing a computer's identity and name
/// for itself:
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::claim::RegisterLocalComputer;
/// let _: RegisterLocalComputer = RegisterLocalComputer {
///     id: "chosen-by-the-adapter".to_owned(),
///     name: "local".to_owned(),
///     display_label: None,
/// };
/// ```
///
/// The control for that gate: the same path, named the same way, reached the only way
/// it can be. A `compile_fail` block passes on ANY error, so without this the gate
/// above would also pass on a renamed type or a missing accessor.
///
/// ```
/// use silverhand_backend_core::host_runtime::claim::RegisterLocalComputer;
/// fn execute(write: &RegisterLocalComputer) -> (&str, &str, Option<&str>) {
///     (write.id(), write.name(), write.display_label())
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterLocalComputer {
    id: String,
    name: String,
    display_label: Option<String>,
}

impl RegisterLocalComputer {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The unique machine name this attempt claims. A row already holding it is the
    /// one outcome the adapter reports back rather than resolving.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The label to store, or `None` to store nothing.
    pub fn display_label(&self) -> Option<&str> {
        self.display_label.as_deref()
    }
}

#[derive(Debug, Error)]
pub enum LocalClaimWriteError {
    /// Another computer already holds the attempted name. The only refusal the
    /// adapter reports rather than resolves, because the next name to try is this
    /// module's to choose.
    #[error("computer name already exists")]
    DuplicateComputerName,
    /// The fresh computer's id could not be minted. Raised by [`LocalClaimAttempt::decide`]
    /// only when the branch is register, so a reuse or adopt never surfaces it.
    #[error(transparent)]
    Entropy(#[from] EntropyFailure),
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
}

/// The one way a claim reaches storage.
///
/// The two fact reads, the branch and the write are one transaction, taken with the
/// root write lock: every branch is conditioned on the absence of a row that a
/// concurrent claim from the same installation would create, so a read outside the
/// lock could be acted on after it stopped being true — two computers for one machine.
///
/// Returns the computer as it stands AFTER the write, re-read in the same transaction.
/// Projecting the record from the attempt instead would report stamps the row may not
/// carry: adopting keeps an existing activation stamp rather than overwriting it.
pub trait ClaimLocalComputerUnitOfWork: Clone + Send + Sync + 'static {
    fn claim_local_computer(
        &self,
        attempt: LocalClaimAttempt,
    ) -> impl Future<Output = Result<ComputerRecord, LocalClaimWriteError>> + Send;
}

/// A claimed computer, and the token its daemon needs when it needs one.
pub struct ClaimedLocalComputer {
    pub computer: ComputerRecord,
    pub host_token: Option<ConnectToken>,
}

#[derive(Debug, Error)]
pub enum ClaimLocalComputerError {
    #[error(transparent)]
    Entropy(#[from] EntropyFailure),
    #[error(transparent)]
    Clock(#[from] ClockFailure),
    #[error("unable to allocate a unique computer name")]
    NameAllocation,
    /// The computer the claim just resolved cannot hold a credential. Reachable only
    /// through a race — a revoke landing between this claim's commit and the issue —
    /// and kept typed because that race has a wire answer of its own.
    #[error("credential issuance refused: {0:?}")]
    CredentialRefused(CredentialIssueRefusal),
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
}

/// Claim the computer an installation runs on.
#[derive(Clone)]
pub struct ClaimLocalComputer<U, I, W, R, C> {
    unit_of_work: U,
    inventory: I,
    credentials: W,
    entropy: Arc<R>,
    clock: C,
}

impl<U, I, W, R, C> ClaimLocalComputer<U, I, W, R, C>
where
    U: ClaimLocalComputerUnitOfWork,
    I: HostCredentialInventory,
    W: CredentialIssueUnitOfWork,
    R: EntropySource + 'static,
    C: Clock,
{
    pub fn new(unit_of_work: U, inventory: I, credentials: W, entropy: R, clock: C) -> Self {
        Self {
            unit_of_work,
            inventory,
            credentials,
            entropy: Arc::new(entropy),
            clock,
        }
    }

    pub async fn execute(
        &self,
        owner: UserId,
        request: LocalClaimRequest,
    ) -> Result<ClaimedLocalComputer, ClaimLocalComputerError> {
        let at = now_ms(&self.clock)?;
        let fresh_id = self.lazy_computer_id();
        let prefix = installation_prefix(&request.installation_id);
        let label = request.label.as_ref().map(ComputerDisplayLabel::as_str);
        let base = normalize_computer_name(label.unwrap_or(LOCAL_COMPUTER_NAME));

        // The names a claim prefers, most specific answer first: what the user called
        // the machine, then the same name disambiguated by the installation, then the
        // default name disambiguated the same way. Deduplicated because a machine with
        // no label — or one labelled `local` — would otherwise be offered the same
        // name twice, and a name is either free for this claim or free for none of its
        // attempts.
        let mut ladder = vec![base.clone()];
        for candidate in [
            format!("{base}-{prefix}"),
            format!("{LOCAL_COMPUTER_NAME}-{prefix}"),
        ] {
            if !ladder.contains(&candidate) {
                ladder.push(candidate);
            }
        }

        for name in ladder {
            if let Some(computer) = self.attempt(&owner, &request, at, &fresh_id, name).await? {
                return self.finish(computer, &request, at).await;
            }
        }
        for _ in 0..NAME_SUFFIX_ATTEMPTS {
            let suffix = random_hex(self.entropy.as_ref(), NAME_SUFFIX_BYTES)?;
            let name = format!("{LOCAL_COMPUTER_NAME}-{prefix}-{suffix}");
            if let Some(computer) = self.attempt(&owner, &request, at, &fresh_id, name).await? {
                return self.finish(computer, &request, at).await;
            }
        }
        Err(ClaimLocalComputerError::NameAllocation)
    }

    /// The fresh computer's id, as a draw deferred until the register branch asks for it.
    /// The closure holds the same entropy source every other draw in this claim uses, so
    /// deferring changes only WHEN the id is minted, never from what.
    fn lazy_computer_id(&self) -> LazyComputerId {
        let entropy = Arc::clone(&self.entropy);
        LazyComputerId::new(move || random_base64url(entropy.as_ref(), COMPUTER_ID_BYTES))
    }

    /// One transaction. `Ok(None)` means the attempted name was taken and the next
    /// candidate is worth trying — the only outcome this operation retries.
    async fn attempt(
        &self,
        owner: &UserId,
        request: &LocalClaimRequest,
        at: i64,
        fresh_id: &LazyComputerId,
        fresh_name: String,
    ) -> Result<Option<ComputerRecord>, ClaimLocalComputerError> {
        let attempt = LocalClaimAttempt {
            owner: owner.clone(),
            installation_id: request.installation_id.clone(),
            at,
            label: request
                .label
                .as_ref()
                .map(|label| label.as_str().to_owned()),
            fresh_id: fresh_id.clone(),
            fresh_name,
        };
        match self.unit_of_work.claim_local_computer(attempt).await {
            Ok(computer) => Ok(Some(computer)),
            Err(LocalClaimWriteError::DuplicateComputerName) => Ok(None),
            Err(LocalClaimWriteError::Entropy(failure)) => Err(failure.into()),
            Err(LocalClaimWriteError::Persistence(failure)) => Err(failure.into()),
        }
    }

    /// Issue a token unless the caller demonstrably already has a working one.
    ///
    /// Two conditions, and the order between them is what keeps the common case cheap:
    /// a caller that named a DIFFERENT computer is always re-issued, so the inventory
    /// is not asked at all. The inventory is only a hint — losing the race costs a
    /// spare credential or a missing one the next claim re-issues, and never a token
    /// the server cannot verify.
    async fn finish(
        &self,
        computer: ComputerRecord,
        request: &LocalClaimRequest,
        at: i64,
    ) -> Result<ClaimedLocalComputer, ClaimLocalComputerError> {
        let already_attached = request.current_computer.as_ref() == Some(&computer.id);
        if already_attached
            && self
                .inventory
                .has_active_long_lived_credential(computer.id.clone(), at)
                .await?
        {
            return Ok(ClaimedLocalComputer {
                computer,
                host_token: None,
            });
        }
        let (credential, host_token) =
            mint_credential(self.entropy.as_ref(), computer.id.as_str())?.into_parts();
        match self.credentials.issue_credential(credential, None).await? {
            Ok(()) => Ok(ClaimedLocalComputer {
                computer,
                host_token: Some(host_token),
            }),
            Err(refusal) => Err(ClaimLocalComputerError::CredentialRefused(refusal)),
        }
    }
}

/// The part of an installation id a computer name is disambiguated with.
fn installation_prefix(installation_id: &str) -> String {
    installation_id
        .chars()
        .filter(|character| *character != '-')
        .take(INSTALLATION_PREFIX_CHARS)
        .collect()
}

/// Turn a name a human chose into the machine coordinate a computer is stored under:
/// lowercase, `[a-z0-9_-]` only, no leading or trailing dash, at most
/// [`COMPUTER_NAME_MAX_CHARS`], and never empty.
///
/// Includes the part that reads like a bug: the length cap is applied AFTER the dash
/// strip, so truncation can leave a trailing dash that the strip would have removed.
/// It is a stored coordinate — changing where the cap lands renames existing
/// computers.
///
/// The trim that opens the TypeScript version is absent, and cannot matter: every
/// whitespace character is outside the allowed set, so leading and trailing whitespace
/// becomes a leading and trailing dash that the strip removes anyway. Falsifier: allow
/// a whitespace character through the filter and the trim has to come back.
fn normalize_computer_name(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut in_disallowed_run = false;
    for character in value.to_lowercase().chars() {
        if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'
            || character == '-'
        {
            collapsed.push(character);
            in_disallowed_run = false;
        } else if !in_disallowed_run {
            collapsed.push('-');
            in_disallowed_run = true;
        }
    }
    // Every surviving character is ASCII, so the cap counts the same units the
    // JavaScript `slice` does.
    let stripped = collapsed.trim_matches('-');
    let capped = &stripped[..stripped.len().min(COMPUTER_NAME_MAX_CHARS)];
    if capped.is_empty() {
        LOCAL_COMPUTER_NAME.to_owned()
    } else {
        capped.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::host_runtime::{
        computer::{HostKind, ProvisioningSource},
        custody::NewLongLivedCredential,
    };

    const INSTALLATION: &str = "11111111-2222-3333-4444-555555555555";
    const INSTALLATION_PREFIX: &str = "11111111";
    const NOW: i64 = 1_700_000_000_123;

    fn owner() -> UserId {
        UserId::parse("user-1").unwrap()
    }

    fn computer(id: &str) -> HostId {
        HostId::parse(id).unwrap()
    }

    fn record(id: &str, name: &str) -> ComputerRecord {
        ComputerRecord {
            id: computer(id),
            name: name.to_owned(),
            kind: HostKind::Local,
            created_at: NOW,
            owner: Some(owner()),
            installation_id: Some(INSTALLATION.to_owned()),
            last_seen_at: Some(NOW),
            activated_at: Some(NOW),
            finalized_at: Some(NOW),
            revoked_at: None,
            provisioning_source: ProvisioningSource::AppInstall,
            display_label: None,
        }
    }

    /// Plays the adapter: reads its facts, asks the attempt what to write, and refuses
    /// names it has been told are taken. The decision under test is therefore reached
    /// the same way production reaches it.
    #[derive(Clone)]
    struct RecordingStore {
        facts: Arc<LocalClaimFacts>,
        taken: Arc<Vec<String>>,
        attempted: Arc<Mutex<Vec<String>>>,
        writes: Arc<Mutex<Vec<LocalClaimWrite>>>,
    }

    impl RecordingStore {
        fn new(facts: LocalClaimFacts, taken: &[&str]) -> Self {
            Self {
                facts: Arc::new(facts),
                taken: Arc::new(taken.iter().map(|name| (*name).to_owned()).collect()),
                attempted: Arc::new(Mutex::new(Vec::new())),
                writes: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn empty() -> Self {
            Self::new(LocalClaimFacts::default(), &[])
        }

        fn attempted(&self) -> Vec<String> {
            self.attempted.lock().unwrap().clone()
        }

        fn only_write(&self) -> LocalClaimWrite {
            let writes = self.writes.lock().unwrap();
            assert_eq!(writes.len(), 1, "exactly one write is expected");
            writes[0].clone()
        }
    }

    impl ClaimLocalComputerUnitOfWork for RecordingStore {
        async fn claim_local_computer(
            &self,
            attempt: LocalClaimAttempt,
        ) -> Result<ComputerRecord, LocalClaimWriteError> {
            assert_eq!(attempt.owner(), &owner());
            assert_eq!(attempt.installation_id(), INSTALLATION);
            assert_eq!(attempt.at(), NOW);

            let write = attempt.decide(&self.facts)?;
            if let LocalClaimWrite::Register(register) = &write {
                self.attempted
                    .lock()
                    .unwrap()
                    .push(register.name().to_owned());
                if self.taken.contains(&register.name().to_owned()) {
                    return Err(LocalClaimWriteError::DuplicateComputerName);
                }
            }
            self.writes.lock().unwrap().push(write.clone());
            Ok(match &write {
                LocalClaimWrite::TouchLastSeen(touch) => {
                    record(touch.computer().as_str(), "already-here")
                }
                LocalClaimWrite::Adopt(adopt) => {
                    record(adopt.computer().as_str(), LOCAL_COMPUTER_NAME)
                }
                LocalClaimWrite::Register(register) => record(register.id(), register.name()),
            })
        }
    }

    #[derive(Clone, Default)]
    struct RecordingCredentials {
        refusal: Option<CredentialIssueRefusal>,
        holds_active: bool,
        asked: Arc<AtomicUsize>,
        issued: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl RecordingCredentials {
        fn holding_an_active_credential() -> Self {
            Self {
                holds_active: true,
                ..Self::default()
            }
        }

        fn refusing(refusal: CredentialIssueRefusal) -> Self {
            Self {
                refusal: Some(refusal),
                ..Self::default()
            }
        }
    }

    impl HostCredentialInventory for RecordingCredentials {
        async fn has_active_long_lived_credential(
            &self,
            _computer: HostId,
            _now: i64,
        ) -> Result<bool, PersistenceFailure> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Ok(self.holds_active)
        }
    }

    impl CredentialIssueUnitOfWork for RecordingCredentials {
        async fn issue_credential(
            &self,
            credential: NewLongLivedCredential,
            sweep: Option<crate::host_runtime::custody::CredentialSweep>,
        ) -> Result<Result<(), CredentialIssueRefusal>, PersistenceFailure> {
            assert!(
                sweep.is_none(),
                "a claim issues a first token; it never sweeps one"
            );
            self.issued.lock().unwrap().push((
                credential.host_id().to_owned(),
                credential.token_hash().to_owned(),
            ));
            Ok(match self.refusal {
                Some(refusal) => Err(refusal),
                None => Ok(()),
            })
        }
    }

    struct FixedEntropy(Mutex<VecDeque<u8>>);

    impl FixedEntropy {
        fn bytes(count: usize) -> Self {
            Self(Mutex::new((0..count).map(|index| index as u8).collect()))
        }
    }

    impl EntropySource for FixedEntropy {
        type Error = Infallible;
        fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
            let mut bytes = self.0.lock().unwrap();
            for destination in destination {
                *destination = bytes.pop_front().unwrap();
            }
            Ok(())
        }
    }

    #[derive(Debug, Error)]
    #[error("the secure random source is down")]
    struct EntropyDown;

    /// An entropy source that fails every draw, so a test can prove a path never reached
    /// one.
    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        type Error = EntropyDown;
        fn fill(&self, _destination: &mut [u8]) -> Result<(), Self::Error> {
            Err(EntropyDown)
        }
    }

    struct FixedClock;

    impl Clock for FixedClock {
        type Error = Infallible;
        fn unix_time_ms(&self) -> Result<i64, Self::Error> {
            Ok(NOW)
        }
    }

    fn request(label: Option<&str>, current: Option<&str>) -> LocalClaimRequest {
        LocalClaimRequest {
            installation_id: INSTALLATION.to_owned(),
            label: label.map(|label| ComputerDisplayLabel::parse(label).unwrap()),
            current_computer: current.map(computer),
        }
    }

    fn claim(
        store: RecordingStore,
        credentials: RecordingCredentials,
    ) -> ClaimLocalComputer<
        RecordingStore,
        RecordingCredentials,
        RecordingCredentials,
        FixedEntropy,
        FixedClock,
    > {
        ClaimLocalComputer::new(
            store,
            credentials.clone(),
            credentials,
            FixedEntropy::bytes(1024),
            FixedClock,
        )
    }

    #[tokio::test]
    async fn a_computer_this_installation_already_holds_is_reused_untouched() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: Some(computer("h-existing")),
                unclaimed_local: Some(computer("h-placeholder")),
            },
            &[],
        );
        let service = claim(store.clone(), RecordingCredentials::default());

        let claimed = service
            .execute(owner(), request(Some("A Brand New Name"), None))
            .await
            .expect("the existing computer is reused");

        assert_eq!(claimed.computer.id.as_str(), "h-existing");
        assert_eq!(
            store.only_write(),
            LocalClaimWrite::TouchLastSeen(TouchLocalComputer {
                computer: computer("h-existing"),
            })
        );
        // The label reached the operation and went nowhere: reuse carries no label
        // field at all, and no name was allocated.
        assert!(store.attempted().is_empty());
    }

    #[tokio::test]
    async fn an_unclaimed_placeholder_is_adopted_and_takes_a_supplied_label() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: None,
                unclaimed_local: Some(computer("h-placeholder")),
            },
            &[],
        );
        let service = claim(store.clone(), RecordingCredentials::default());

        let claimed = service
            .execute(owner(), request(Some("  Shiki's Mac  "), None))
            .await
            .expect("the placeholder is adopted");

        assert_eq!(claimed.computer.id.as_str(), "h-placeholder");
        let LocalClaimWrite::Adopt(adopt) = store.only_write() else {
            panic!("an unclaimed placeholder is adopted");
        };
        assert_eq!(adopt.computer().as_str(), "h-placeholder");
        assert_eq!(adopt.display_label(), Some("Shiki's Mac"));
    }

    #[tokio::test]
    async fn an_adoption_without_a_label_emits_no_label_update() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: None,
                unclaimed_local: Some(computer("h-placeholder")),
            },
            &[],
        );
        let service = claim(store.clone(), RecordingCredentials::default());

        service
            .execute(owner(), request(None, None))
            .await
            .expect("the placeholder is adopted");

        let LocalClaimWrite::Adopt(adopt) = store.only_write() else {
            panic!("an unclaimed placeholder is adopted");
        };
        assert_eq!(adopt.display_label(), None);
    }

    #[tokio::test]
    async fn a_first_claim_registers_the_computer_under_its_normalized_label() {
        let store = RecordingStore::empty();
        let service = claim(store.clone(), RecordingCredentials::default());

        let claimed = service
            .execute(owner(), request(Some("Shiki's MacBook Pro!"), None))
            .await
            .expect("a computer is registered");

        assert_eq!(claimed.computer.id.as_str(), "AAECAwQFBgcICQoL");
        assert_eq!(claimed.computer.name, "shiki-s-macbook-pro");
        let LocalClaimWrite::Register(register) = store.only_write() else {
            panic!("nothing to reuse means a registration");
        };
        // The name is the coordinate, normalized; the label is what the user typed.
        assert_eq!(register.name(), "shiki-s-macbook-pro");
        assert_eq!(register.display_label(), Some("Shiki's MacBook Pro!"));
    }

    #[tokio::test]
    async fn a_taken_name_walks_the_ladder_from_specific_to_disambiguated() {
        let store = RecordingStore::new(
            LocalClaimFacts::default(),
            &["mac", &format!("mac-{INSTALLATION_PREFIX}")],
        );
        let service = claim(store.clone(), RecordingCredentials::default());

        let claimed = service
            .execute(owner(), request(Some("mac"), None))
            .await
            .expect("the third candidate is free");

        assert_eq!(
            claimed.computer.name,
            format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}")
        );
        assert_eq!(
            store.attempted(),
            vec![
                "mac".to_owned(),
                format!("mac-{INSTALLATION_PREFIX}"),
                format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}"),
            ]
        );
    }

    #[tokio::test]
    async fn an_unlabelled_claim_is_never_offered_the_same_name_twice() {
        let store = RecordingStore::new(
            LocalClaimFacts::default(),
            &[
                LOCAL_COMPUTER_NAME,
                &format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}"),
            ],
        );
        let service = claim(store.clone(), RecordingCredentials::default());

        let claimed = service
            .execute(owner(), request(None, None))
            .await
            .expect("a random suffix is free");

        // Without the deduplication the second and third candidates are the same
        // name, and one attempt of the eleven is spent proving what the previous one
        // already proved.
        assert_eq!(
            store.attempted(),
            vec![
                LOCAL_COMPUTER_NAME.to_owned(),
                format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}"),
                format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}-0c0d0e"),
            ]
        );
        assert_eq!(
            claimed.computer.name,
            format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}-0c0d0e")
        );
    }

    #[tokio::test]
    async fn a_labelled_claim_exhausts_three_names_and_eight_suffixes() {
        // Every name the ladder will ask for, refused: the three deterministic
        // candidates, then one per entropy-derived suffix.
        let mut taken = vec![
            "mac".to_owned(),
            format!("mac-{INSTALLATION_PREFIX}"),
            format!("{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}"),
        ];
        for suffix in [
            "0c0d0e", "0f1011", "121314", "151617", "18191a", "1b1c1d", "1e1f20", "212223",
        ] {
            taken.push(format!(
                "{LOCAL_COMPUTER_NAME}-{INSTALLATION_PREFIX}-{suffix}"
            ));
        }
        let refused: Vec<&str> = taken.iter().map(String::as_str).collect();
        let store = RecordingStore::new(LocalClaimFacts::default(), &refused);
        let service = claim(store.clone(), RecordingCredentials::default());

        assert!(matches!(
            service.execute(owner(), request(Some("mac"), None)).await,
            Err(ClaimLocalComputerError::NameAllocation)
        ));
        assert_eq!(store.attempted(), taken);
    }

    /// Falsifier for the lazy id: a reuse that already holds a working token writes no
    /// computer and no credential, so it must succeed even when entropy is down. Minting
    /// the id up front — the bug — turns this deterministic reuse into a `500`.
    #[tokio::test]
    async fn a_reuse_with_a_working_token_succeeds_even_when_entropy_is_down() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: Some(computer("h-existing")),
                unclaimed_local: None,
            },
            &[],
        );
        let credentials = RecordingCredentials::holding_an_active_credential();
        let service = ClaimLocalComputer::new(
            store.clone(),
            credentials.clone(),
            credentials.clone(),
            FailingEntropy,
            FixedClock,
        );

        let claimed = service
            .execute(owner(), request(None, Some("h-existing")))
            .await
            .expect("a reuse that needs no token never touches entropy");

        assert!(claimed.host_token.is_none());
        assert_eq!(credentials.asked.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.only_write(),
            LocalClaimWrite::TouchLastSeen(TouchLocalComputer {
                computer: computer("h-existing"),
            })
        );
        assert!(store.attempted().is_empty());
        assert!(credentials.issued.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_caller_already_on_this_computer_without_one_is_re_issued() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: Some(computer("h-existing")),
                unclaimed_local: None,
            },
            &[],
        );
        let credentials = RecordingCredentials::default();
        let service = claim(store, credentials.clone());

        let claimed = service
            .execute(owner(), request(None, Some("h-existing")))
            .await
            .expect("the existing computer is reused");

        let Some(token) = claimed.host_token else {
            panic!("a computer with no working credential is issued one");
        };
        assert_eq!(token.reveal_once().len(), 43);
        assert_eq!(credentials.asked.load(Ordering::SeqCst), 1);
        assert_eq!(
            credentials.issued.lock().unwrap()[0].0.as_str(),
            "h-existing"
        );
    }

    #[tokio::test]
    async fn a_caller_naming_another_computer_is_re_issued_without_asking() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: Some(computer("h-existing")),
                unclaimed_local: None,
            },
            &[],
        );
        let credentials = RecordingCredentials::holding_an_active_credential();
        let service = claim(store, credentials.clone());

        let claimed = service
            .execute(owner(), request(None, Some("h-somewhere-else")))
            .await
            .expect("the existing computer is reused");

        assert!(claimed.host_token.is_some());
        // The hint is not consulted: a caller holding the wrong computer's token
        // always needs this one's.
        assert_eq!(credentials.asked.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_revoked_credential_issue_verdict_is_propagated() {
        let store = RecordingStore::new(
            LocalClaimFacts {
                claimed: Some(computer("h-existing")),
                unclaimed_local: None,
            },
            &[],
        );
        let service = claim(
            store,
            RecordingCredentials::refusing(CredentialIssueRefusal::HostRevoked),
        );

        let Err(error) = service.execute(owner(), request(None, None)).await else {
            panic!("a refused credential is not a claim");
        };
        assert!(matches!(
            error,
            ClaimLocalComputerError::CredentialRefused(CredentialIssueRefusal::HostRevoked)
        ));
    }

    #[test]
    fn a_name_keeps_only_the_coordinate_characters() {
        assert_eq!(normalize_computer_name("local"), "local");
        assert_eq!(normalize_computer_name("  Shiki's Mac!!  "), "shiki-s-mac");
        assert_eq!(normalize_computer_name("Ünicode"), "nicode");
        assert_eq!(normalize_computer_name("---"), LOCAL_COMPUTER_NAME);
        assert_eq!(normalize_computer_name(""), LOCAL_COMPUTER_NAME);
        assert_eq!(normalize_computer_name("桌面"), LOCAL_COMPUTER_NAME);
        assert_eq!(normalize_computer_name("Keep_me-9"), "keep_me-9");
    }

    #[test]
    fn the_length_cap_lands_after_the_dash_strip_and_can_leave_one() {
        assert_eq!(normalize_computer_name(&"a".repeat(60)), "a".repeat(48));
        // Forty-seven `a`, a dash, then a `b`: the strip has nothing to remove, and
        // the cap cuts between the dash and the `b`. Moving the cap before the strip
        // would return forty-seven `a` instead — a different stored coordinate.
        let quirk = format!("{}-b", "a".repeat(47));
        assert_eq!(
            normalize_computer_name(&quirk),
            format!("{}-", "a".repeat(47))
        );
    }

    #[test]
    fn the_installation_prefix_drops_dashes_and_keeps_eight() {
        assert_eq!(installation_prefix(INSTALLATION), INSTALLATION_PREFIX);
        assert_eq!(installation_prefix("ab-cd"), "abcd");
    }
}
