//! The one authority over a host connect credential's whole life.
//!
//! Minting, hashing, presenting and judging a connect token all live here, and the
//! reason is that they are one secret's lifecycle rather than four operations that
//! happen to share a column. A second minter would decide token length or encoding
//! for itself; a second hasher would decide what "the same token" means; a second
//! verdict would decide when a rotated token stops working. Each of those is a way
//! for a credential to be accepted by one half of the system and rejected by the
//! other.
//!
//! ## Nothing else can bring a credential into existence
//!
//! [`NewLongLivedCredential`] has private fields and no public constructor, and the
//! only function in the crate that returns one is [`mint_credential`]. So a route
//! cannot assemble a credential from a token it chose, and a persistence adapter
//! cannot be handed one that was never minted — the insert signature can only be
//! satisfied by this module's output.
//!
//! ## The plaintext exists as a return value and nowhere else
//!
//! [`ConnectToken`] is reveal-once by type: no `Clone`, `Display`, `Deref`, `AsRef`,
//! equality or serialization, and a `Debug` that redacts. It can be moved to exactly
//! one consumer, which must destroy it to read it. The persisted half is the hash,
//! and the two are minted together so no caller can persist one secret and reveal
//! another.

use std::fmt;

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{
    computer::ComputerRecord,
    environment::{EntropyFailure, EntropySource, base64url, random_base64url},
};
use crate::{
    foundation::identity::{HostId, UserId},
    foundation::persistence::PersistenceFailure,
};

const CREDENTIAL_ID_BYTES: usize = 12;
const CONNECT_TOKEN_BYTES: usize = 32;

/// How long a rotated-away token keeps working.
///
/// The window exists because the daemon holding the old token has to be told about
/// the new one over a live connection and then write it down. Revoking the old token
/// at the instant the new one is inserted would disconnect every rotating daemon and
/// leave it unable to reconnect.
pub const ROTATION_GRACE_MS: i64 = 60_000;

/// Persistence input for a long-lived credential. Its type fixes the lifecycle to
/// active, long-lived and unconsumed; no caller can construct another state.
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::NewLongLivedCredential;
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<NewLongLivedCredential>();
/// ```
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::NewLongLivedCredential;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<NewLongLivedCredential>();
/// ```
///
/// Assembled from parts, this would persist a verifier for a secret no mint
/// produced — the token could then be anything at all, including a value chosen by
/// the request:
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::NewLongLivedCredential;
/// let _ = NewLongLivedCredential {
///     id: "c1".to_owned(),
///     host_id: "h1".to_owned(),
///     token_hash: "chosen-by-the-caller".to_owned(),
/// };
/// ```
pub struct NewLongLivedCredential {
    id: String,
    host_id: String,
    token_hash: String,
}

impl NewLongLivedCredential {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn host_id(&self) -> &str {
        &self.host_id
    }
    pub fn token_hash(&self) -> &str {
        &self.token_hash
    }
}

/// Reveal-once credential. Its public surface intentionally has no `Clone`,
/// `Display`, `Deref`, serialization, or equality implementation.
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::ConnectToken;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<ConnectToken>();
/// ```
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::ConnectToken;
/// fn requires_display<T: std::fmt::Display>() {}
/// requires_display::<ConnectToken>();
/// ```
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::ConnectToken;
/// fn requires_equality<T: PartialEq>() {}
/// requires_equality::<ConnectToken>();
/// ```
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::ConnectToken;
/// fn requires_deref<T: std::ops::Deref<Target = str>>() {}
/// requires_deref::<ConnectToken>();
/// ```
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::ConnectToken;
/// fn requires_as_ref<T: AsRef<str>>() {}
/// requires_as_ref::<ConnectToken>();
/// ```
pub struct ConnectToken(String);

impl ConnectToken {
    pub fn reveal_once(self) -> String {
        self.0
    }
}

impl fmt::Debug for ConnectToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConnectToken([REDACTED])")
    }
}

