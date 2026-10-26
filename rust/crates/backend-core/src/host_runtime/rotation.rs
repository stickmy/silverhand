//! Replacing a computer's connect credential.
//!
//! Two doors, and the difference between them is not the storage write — both mint one
//! credential and kill the rest — but WHO learns the new secret and how long the old one
//! keeps working:
//!
//! - A computer the app installed rotates its DAEMON's token. The daemon has to write
//!   the new one down before the old one dies, so the old one is superseded now and dies
//!   one grace window later, and the secret goes to whoever is going to store it: the
//!   daemon over its own connection when it is remote, the caller when the daemon shares
//!   this process.
//! - A computer the user brought rotates the CONNECT token the user holds. There is
//!   nothing to warn: the user is replacing a secret they believe is compromised, so the
//!   previous one dies at the same instant and any live socket is dropped.
//!
//! That is the whole of it. Neither door touches the computer row, and neither is a
//! lifecycle operation — a rotated computer is the same computer, in the same state,
//! reachable with a different secret.
//!
//! ## Why the remote arm is two phases
//!
//! Writing the new credential before the daemon has stored it would leave a computer
//! nobody can reach: the daemon still holds the old token, and the old token is now in a
//! grace window that expires. So the mint is separated from the write, the daemon is
//! told in between, and a daemon that does not acknowledge the rotation FOR THIS
//! COMPUTER leaves the database exactly as it was. The cost of that ordering is a minted
//! secret that is never stored, which is worth nothing to anyone.

use thiserror::Error;

use super::{
    aftermath::{CustodyEffectFailure, DaemonConnectionControl},
    custody::{
        ConnectToken, CredentialIssueRefusal, CredentialIssueUnitOfWork, CredentialSweep,
        NewLongLivedCredential, mint_credential,
    },
    environment::{Clock, ClockFailure, EntropyFailure, EntropySource, now_ms},
    lifecycle::{HostLifecycleStatus, lifecycle_status},
    read::OwnedComputer,
};
use crate::{
    foundation::identity::HostId,
    foundation::persistence::PersistenceFailure,
    host_runtime::computer::{ComputerRecord, HostKind},
};

/// Why an app-managed rotation is refused.
///
/// Custody is decided first: a revoked computer is answered as revoked whatever kind it
/// is, because that is the state the user has to fix and the kind is not.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum HostTokenRotationRefusal {
    #[error("host has been revoked")]
    CustodyEnded,
    /// Only the user's own machine runs a daemon this platform hands a token to. A
    /// remote computer's secret is the user's to hold, and it rotates through the
    /// connect-token door instead.
    #[error("rotate is only supported for local hosts")]
    NotALocalComputer,
}

/// Why a user-managed rotation is refused. The mirror image of the rule above, and
/// deliberately a separate vocabulary: the two doors refuse for opposite reasons and a
/// shared enum would have to carry both, letting one door answer the other's refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ConnectTokenRotationRefusal {
    #[error("host has been revoked")]
    CustodyEnded,
    #[error("connect-token rotation is only supported for user-managed computers")]
    NotUserManaged,
}

/// The mint, held back until the daemon has stored it.
///
/// Module-private, and that is the two-phase guarantee: the only things that can produce
/// one are the arms below, so no caller can hold a prepared rotation, skip the
/// acknowledgement, and complete it. The plaintext is NOT in here — it leaves with the
/// caller, so the value that reaches storage cannot also be the value that reaches a
/// response body.
struct PreparedHostTokenRotation {
    /// The verdict's computer. The credential's own host id is taken from this same
    /// value, so the row the write lands on and the daemon the token is sent to cannot
    /// be two different computers.
    computer: HostId,
    credential: NewLongLivedCredential,
    sweep: CredentialSweep,
    issued_at: i64,
}

/// A rotation the caller must store itself: the token, and the window the previous one
/// still works for.
///
/// Only the in-process arm returns this, because only there is the caller the thing that
/// keeps the daemon's config.
#[derive(Debug)]
pub struct RotatedHostToken {
    pub token: ConnectToken,
    pub issued_at: i64,
    /// When the previous token stops verifying.
    pub grace_until: i64,
}