/// A freshly minted credential: the verifier to persist and the plaintext to
/// reveal, produced together.
///
/// They are one value because the pairing is the invariant. Two separate returns
/// could be crossed — a hash from one mint persisted next to another mint's
/// plaintext — and the result would be a token the daemon holds and the server
/// cannot verify, with no failing request to point at.
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::custody::MintedCredential;
/// let _ = MintedCredential { credential: unimplemented!(), token: unimplemented!() };
/// ```
pub struct MintedCredential {
    credential: NewLongLivedCredential,
    token: ConnectToken,
}

impl MintedCredential {
    pub fn credential(&self) -> &NewLongLivedCredential {
        &self.credential
    }

    pub fn into_parts(self) -> (NewLongLivedCredential, ConnectToken) {
        (self.credential, self.token)
    }
}

impl fmt::Debug for MintedCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MintedCredential")
            .field("credential_id", &self.credential.id)
            .field("host_id", &self.credential.host_id)
            .finish_non_exhaustive()
    }
}

/// Mint the one kind of credential this platform issues, hashing the plaintext
/// before returning it.
///
/// The only source of host connect credentials, and therefore the one place that
/// decides the token's length and encoding, the credential id's, and the hash.
/// Invitation, claim and both rotations take their credential from this call, which
/// is what makes a token minted by one operation verifiable by the code path another
/// operation's daemon will use.
///
/// Takes no lifecycle parameters: every credential issued over HTTP is an active,
/// unconsumed, non-expiring long-lived one, and offering the other combinations here
/// would make "what kind of credential did we just issue" a per-caller decision.
pub fn mint_credential<R: EntropySource>(
    entropy: &R,
    host_id: &str,
) -> Result<MintedCredential, EntropyFailure> {
    let id = random_base64url(entropy, CREDENTIAL_ID_BYTES)?;
    let raw_token = random_base64url(entropy, CONNECT_TOKEN_BYTES)?;
    Ok(MintedCredential {
        credential: NewLongLivedCredential {
            id,
            host_id: host_id.to_owned(),
            token_hash: hash_host_token(&raw_token),
        },
        token: ConnectToken(raw_token),
    })
}

/// A token as it arrived from a request: trimmed, non-empty, and never compared as
/// plaintext against anything.
struct PresentedConnectToken(String);

impl PresentedConnectToken {
    fn parse(raw: String) -> Option<Self> {
        let normalized = raw.trim();
        (!normalized.is_empty()).then(|| Self(normalized.to_owned()))
    }

    fn hash(&self) -> String {
        hash_host_token(&self.0)
    }
}

impl fmt::Debug for PresentedConnectToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PresentedConnectToken([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialKind {
    LongLived,
    Bootstrap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialAuthState {
    pub credential_id: String,
    pub host_id: String,
    pub host_user_id: Option<UserId>,
    pub kind: CredentialKind,
    pub expires_at: Option<i64>,
    pub consumed_at: Option<i64>,
    pub credential_revoked_at: Option<i64>,
    pub host_revoked_at: Option<i64>,
}

pub trait HostCredentialLookup: Clone + Send + Sync + 'static {
    fn lookup_by_token_hash(
        &self,
        token_hash: String,
    ) -> impl Future<Output = Result<Option<CredentialAuthState>, PersistenceFailure>> + Send;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostTokenRejection {
    Missing,
    CredentialNotUsable,
    CredentialExpired,
    CredentialRevoked,
    HostRevoked,
    HostUnbound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerifyHostTokenFact {
    Accepted {
        host_id: String,
        user_id: UserId,
        credential_id: String,
    },
    Rejected(HostTokenRejection),
}

/// Judge a presented token against the persisted verifier.
///
/// The grace clause is the load-bearing part: a credential with `revoked_at` set is
/// still usable while `consumed_at` is set and `revoked_at` is in the FUTURE. That is
/// how [`ROTATION_GRACE_MS`] is expressed in storage, and a reader that treated
/// `revoked_at IS NOT NULL` as "invalid" would reject every daemon in its rotation
/// window.
pub async fn verify_host_token<S: HostCredentialLookup>(
    store: &S,
    raw_token: String,
    now: i64,
) -> Result<VerifyHostTokenFact, PersistenceFailure> {
    let Some(token) = PresentedConnectToken::parse(raw_token) else {
        return Ok(VerifyHostTokenFact::Rejected(HostTokenRejection::Missing));
    };
    let Some(state) = store.lookup_by_token_hash(token.hash()).await? else {
        return Ok(VerifyHostTokenFact::Rejected(HostTokenRejection::Missing));
    };
    if state.kind != CredentialKind::LongLived {
        return Ok(VerifyHostTokenFact::Rejected(
            HostTokenRejection::CredentialNotUsable,
        ));
    }
    if let Some(revoked_at) = state.credential_revoked_at {
        if state.consumed_at.is_none() {
            return Ok(VerifyHostTokenFact::Rejected(
                HostTokenRejection::CredentialRevoked,
            ));
        }
        if revoked_at <= now {
            return Ok(VerifyHostTokenFact::Rejected(
                HostTokenRejection::CredentialExpired,
            ));
        }
    } else if state.consumed_at.is_some() {
        return Ok(VerifyHostTokenFact::Rejected(
            HostTokenRejection::CredentialNotUsable,
        ));
    }
    if state.expires_at.is_some_and(|expires_at| expires_at <= now) {
        return Ok(VerifyHostTokenFact::Rejected(
            HostTokenRejection::CredentialExpired,
        ));
    }
    if state.host_revoked_at.is_some() {
        return Ok(VerifyHostTokenFact::Rejected(
            HostTokenRejection::HostRevoked,
        ));
    }
    let Some(user_id) = state.host_user_id else {
        return Ok(VerifyHostTokenFact::Rejected(
            HostTokenRejection::HostUnbound,
        ));
    };
    Ok(VerifyHostTokenFact::Accepted {
        host_id: state.host_id,
        user_id,
        credential_id: state.credential_id,
    })
}

/// This credential's computer is not one whose coordinate a daemon may learn.
///
/// ONE refusal for every reason — gone, platform-owned, unclaimed, custody ended —
/// because the caller here holds only a token: telling it WHICH of those is true
/// would let a stolen or stale token map out the fleet it came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("this credential's computer may not be resolved")]
pub struct ConnectTokenResolutionRefused;

/// May the daemon holding this verified credential be told which computer it is?
///
/// The bootstrap door's second half. A copied connection command carries no computer
/// id, so the daemon proves possession first ([`verify_host_token`]) and learns its
/// coordinate here — which is why the resolvable computer is only the USER-MANAGED
/// one: an app install already knows its own id from the claim that registered it, so
/// resolving one over a token would answer a question nobody needs to ask.
///
/// Custody and binding are judged AGAIN, and this is not a repeat of the verify: that
/// verdict came from the credential's own join, this one from a later read of the
/// computer. A row revoked in between must refuse rather than resolve, and the only
/// way to see that is to look at the newer facts.
pub fn admit_connect_token_resolution(
    computer: Option<&ComputerRecord>,
) -> Result<(), ConnectTokenResolutionRefused> {
    let Some(computer) = computer else {
        return Err(ConnectTokenResolutionRefused);
    };
    if !computer.is_user_managed() || computer.owner.is_none() || computer.revoked_at.is_some() {
        return Err(ConnectTokenResolutionRefused);
    }
    Ok(())
}

/// The two host facts that decide whether it may receive a credential at all.
///
/// Deliberately narrower than the host's DTO facts: adding a column to `hosts` must
/// not be able to become an input to credential issuance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCredentialEligibilityFacts {
    pub user_id: Option<UserId>,
    pub revoked_at: Option<i64>,
}

/// Why a host may not receive a credential.
///
/// Absent and unbound are kept apart from revoked because only revoked is a state a
/// user caused and can be told about; the other two mean the coordinate the request
/// named does not identify a host that can own a secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialIssueRefusal {
    HostMissing,
    HostUnbound,
    HostRevoked,
}

/// May this host receive a credential?
///
/// Pure, so the same rule answers the pre-lock hint and the authoritative in-lock
/// read. Only the in-lock answer may be committed on: a host revoked between the two
/// reads would otherwise be issued a working token.
pub fn judge_credential_eligibility(
    facts: Option<&HostCredentialEligibilityFacts>,
) -> Result<(), CredentialIssueRefusal> {
    let Some(facts) = facts else {
        return Err(CredentialIssueRefusal::HostMissing);
    };
    if facts.user_id.is_none() {
        return Err(CredentialIssueRefusal::HostUnbound);
    }
    if facts.revoked_at.is_some() {
        return Err(CredentialIssueRefusal::HostRevoked);
    }
    Ok(())
}

/// When the host's OTHER long-lived credentials stop working, as the two numbers
/// [`verify_host_token`] reads back.
///
/// The pair is a value with private fields and exactly two constructors because it is
/// the WRITE half of the grace clause: `consumed_at` marks a credential as superseded,
/// and `revoked_at` says when being superseded turns into being dead. Every other
/// combination of the two is a state the reader cannot make sense of — a future
/// `revoked_at` with no `consumed_at` is a credential the reader rejects outright, and
/// a past `revoked_at` with a `consumed_at` is one it treats as expired — so those
/// combinations are not offered.
///
/// There is no "only sweep the unexpired ones" option. Such a guard could only change the outcome for a long-lived credential that carries an
/// expiry, and [`mint_credential`] is the only source of long-lived credentials and
/// takes no expiry — the state the guard protects against is unreachable here.
/// Falsifier: give long-lived credentials an expiry and the guard has to come back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialSweep {
    revoked_at: i64,
    consumed_at: i64,
}

impl CredentialSweep {
    /// Superseded and dead at the same instant: no daemon keeps working on an older
    /// token.
    ///
    /// What connect-token rotation needs, because the point of that operation is that
    /// whoever held the previous token loses it — the user is rotating a secret they
    /// believe is compromised, so a grace window would be the one thing they asked
    /// against.
    pub fn immediately(now: i64) -> Self {
        Self {
            revoked_at: now,
            consumed_at: now,
        }
    }

    /// Superseded now, dead one [`ROTATION_GRACE_MS`] later.
    ///
    /// What host-token rotation needs: the daemon holding the previous token has to be
    /// told the new one over a live connection and write it down, and killing the old
    /// one at the instant the new one lands would disconnect it before it could.
    ///
    /// The window is computed here rather than accepted as a parameter, which is why
    /// this port has no equivalent of `completePreparedHostTokenRotation`'s
    /// `graceUntil <= issuedAt` guard: a caller cannot ask for a window that has
    /// already closed.
    pub fn after_grace(issued_at: i64) -> Self {
        Self {
            revoked_at: issued_at + ROTATION_GRACE_MS,
            consumed_at: issued_at,
        }
    }

    /// When the swept credentials stop verifying. Also the instant the daemon is told
    /// its old token expires, when a rotation has one to tell.
    pub fn revoked_at(&self) -> i64 {
        self.revoked_at
    }

    /// When they were superseded. Never overwrites an earlier one: a credential
    /// already in a grace window keeps the instant it was first superseded.
    pub fn consumed_at(&self) -> i64 {
        self.consumed_at
    }
}

/// The one way a credential reaches storage.
///
/// One method for all three issuing operations — the first credential of a claim and
/// both rotations — because they differ only in whether the host's other credentials
/// are swept and when. Separate methods would each own a copy of "insert, then decide
/// what happens to the previous token", and the copies are exactly where a rotation
/// that leaves the old token alive comes from.
///
/// ## The verdict is the judge's, produced under the lock
///
/// The return type is [`judge_credential_eligibility`]'s own, and that is deliberate:
/// the adapter re-reads the host's [`HostCredentialEligibilityFacts`] inside the write
/// transaction and calls that same function. A named outcome type here would be a
/// second vocabulary for the same rule, free to drift from it. A host revoked between
/// a route's read and this write is refused by the in-lock answer, and the route's
/// own earlier check is only a fast path.
pub trait CredentialIssueUnitOfWork: Clone + Send + Sync + 'static {
    /// Insert `credential` and, when `sweep` is present, apply it to every OTHER
    /// long-lived credential of the same host — atomically, and only if the host is
    /// still eligible.
    ///
    /// The sweep carries no credential id: the one credential kept is always the one
    /// being inserted, so a caller cannot sweep the token it just issued.
    fn issue_credential(
        &self,
        credential: NewLongLivedCredential,
        sweep: Option<CredentialSweep>,
    ) -> impl Future<Output = Result<Result<(), CredentialIssueRefusal>, PersistenceFailure>> + Send;
}