/// A rotation the daemon already stored.
///
/// It carries NO token, and that is the point: on this arm the secret was revealed into
/// the daemon's own delivery, so there is no value here for a response body to leak.
///
/// The leak a response builder would write does not compile:
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::rotation::DaemonRotationReceipt;
/// fn leak(receipt: DaemonRotationReceipt) -> String {
///     receipt.token.reveal_once()
/// }
/// ```
///
/// The control for that gate: the same path, carrying exactly the three instants it is
/// allowed to carry. A `compile_fail` block passes on ANY error, so without this the gate
/// above would also pass on a renamed type.
///
/// ```
/// use silverhand_backend_core::host_runtime::rotation::DaemonRotationReceipt;
/// let receipt = DaemonRotationReceipt {
///     issued_at: 1,
///     grace_until: 61,
///     config_refreshed_at: 2,
/// };
/// assert_eq!(
///     (receipt.issued_at, receipt.grace_until, receipt.config_refreshed_at),
///     (1, 61, 2),
/// );
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonRotationReceipt {
    pub issued_at: i64,
    pub grace_until: i64,
    /// When the daemon says it refreshed its config. The daemon's own clock, kept for
    /// the operator's record and never compared against ours.
    pub config_refreshed_at: i64,
}

/// A fresh connect token for the user, and whether the daemon holding the old one could
/// be kicked off.
#[derive(Debug)]
pub struct RotatedConnectToken {
    pub token: ConnectToken,
    pub at: i64,
    /// Set when dropping the live connection failed. Best-effort by contract: the old
    /// token is already dead, so a daemon still attached is a socket that can no longer
    /// re-authenticate.
    pub daemon_disconnect_failure: Option<CustodyEffectFailure>,
}

/// The new token on its way to the daemon that will store it.
///
/// Private fields with the plaintext behind a consuming reveal, so a transport that
/// builds a frame from this cannot also log it. Read the coordinates first, then take
/// the secret last — the type enforces that order.
///
/// A second read of the secret does not compile:
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::rotation::RotatedTokenDelivery;
/// fn leak(delivery: RotatedTokenDelivery) -> (String, String) {
///     (delivery.reveal_token(), delivery.reveal_token())
/// }
/// ```
///
/// The control for that gate: the one order a frame builder can use. A `compile_fail`
/// block passes on ANY error, so without this the gate above would also pass on a
/// renamed type or a dropped accessor.
///
/// ```
/// use silverhand_backend_core::host_runtime::rotation::RotatedTokenDelivery;
/// fn frame(delivery: RotatedTokenDelivery) -> (String, i64, i64, String) {
///     let computer = delivery.computer().as_str().to_owned();
///     let issued_at = delivery.issued_at();
///     let grace_until = delivery.grace_until();
///     (computer, issued_at, grace_until, delivery.reveal_token())
/// }
/// let _: fn(RotatedTokenDelivery) -> (String, i64, i64, String) = frame;
/// ```
pub struct RotatedTokenDelivery {
    computer: HostId,
    token: ConnectToken,
    issued_at: i64,
    grace_until: i64,
}

impl RotatedTokenDelivery {
    pub fn computer(&self) -> &HostId {
        &self.computer
    }

    pub fn issued_at(&self) -> i64 {
        self.issued_at
    }

    /// When the token the daemon currently holds stops verifying. Sent so the daemon can
    /// see the deadline it is racing, not so it can decide anything.
    pub fn grace_until(&self) -> i64 {
        self.grace_until
    }

    /// The plaintext, once, for the frame.
    pub fn reveal_token(self) -> String {
        self.token.reveal_once()
    }
}

/// What the daemon said about a rotation it was handed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonRotationAck {
    /// It stored the token. The computer it names is checked against the one we sent to,
    /// because an acknowledgement from the wrong daemon is not an acknowledgement.
    Stored {
        computer: HostId,
        config_refreshed_at: i64,
    },
    /// It would not, in its own words. The prose message a daemon may add belongs in the
    /// adapter's log, not in this vocabulary: only the reason reaches a user.
    Refused { reason: String },
}

/// The way a rotation reaches the daemon that has to store it.
pub trait DaemonTokenRotationChannel: Clone + Send + Sync + 'static {
    /// The transport's own failure vocabulary, kept as an associated type because
    /// whether a failure means "nobody is connected" or "the connection broke" decides
    /// the status code, and that distinction belongs to the component that knows it.
    type Failure: std::error::Error + Send + Sync + 'static;

    /// Hand the token over and wait for the daemon's answer.
    ///
    /// A timeout is a failure of this port, not an ack: silence means the daemon may or
    /// may not have stored the token, and the only safe reading of "may not" is to leave
    /// the database alone and let the user rotate again.
    fn deliver_rotated_token(
        &self,
        delivery: RotatedTokenDelivery,
    ) -> impl Future<Output = Result<DaemonRotationAck, Self::Failure>> + Send;
}

/// Why an app-managed rotation produced nothing.
#[derive(Debug, Error)]
pub enum HostTokenRotationError {
    #[error(transparent)]
    Refused(#[from] HostTokenRotationRefusal),
    /// The in-lock verdict refused a computer this door had already admitted. Reachable
    /// only through a race with a revoke, and kept typed because that race has a wire
    /// answer of its own.
    #[error("credential issuance refused: {0:?}")]
    CredentialRefused(CredentialIssueRefusal),
    #[error(transparent)]
    Entropy(#[from] EntropyFailure),
    #[error(transparent)]
    Clock(#[from] ClockFailure),
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
}

/// Why a rotation through a daemon produced nothing.
///
/// Every arm below the first one leaves storage untouched, which is the property this
/// door exists to have.
#[derive(Debug, Error)]
pub enum DaemonRotationError<T>
where
    T: std::error::Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Custody(#[from] HostTokenRotationError),
    #[error("the rotation could not be delivered to the daemon")]
    Transport(#[source] T),
    #[error("the daemon refused the rotation: {reason}")]
    DaemonRefused { reason: String },
    #[error("the daemon acknowledged a rotation for {}", acknowledged.as_str())]
    DaemonHostMismatch { acknowledged: HostId },
}

/// Why a user-managed rotation produced nothing.
#[derive(Debug, Error)]
pub enum ConnectTokenRotationError {
    #[error(transparent)]
    Refused(#[from] ConnectTokenRotationRefusal),
    #[error("credential issuance refused: {0:?}")]
    CredentialRefused(CredentialIssueRefusal),
    #[error(transparent)]
    Entropy(#[from] EntropyFailure),
    #[error(transparent)]
    Clock(#[from] ClockFailure),
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
}

/// Rotate the token an app-installed computer's daemon authenticates with.
///
/// One type owns both arms because the custody rules, the mint and the grace window are
/// the same on both, and the only difference is who is told. Two use cases would each
/// carry a copy of the window, and a rotation whose grace disagreed with the daemon's
/// deadline is a computer that locks itself out.
#[derive(Clone)]
pub struct RotateHostToken<W, R, C> {
    credentials: W,
    entropy: R,
    clock: C,
}

impl<W, R, C> RotateHostToken<W, R, C>
where
    W: CredentialIssueUnitOfWork,
    R: EntropySource,
    C: Clock,
{
    pub fn new(credentials: W, entropy: R, clock: C) -> Self {
        Self {
            credentials,
            entropy,
            clock,
        }
    }

    /// The arm for a daemon that shares this process: nothing to tell over a wire, so
    /// the two phases run back to back and the token is revealed to the caller, which is
    /// the thing that writes the daemon's config.
    pub async fn in_process(
        &self,
        owned: OwnedComputer,
    ) -> Result<RotatedHostToken, HostTokenRotationError> {
        let (prepared, token) = self.prepare(owned)?;
        let issued_at = prepared.issued_at;
        let grace_until = prepared.sweep.revoked_at();
        self.complete(prepared).await?;
        Ok(RotatedHostToken {
            token,
            issued_at,
            grace_until,
        })
    }

    /// Mint, and decide the window. Writes nothing.
    ///
    /// [`admit_host_token_rotation`] runs here as a PRE-lock refusal — the same fast
    /// answer the route already gave — never as the in-lock authority. Beyond it, no
    /// fuller eligibility is pre-checked: the computer arrived owned, so it is
    /// bound to a user, and a second pre-lock copy of the eligibility rule would be one
    /// more place that could disagree with the write. The authority is the host re-read
    /// under the lock and judged by `judge_credential_eligibility` when `complete` issues
    /// the credential; that in-lock answer is the only one committed on.
    fn prepare(
        &self,
        owned: OwnedComputer,
    ) -> Result<(PreparedHostTokenRotation, ConnectToken), HostTokenRotationError> {
        let (access, record) = owned.into_parts();
        admit_host_token_rotation(&record)?;
        let issued_at = now_ms(&self.clock)?;
        let (credential, token) =
            mint_credential(&self.entropy, access.computer().as_str())?.into_parts();
        Ok((
            PreparedHostTokenRotation {
                computer: access.computer().clone(),
                credential,
                sweep: CredentialSweep::after_grace(issued_at),
                issued_at,
            },
            token,
        ))
    }

    /// The one write: insert the new credential and supersede the rest, atomically, and
    /// only if the computer is still allowed one.
    async fn complete(
        &self,
        prepared: PreparedHostTokenRotation,
    ) -> Result<(), HostTokenRotationError> {
        match self
            .credentials
            .issue_credential(prepared.credential, Some(prepared.sweep))
            .await?
        {
            Ok(()) => Ok(()),
            Err(refusal) => Err(HostTokenRotationError::CredentialRefused(refusal)),
        }
    }
}

/// Rotate a remote daemon's token, telling it before writing anything down.
#[derive(Clone)]
pub struct RotateHostTokenThroughDaemon<W, R, C, D> {
    rotation: RotateHostToken<W, R, C>,
    daemons: D,
}

impl<W, R, C, D> RotateHostTokenThroughDaemon<W, R, C, D>
where
    W: CredentialIssueUnitOfWork,
    R: EntropySource,
    C: Clock,
    D: DaemonTokenRotationChannel,
{
    pub fn new(rotation: RotateHostToken<W, R, C>, daemons: D) -> Self {
        Self { rotation, daemons }
    }

    pub async fn execute(
        &self,
        owned: OwnedComputer,
    ) -> Result<DaemonRotationReceipt, DaemonRotationError<D::Failure>> {
        let (prepared, token) = self.rotation.prepare(owned)?;
        let issued_at = prepared.issued_at;
        let grace_until = prepared.sweep.revoked_at();
        let computer = prepared.computer.clone();

        let ack = self
            .daemons
            .deliver_rotated_token(RotatedTokenDelivery {
                computer: computer.clone(),
                token,
                issued_at,
                grace_until,
            })
            .await
            .map_err(DaemonRotationError::Transport)?;

        let config_refreshed_at = match ack {
            DaemonRotationAck::Refused { reason } => {
                return Err(DaemonRotationError::DaemonRefused { reason });
            }
            DaemonRotationAck::Stored {
                computer: acknowledged,
                ..
            } if acknowledged != computer => {
                return Err(DaemonRotationError::DaemonHostMismatch { acknowledged });
            }
            DaemonRotationAck::Stored {
                config_refreshed_at,
                ..
            } => config_refreshed_at,
        };

        self.rotation.complete(prepared).await?;
        Ok(DaemonRotationReceipt {
            issued_at,
            grace_until,
            config_refreshed_at,
        })
    }
}

/// Rotate the connect token a user holds for a computer they brought.
#[derive(Clone)]
pub struct RotateConnectToken<W, R, C, D> {
    credentials: W,
    entropy: R,
    clock: C,
    daemons: D,
}

impl<W, R, C, D> RotateConnectToken<W, R, C, D>
where
    W: CredentialIssueUnitOfWork,
    R: EntropySource,
    C: Clock,
    D: DaemonConnectionControl,
{
    pub fn new(credentials: W, entropy: R, clock: C, daemons: D) -> Self {
        Self {
            credentials,
            entropy,
            clock,
            daemons,
        }
    }

    /// Mint, kill the old one at the same instant, then drop whatever connection was
    /// still using it.
    ///
    /// The disconnect runs AFTER the commit and cannot fail the rotation: the token the
    /// attached daemon is holding is already dead, so an unclosed socket is one that can
    /// no longer re-authenticate. Closing it only shortens the interval in which the
    /// daemon keeps trying.
    pub async fn execute(
        &self,
        owned: OwnedComputer,
    ) -> Result<RotatedConnectToken, ConnectTokenRotationError> {
        let (access, record) = owned.into_parts();
        admit_connect_token_rotation(&record)?;
        let at = now_ms(&self.clock)?;
        let computer = access.computer().clone();
        let (credential, token) = mint_credential(&self.entropy, computer.as_str())?.into_parts();

        match self
            .credentials
            .issue_credential(credential, Some(CredentialSweep::immediately(at)))
            .await?
        {
            Ok(()) => {}
            Err(refusal) => return Err(ConnectTokenRotationError::CredentialRefused(refusal)),
        }

        let daemon_disconnect_failure = self.daemons.disconnect(computer).await.err();
        Ok(RotatedConnectToken {
            token,
            at,
            daemon_disconnect_failure,
        })
    }
}

/// May this computer's daemon token be rotated?
///
/// Custody first, then kind, in the order the wire answers them.
///
/// Pure, and called BEFORE the write by two readers: the route's pre-branch refusal —
/// reached before the standalone arm decides whether a runtime is even attached — and
/// [`RotateHostToken::prepare`], which re-checks it before it mints. NEITHER is the
/// in-lock authority. What the write re-decides under the lock is narrower and lives in
/// the credential adapter: it re-reads the host inside the write transaction and runs
/// `judge_credential_eligibility`, refusing a computer revoked between a pre-lock read
/// and the write — the CUSTODY half of this rule, under a different name and refusal
/// type. The KIND half (`kind == Local`) is decided only here, before the write, and
/// safely: no committed transition changes a computer's `kind` after creation, so its
/// pre-lock read cannot go stale. Falsifier: add a transition that mutates `kind` and
/// this half has to move in-lock too.
pub fn admit_host_token_rotation(record: &ComputerRecord) -> Result<(), HostTokenRotationRefusal> {
    if lifecycle_status(&record.lifecycle()) == HostLifecycleStatus::Revoked {
        return Err(HostTokenRotationRefusal::CustodyEnded);
    }
    if record.kind != HostKind::Local {
        return Err(HostTokenRotationRefusal::NotALocalComputer);
    }
    Ok(())
}

/// May this computer's connect token be rotated?
fn admit_connect_token_rotation(
    record: &ComputerRecord,
) -> Result<(), ConnectTokenRotationRefusal> {
    if lifecycle_status(&record.lifecycle()) == HostLifecycleStatus::Revoked {
        return Err(ConnectTokenRotationRefusal::CustodyEnded);
    }
    if !record.is_user_managed() {
        return Err(ConnectTokenRotationRefusal::NotUserManaged);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        foundation::identity::UserId,
        host_runtime::{
            computer::ProvisioningSource, environment::base64url, read::find_owned_computer,
        },
    };

    const NOW: i64 = 1_700_000_000_123;
    const GRACE_MS: i64 = 60_000;

    #[derive(Debug, Error)]
    #[error("the clock is broken")]
    struct BrokenClock;

    #[derive(Clone, Copy, Debug, Default)]
    struct FixedClock;

    impl Clock for FixedClock {
        type Error = BrokenClock;
        fn unix_time_ms(&self) -> Result<i64, Self::Error> {
            Ok(NOW)
        }
    }

    /// Deterministic bytes, so a test can name the token it expects the daemon to be
    /// handed and the hash it expects to be stored.
    #[derive(Clone, Debug, Default)]
    struct CountingEntropy {
        next: Arc<Mutex<u8>>,
    }

    impl EntropySource for CountingEntropy {
        type Error = Infallible;
        fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
            let mut next = self.next.lock().expect("the counter is not poisoned");
            for slot in destination.iter_mut() {
                *slot = *next;
                *next = next.wrapping_add(1);
            }
            Ok(())
        }
    }

    #[derive(Debug, Error)]
    #[error("nobody is connected")]
    struct NobodyConnected;

    #[derive(Debug, Default)]
    struct Log {
        issued: Vec<(String, String, Option<CredentialSweep>)>,
        delivered: Vec<(String, String, i64, i64)>,
        disconnects: Vec<String>,
    }

    /// Plays the write side, the daemon channel and the connection control, so a test can
    /// see whether an aborted rotation reached storage.
    #[derive(Clone)]
    struct World {
        refusal: Option<CredentialIssueRefusal>,
        ack: Option<DaemonRotationAck>,
        disconnect_fails: bool,
        log: Arc<Mutex<Log>>,
    }

    impl World {
        fn new() -> Self {
            Self {
                refusal: None,
                ack: Some(DaemonRotationAck::Stored {
                    computer: HostId::parse("h1").expect("host id"),
                    config_refreshed_at: NOW + 12,
                }),
                disconnect_fails: false,
                log: Arc::default(),
            }
        }

        fn log(&self) -> std::sync::MutexGuard<'_, Log> {
            self.log.lock().expect("the log is not poisoned")
        }

        fn host_rotation(&self) -> RotateHostToken<Self, CountingEntropy, FixedClock> {
            RotateHostToken::new(self.clone(), CountingEntropy::default(), FixedClock)
        }

        fn through_daemon(
            &self,
        ) -> RotateHostTokenThroughDaemon<Self, CountingEntropy, FixedClock, Self> {
            RotateHostTokenThroughDaemon::new(self.host_rotation(), self.clone())
        }

        fn connect_rotation(&self) -> RotateConnectToken<Self, CountingEntropy, FixedClock, Self> {
            RotateConnectToken::new(
                self.clone(),
                CountingEntropy::default(),
                FixedClock,
                self.clone(),
            )
        }
    }

    impl CredentialIssueUnitOfWork for World {
        async fn issue_credential(
            &self,
            credential: NewLongLivedCredential,
            sweep: Option<CredentialSweep>,
        ) -> Result<Result<(), CredentialIssueRefusal>, PersistenceFailure> {
            self.log().issued.push((
                credential.host_id().to_owned(),
                credential.token_hash().to_owned(),
                sweep,
            ));
            Ok(match self.refusal {
                Some(refusal) => Err(refusal),
                None => Ok(()),
            })
        }
    }

    impl DaemonTokenRotationChannel for World {
        type Failure = NobodyConnected;
        async fn deliver_rotated_token(
            &self,
            delivery: RotatedTokenDelivery,
        ) -> Result<DaemonRotationAck, Self::Failure> {
            let computer = delivery.computer().as_str().to_owned();
            let issued_at = delivery.issued_at();
            let grace_until = delivery.grace_until();
            let token = delivery.reveal_token();
            self.log()
                .delivered
                .push((computer, token, issued_at, grace_until));
            self.ack.clone().ok_or(NobodyConnected)
        }
    }

    impl DaemonConnectionControl for World {
        async fn disconnect(&self, computer: HostId) -> Result<(), CustodyEffectFailure> {
            self.log().disconnects.push(computer.into_inner());
            if self.disconnect_fails {
                return Err(CustodyEffectFailure::from_source(NobodyConnected));
            }
            Ok(())
        }
    }

    fn record(
        kind: HostKind,
        provisioning_source: ProvisioningSource,
        revoked_at: Option<i64>,
    ) -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse("h1").expect("host id"),
            name: "local".to_owned(),
            kind,
            created_at: 1,
            owner: Some(UserId::parse("u1").expect("user id")),
            installation_id: Some("i1".to_owned()),
            last_seen_at: Some(1),
            activated_at: Some(1),
            finalized_at: Some(1),
            revoked_at,
            provisioning_source,
            display_label: None,
        }
    }

    fn app_managed(revoked_at: Option<i64>) -> ComputerRecord {
        record(HostKind::Local, ProvisioningSource::AppInstall, revoked_at)
    }

    fn user_managed(revoked_at: Option<i64>) -> ComputerRecord {
        record(HostKind::Remote, ProvisioningSource::UserInvite, revoked_at)
    }

    fn owned(row: ComputerRecord) -> OwnedComputer {
        let Some(owned) = find_owned_computer(&UserId::parse("u1").expect("user id"), Some(row))
        else {
            panic!("the owner is admitted");
        };
        owned
    }

    fn hash(token: &str) -> String {
        base64url(&Sha256::digest(token.as_bytes()))
    }

    #[tokio::test]
    async fn custody_is_answered_before_the_kind_is_looked_at() {
        let world = World::new();
        let error = world
            .host_rotation()
            .in_process(owned(record(
                HostKind::Remote,
                ProvisioningSource::UserInvite,
                Some(NOW - 1),
            )))
            .await
            .expect_err("a revoked computer rotates nothing");

        assert!(matches!(
            error,
            HostTokenRotationError::Refused(HostTokenRotationRefusal::CustodyEnded)
        ));
        assert!(world.log().issued.is_empty());
    }

    #[tokio::test]
    async fn a_remote_computer_has_no_daemon_token_to_rotate() {
        let world = World::new();
        let error = world
            .host_rotation()
            .in_process(owned(user_managed(None)))
            .await
            .expect_err("the daemon-token door is the local computer's");

        assert!(matches!(
            error,
            HostTokenRotationError::Refused(HostTokenRotationRefusal::NotALocalComputer)
        ));
        assert!(world.log().issued.is_empty());
    }

    #[tokio::test]
    async fn the_in_process_arm_hands_the_token_back_and_supersedes_after_the_grace() {
        let world = World::new();
        let rotated = world
            .host_rotation()
            .in_process(owned(app_managed(None)))
            .await
            .expect("the token rotates");

        assert_eq!(rotated.issued_at, NOW);
        assert_eq!(rotated.grace_until, NOW + GRACE_MS);

        let issued = world.log().issued.clone();
        let [(computer, token_hash, sweep)] = issued.as_slice() else {
            panic!("exactly one credential is written");
        };
        assert_eq!(computer, "h1");
        let sweep = sweep.expect("the previous token is superseded");
        assert_eq!(sweep.consumed_at(), NOW);
        assert_eq!(sweep.revoked_at(), NOW + GRACE_MS);
        // The secret the caller was handed is the one whose verifier was stored.
        assert_eq!(&hash(&rotated.token.reveal_once()), token_hash);
    }

    #[tokio::test]
    async fn the_daemon_is_told_the_secret_that_gets_stored_before_it_is_stored() {
        let world = World::new();
        let receipt = world
            .through_daemon()
            .execute(owned(app_managed(None)))
            .await
            .expect("the daemon stored it");

        assert_eq!(
            receipt,
            DaemonRotationReceipt {
                issued_at: NOW,
                grace_until: NOW + GRACE_MS,
                config_refreshed_at: NOW + 12,
            }
        );

        let log = world.log();
        let [(delivered_to, token, issued_at, grace_until)] = log.delivered.as_slice() else {
            panic!("exactly one delivery");
        };
        assert_eq!(delivered_to, "h1");
        assert_eq!(*issued_at, NOW);
        assert_eq!(*grace_until, NOW + GRACE_MS);

        let [(_, token_hash, _)] = log.issued.as_slice() else {
            panic!("exactly one credential is written");
        };
        assert_eq!(&hash(token), token_hash);
    }

    #[tokio::test]
    async fn a_daemon_that_refuses_leaves_the_credentials_alone() {
        let mut world = World::new();
        world.ack = Some(DaemonRotationAck::Refused {
            reason: "config_locked".to_owned(),
        });

        let error = world
            .through_daemon()
            .execute(owned(app_managed(None)))
            .await
            .expect_err("a refusal is not a rotation");

        let DaemonRotationError::DaemonRefused { reason } = error else {
            panic!("the daemon's reason reaches the user");
        };
        assert_eq!(reason, "config_locked");
        assert_eq!(world.log().delivered.len(), 1);
        assert!(world.log().issued.is_empty());
    }

    #[tokio::test]
    async fn an_ack_for_another_computer_is_not_an_ack() {
        let mut world = World::new();
        world.ack = Some(DaemonRotationAck::Stored {
            computer: HostId::parse("h2").expect("host id"),
            config_refreshed_at: NOW,
        });

        let error = world
            .through_daemon()
            .execute(owned(app_managed(None)))
            .await
            .expect_err("the wrong daemon answered");

        let DaemonRotationError::DaemonHostMismatch { acknowledged } = error else {
            panic!("the fault names who answered");
        };
        assert_eq!(acknowledged.as_str(), "h2");
        assert!(world.log().issued.is_empty());
    }

    #[tokio::test]
    async fn silence_from_the_transport_leaves_the_credentials_alone() {
        let mut world = World::new();
        world.ack = None;

        let error = world
            .through_daemon()
            .execute(owned(app_managed(None)))
            .await
            .expect_err("an undelivered rotation is not a rotation");

        assert!(matches!(error, DaemonRotationError::Transport(_)));
        assert!(world.log().issued.is_empty());
    }

    #[tokio::test]
    async fn a_revoked_credential_issue_verdict_is_propagated() {
        let mut world = World::new();
        world.refusal = Some(CredentialIssueRefusal::HostRevoked);

        let error = world
            .host_rotation()
            .in_process(owned(app_managed(None)))
            .await
            .expect_err("the in-lock verdict decides");

        assert!(matches!(
            error,
            HostTokenRotationError::CredentialRefused(CredentialIssueRefusal::HostRevoked)
        ));
    }

    #[tokio::test]
    async fn the_connect_token_door_is_the_user_managed_computers() {
        let world = World::new();
        let error = world
            .connect_rotation()
            .execute(owned(app_managed(None)))
            .await
            .expect_err("an app-managed computer rotates through the other door");

        assert!(matches!(
            error,
            ConnectTokenRotationError::Refused(ConnectTokenRotationRefusal::NotUserManaged)
        ));
        let log = world.log();
        assert!(log.issued.is_empty());
        assert!(log.disconnects.is_empty());
    }

    #[tokio::test]
    async fn a_revoked_user_managed_computer_is_answered_as_revoked() {
        let world = World::new();
        let error = world
            .connect_rotation()
            .execute(owned(user_managed(Some(NOW - 1))))
            .await
            .expect_err("a revoked computer rotates nothing");

        assert!(matches!(
            error,
            ConnectTokenRotationError::Refused(ConnectTokenRotationRefusal::CustodyEnded)
        ));
        assert!(world.log().issued.is_empty());
    }

    #[tokio::test]
    async fn a_rotated_connect_token_kills_the_old_one_at_once_and_drops_the_socket() {
        let world = World::new();
        let rotated = world
            .connect_rotation()
            .execute(owned(user_managed(None)))
            .await
            .expect("the connect token rotates");

        assert_eq!(rotated.at, NOW);
        assert!(rotated.daemon_disconnect_failure.is_none());

        let issued = world.log().issued.clone();
        let [(computer, token_hash, sweep)] = issued.as_slice() else {
            panic!("exactly one credential is written");
        };
        assert_eq!(computer, "h1");
        let sweep = sweep.expect("the previous token dies");
        assert_eq!(sweep.consumed_at(), NOW);
        assert_eq!(sweep.revoked_at(), NOW);
        assert_eq!(&hash(&rotated.token.reveal_once()), token_hash);
        assert_eq!(world.log().disconnects, vec!["h1".to_owned()]);
    }

    #[tokio::test]
    async fn a_socket_that_will_not_close_is_reported_and_the_rotation_stands() {
        let mut world = World::new();
        world.disconnect_fails = true;

        let rotated = world
            .connect_rotation()
            .execute(owned(user_managed(None)))
            .await
            .expect("the commit already happened");

        assert!(rotated.daemon_disconnect_failure.is_some());
        assert_eq!(world.log().issued.len(), 1);
    }
}