/// Whether a computer already holds a credential its daemon could still use.
///
/// A HINT, not an authorization, and separate from [`CredentialIssueUnitOfWork`] so
/// that is visible at the call site: it is read outside the write transaction, and it
/// decides only whether an operation bothers to mint. Racing it costs a redundant
/// token or a missing one the next claim re-issues; nothing it says may stand in for
/// the in-lock verdict.
pub trait HostCredentialInventory: Clone + Send + Sync + 'static {
    /// Is there a long-lived credential for this computer that is unconsumed,
    /// unrevoked, unexpired, and belongs to a live computer someone owns?
    ///
    /// "Unconsumed" excludes a credential inside its rotation grace window: it still
    /// verifies, but it is the token being replaced, and treating it as current is how
    /// a re-claiming daemon would be handed nothing at all.
    fn has_active_long_lived_credential(
        &self,
        computer: HostId,
        now: i64,
    ) -> impl Future<Output = Result<bool, PersistenceFailure>> + Send;
}

/// SHA-256, base64url without padding. Stored verifiers depend on this exact encoding.
///
/// Private on purpose. A caller able to hash could compare a presented token itself
/// and skip the verdict above, or persist a verifier for a token it chose.
fn hash_host_token(token: &str) -> String {
    base64url(&Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, convert::Infallible, sync::Mutex};

    use thiserror::Error;

    use super::*;
    use crate::host_runtime::{
        computer::{HostKind, ProvisioningSource},
        environment::OsEntropy,
    };

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
    #[error("entropy unavailable")]
    struct EntropyUnavailable;

    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        type Error = EntropyUnavailable;
        fn fill(&self, _destination: &mut [u8]) -> Result<(), Self::Error> {
            Err(EntropyUnavailable)
        }
    }

    #[test]
    fn a_mint_pairs_one_plaintext_with_the_verifier_for_that_plaintext() {
        let minted = mint_credential(&FixedEntropy::bytes(64), "host-1").unwrap();
        assert_eq!(minted.credential().host_id(), "host-1");
        assert_eq!(minted.credential().id(), "AAECAwQFBgcICQoL");
        let (credential, token) = minted.into_parts();
        let raw = token.reveal_once();

        // Independent base64url and SHA-256 vectors for entropy bytes 12..44.
        assert_eq!(raw, "DA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKis");
        assert_eq!(
            credential.token_hash(),
            "AxP1ZVhTWFkBwbwNPuuRTxGPDCBKB8T-MT20HD4DRaQ"
        );
    }

    #[test]
    fn two_sampled_mints_have_distinct_credentials() {
        let first = mint_credential(&OsEntropy, "host-1").unwrap();
        let second = mint_credential(&OsEntropy, "host-1").unwrap();
        assert_ne!(first.credential().id(), second.credential().id());
        assert_ne!(
            first.credential().token_hash(),
            second.credential().token_hash()
        );
    }

    #[test]
    fn a_mint_failure_yields_no_credential_at_all() {
        assert!(mint_credential(&FailingEntropy, "host-1").is_err());
    }

    #[test]
    fn neither_the_token_nor_the_minted_pair_can_be_printed() {
        let minted = mint_credential(&FixedEntropy::bytes(64), "host-1").unwrap();
        let rendered = format!("{minted:?}");
        assert!(rendered.contains("host-1"));
        assert!(!rendered.contains("token"));
        let (_, token) = minted.into_parts();
        assert_eq!(format!("{token:?}"), "ConnectToken([REDACTED])");
    }

    #[test]
    fn a_presented_token_is_redacted_and_normalized() {
        let presented = PresentedConnectToken::parse("  spaced  ".to_owned()).unwrap();
        assert_eq!(
            format!("{presented:?}"),
            "PresentedConnectToken([REDACTED])"
        );
        assert_eq!(presented.hash(), hash_host_token("spaced"));
        assert!(PresentedConnectToken::parse("   ".to_owned()).is_none());
        assert!(PresentedConnectToken::parse(String::new()).is_none());
    }

    #[test]
    fn sha256_hash_matches_the_node_base64url_contract() {
        assert_eq!(
            hash_host_token("host token"),
            "AaArGqMOKkFjJjNtp11rIN4CVXC7R5Hhxa1iE3_W-b0"
        );
    }

    fn eligible(user: Option<&str>, revoked_at: Option<i64>) -> HostCredentialEligibilityFacts {
        HostCredentialEligibilityFacts {
            user_id: user.map(|raw| UserId::parse(raw).unwrap()),
            revoked_at,
        }
    }

    #[test]
    fn credential_eligibility_separates_the_three_refusals() {
        assert_eq!(
            judge_credential_eligibility(None),
            Err(CredentialIssueRefusal::HostMissing)
        );
        assert_eq!(
            judge_credential_eligibility(Some(&eligible(None, None))),
            Err(CredentialIssueRefusal::HostUnbound)
        );
        assert_eq!(
            judge_credential_eligibility(Some(&eligible(Some("u1"), Some(5)))),
            Err(CredentialIssueRefusal::HostRevoked)
        );
        assert_eq!(
            judge_credential_eligibility(Some(&eligible(Some("u1"), None))),
            Ok(())
        );
    }

    /// Unbound is checked before revoked: a host with no owner has nobody the
    /// refusal is about, so reporting it as revoked would name a state the caller
    /// cannot act on.
    #[test]
    fn an_unbound_revoked_host_reports_unbound() {
        assert_eq!(
            judge_credential_eligibility(Some(&eligible(None, Some(5)))),
            Err(CredentialIssueRefusal::HostUnbound)
        );
    }

    #[derive(Clone)]
    struct OneCredential(CredentialAuthState);

    impl HostCredentialLookup for OneCredential {
        async fn lookup_by_token_hash(
            &self,
            _token_hash: String,
        ) -> Result<Option<CredentialAuthState>, PersistenceFailure> {
            Ok(Some(self.0.clone()))
        }
    }

    /// The state a swept credential is left in: the sweep's two numbers, written to
    /// the columns the verdict reads.
    fn swept_by(sweep: CredentialSweep) -> OneCredential {
        OneCredential(CredentialAuthState {
            credential_id: "c1".to_owned(),
            host_id: "h1".to_owned(),
            host_user_id: Some(UserId::parse("u1").unwrap()),
            kind: CredentialKind::LongLived,
            expires_at: None,
            consumed_at: Some(sweep.consumed_at()),
            credential_revoked_at: Some(sweep.revoked_at()),
            host_revoked_at: None,
        })
    }

    async fn verdict_at(store: &OneCredential, now: i64) -> VerifyHostTokenFact {
        verify_host_token(store, "the-old-token".to_owned(), now)
            .await
            .expect("the stub cannot fail")
    }

    /// The grace window only exists if the writer's encoding is the one the reader
    /// decodes. Falsifier: swap [`CredentialSweep::after_grace`] for
    /// [`CredentialSweep::immediately`] and the rotating daemon is locked out the
    /// instant its replacement token is minted.
    #[tokio::test]
    async fn a_grace_sweep_leaves_the_old_token_working_until_the_window_closes() {
        let issued_at = 1_700_000_000_000;
        let sweep = CredentialSweep::after_grace(issued_at);
        assert_eq!(sweep.consumed_at(), issued_at);
        assert_eq!(sweep.revoked_at(), issued_at + 60_000);
        let store = swept_by(sweep);

        assert!(matches!(
            verdict_at(&store, issued_at + 1).await,
            VerifyHostTokenFact::Accepted { .. }
        ));
        assert_eq!(
            verdict_at(&store, issued_at + 59_999).await,
            VerifyHostTokenFact::Accepted {
                host_id: "h1".to_owned(),
                user_id: UserId::parse("u1").unwrap(),
                credential_id: "c1".to_owned(),
            }
        );
        assert_eq!(
            verdict_at(&store, issued_at + 60_000).await,
            VerifyHostTokenFact::Rejected(HostTokenRejection::CredentialExpired)
        );
    }

    #[tokio::test]
    async fn an_immediate_sweep_kills_the_old_token_at_once() {
        let now = 1_700_000_000_000;
        let store = swept_by(CredentialSweep::immediately(now));
        assert_eq!(
            verdict_at(&store, now).await,
            VerifyHostTokenFact::Rejected(HostTokenRejection::CredentialExpired)
        );
    }

    fn computer(
        kind: HostKind,
        provisioning_source: ProvisioningSource,
        owner: Option<&str>,
        revoked_at: Option<i64>,
    ) -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse("h1").unwrap(),
            name: "h1".to_owned(),
            kind,
            created_at: 1,
            owner: owner.map(|raw| UserId::parse(raw).unwrap()),
            installation_id: None,
            last_seen_at: None,
            activated_at: Some(1),
            finalized_at: Some(1),
            revoked_at,
            provisioning_source,
            display_label: None,
        }
    }

    fn invited(owner: Option<&str>, revoked_at: Option<i64>) -> ComputerRecord {
        computer(
            HostKind::Remote,
            ProvisioningSource::UserInvite,
            owner,
            revoked_at,
        )
    }

    #[test]
    fn a_live_user_managed_computer_may_be_resolved_over_its_token() {
        assert_eq!(
            admit_connect_token_resolution(Some(&invited(Some("u1"), None))),
            Ok(())
        );
    }

    /// The four causes are asserted as one VALUE, not four arms: the refusal carries no
    /// data, so a holder that tried every token it has still cannot tell a computer that
    /// vanished from one it was never allowed to ask about.
    #[test]
    fn every_reason_a_computer_is_unresolvable_gives_the_same_answer() {
        let refused = Err(ConnectTokenResolutionRefused);
        assert_eq!(admit_connect_token_resolution(None), refused);
        assert_eq!(
            admit_connect_token_resolution(Some(&computer(
                HostKind::Remote,
                ProvisioningSource::AppInstall,
                Some("u1"),
                None,
            ))),
            refused
        );
        assert_eq!(
            admit_connect_token_resolution(Some(&computer(
                HostKind::Local,
                ProvisioningSource::UserInvite,
                Some("u1"),
                None,
            ))),
            refused
        );
        assert_eq!(
            admit_connect_token_resolution(Some(&invited(None, None))),
            refused
        );
        assert_eq!(
            admit_connect_token_resolution(Some(&invited(Some("u1"), Some(5)))),
            refused
        );
    }
}
