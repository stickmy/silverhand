//! Reading computers, and the one place "no such computer" and "not yours" become
//! the same answer.
//!
//! Both read faces are keyed by the requester, so both could have been written as a
//! query that simply returns nothing. They are not: the query narrows, and
//! [`can_access_computer`] decides, once, for every route that names a computer.
//! A route that trusted the narrowing alone would be a second authorization
//! authority written in SQL, and the moment the two disagreed the disagreement would
//! be invisible.

use crate::{
    foundation::identity::{HostId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::capability::{
        computer_access::{ComputerAccess, ComputerAccessHeld, can_access_computer},
        read_tier::ReadTier,
    },
};

use super::computer::ComputerRecord;

/// The computer facts a read surface can ask for.
///
/// Neither method takes a verdict: they are fact sources, and the rules that judge
/// what comes back live here. `list_for_owner` is keyed by owner because the index
/// is, not because the key is the authorization.
pub trait ComputerReadModel: Clone + Send + Sync + 'static {
    /// Every computer this owner should see on the bulk list, in the order the
    /// product asks for: live rows before revoked ones, then oldest first.
    ///
    /// The adapter owns two contract clauses that are NOT re-decided here, because a
    /// second copy of either is a second authority with no production consumer:
    ///
    /// - **Order** — `revoked_at IS NULL` first, then `created_at ASC`.
    /// - **Draft-hiding** — a user-managed computer that has never finalized is
    ///   omitted. It is a half-finished invitation, and the bulk list is not where
    ///   the user watches one progress; [`Self::read_computer`] deliberately still
    ///   returns it so the add-computer flow can.
    ///
    /// Both clauses are a filter and an order, never a refusal: nothing here reports
    /// that a draft was withheld.
    fn list_for_owner(
        &self,
        owner: UserId,
    ) -> impl Future<Output = Result<Vec<ComputerRecord>, PersistenceFailure>> + Send;

    /// One computer by coordinate, whoever it belongs to.
    ///
    /// Unfiltered on purpose: the ownership question is
    /// [`find_owned_computer`]'s, and a query that pre-filtered by owner would make
    /// the absent row and the foreign row indistinguishable *before* the rule that
    /// is supposed to be the one deciding they are.
    fn read_computer(
        &self,
        computer: HostId,
    ) -> impl Future<Output = Result<Option<ComputerRecord>, PersistenceFailure>> + Send;
}

/// A computer record beside the verdict that admitted it.
///
/// The pair exists so a lifecycle operation cannot be handed a record on its own:
/// the writes in [`super::revoke`] and [`super::rotation`] require the witness, and
/// taking both from one value is what makes it impossible for the record being
/// changed and the computer that was authorized to be two different computers.
///
/// Assembling one from parts would manufacture exactly that:
///
/// ```compile_fail
/// use silverhand_backend_core::host_runtime::read::OwnedComputer;
/// let _: OwnedComputer = OwnedComputer {
///     access: unimplemented!(),
///     record: unimplemented!(),
/// };
/// ```
///
/// The control for that gate: the same path, named the same way, minted the only way
/// it can be. A `compile_fail` block passes on ANY error, so without this the gate
/// above would also pass on a renamed type.
///
/// ```
/// use silverhand_backend_core::{
///     host_runtime::{
///         computer::{ComputerRecord, HostKind, ProvisioningSource},
///         read::{OwnedComputer, find_owned_computer},
///     },
///     foundation::identity::{HostId, UserId},
/// };
/// let owner = UserId::parse("u1").unwrap();
/// let record = ComputerRecord {
///     id: HostId::parse("h1").unwrap(),
///     name: "local".to_owned(),
///     kind: HostKind::Local,
///     created_at: 1,
///     owner: Some(owner.clone()),
///     installation_id: None,
///     last_seen_at: None,
///     activated_at: Some(1),
///     finalized_at: Some(1),
///     revoked_at: None,
///     provisioning_source: ProvisioningSource::AppInstall,
///     display_label: None,
/// };
/// let Some(found) = find_owned_computer(&owner, Some(record)) else {
///     panic!("the owner is admitted");
/// };
/// let found: OwnedComputer = found;
/// assert_eq!(found.access().computer().as_str(), "h1");
/// assert_eq!(found.record().name, "local");
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedComputer {
    access: ComputerAccessHeld,
    record: ComputerRecord,
}

impl OwnedComputer {
    /// The verdict a write must present.
    pub fn access(&self) -> &ComputerAccessHeld {
        &self.access
    }

    /// The record, for the wire projection and for the lifecycle questions the
    /// stamps answer.
    pub fn record(&self) -> &ComputerRecord {
        &self.record
    }

    /// Both halves, for an operation that consumes the verdict and keeps the record.
    pub fn into_parts(self) -> (ComputerAccessHeld, ComputerRecord) {
        (self.access, self.record)
    }
}

/// Resolve a named computer for a requester.
///
/// Takes the record by value and the absence as `None`, so the caller's only job is
/// to read; deciding what an absent row means is this function's.
///
/// `None` is one answer covering two worlds — the row does not exist, and the row is
/// not this requester's — because every route built on this answers 404 for both.
/// Telling them apart here would let one route eventually answer 403 for the second,
/// and that answer confirms a computer id the requester was never shown.
pub fn find_owned_computer(
    requester: &UserId,
    record: Option<ComputerRecord>,
) -> Option<OwnedComputer> {
    let record = record?;
    match can_access_computer(requester, Some(&record.ownership())) {
        ComputerAccess::Held(access) => Some(OwnedComputer { access, record }),
        ComputerAccess::Refused(_) => None,
    }
}

/// The tier `viewer` reads a computer at by id, or `None` for the not-found answer.
///
/// The privileged tier is exactly [`can_access_computer`]; every other signed-in
/// human reads the public tier. An unfinalized user-invite draft is the owner's
/// alone: it is a half-finished invitation, not a computer anyone else can name.
/// Writes keep calling [`find_owned_computer`], so this rule widens none of them.
pub fn computer_read_tier(viewer: &UserId, record: &ComputerRecord) -> Option<ReadTier> {
    if let ComputerAccess::Held(_) = can_access_computer(viewer, Some(&record.ownership())) {
        return Some(ReadTier::Privileged);
    }
    let draft = record.is_user_managed() && record.finalized_at.is_none();
    (!draft).then_some(ReadTier::Public)
}

/// A row the owner-keyed query offered that the authorization rule then refused.
///
/// The narrowing is supposed to be a faithful subset: every row it yields must be one
/// [`can_access_computer`] admits. If it is not, the SQL and the rule have drifted,
/// and that is a fact-source defect, never a product refusal — nobody asked about
/// that computer. It surfaces named rather than as a silently dropped row, because a
/// list that quietly hides a foreign computer is a list that quietly showed one until
/// the day it did not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComputerListFactsMismatch {
    computer: HostId,
}

impl ComputerListFactsMismatch {
    /// The candidate whose verdict disagreed with the narrowing.
    pub fn computer(&self) -> &HostId {
        &self.computer
    }
}

/// Re-run the ONE authorization authority over the rows the narrowing produced.
///
/// The input order is carried verbatim: the port's ordering clause is the adapter's,
/// and re-sorting here would be a second, silent order. Rows are borrowed, so the
/// caller keeps the snapshot and the projector reads it once.
pub fn owned_computers<'a>(
    owner: &UserId,
    candidates: &'a [ComputerRecord],
) -> Result<Vec<&'a ComputerRecord>, ComputerListFactsMismatch> {
    let mut owned = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match can_access_computer(owner, Some(&candidate.ownership())) {
            ComputerAccess::Held(_) => owned.push(candidate),
            ComputerAccess::Refused(_) => {
                return Err(ComputerListFactsMismatch {
                    computer: candidate.id.clone(),
                });
            }
        }
    }
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_runtime::computer::{HostKind, ProvisioningSource};

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn record(id: &str, owner: Option<&str>) -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse(id).expect("host id"),
            name: id.to_owned(),
            kind: HostKind::Remote,
            created_at: 1,
            owner: owner.map(user),
            installation_id: None,
            last_seen_at: None,
            activated_at: Some(1),
            finalized_at: Some(1),
            revoked_at: None,
            provisioning_source: ProvisioningSource::UserInvite,
            display_label: None,
        }
    }

    #[test]
    fn the_owner_gets_the_record_and_a_verdict_naming_it() {
        let owner = user("u1");
        let Some(found) = find_owned_computer(&owner, Some(record("h1", Some("u1")))) else {
            panic!("the owner is admitted");
        };
        assert_eq!(found.access().computer(), &found.record().id);
        assert_eq!(found.access().owner(), &owner);
    }

    #[test]
    fn a_missing_row_and_a_foreign_row_are_the_same_answer() {
        let requester = user("u1");
        assert_eq!(find_owned_computer(&requester, None), None);
        assert_eq!(
            find_owned_computer(&requester, Some(record("h1", Some("u2")))),
            None
        );
    }

    #[test]
    fn an_unclaimed_computer_is_nobodys_to_name() {
        assert_eq!(
            find_owned_computer(&user("u1"), Some(record("h1", None))),
            None
        );
    }

    #[test]
    fn the_list_keeps_every_row_in_the_order_given() {
        let candidates = vec![
            record("h2", Some("u1")),
            record("h1", Some("u1")),
            record("h3", Some("u1")),
        ];
        let owned = owned_computers(&user("u1"), &candidates).expect("all rows are the owner's");
        let ids: Vec<&str> = owned.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["h2", "h1", "h3"]);
    }

    #[test]
    fn the_owner_reads_privileged_and_anyone_else_public() {
        let row = record("h1", Some("u1"));
        assert_eq!(
            computer_read_tier(&user("u1"), &row),
            Some(ReadTier::Privileged)
        );
        assert_eq!(
            computer_read_tier(&user("u2"), &row),
            Some(ReadTier::Public)
        );
    }

    #[test]
    fn an_unfinalized_invite_draft_is_the_owners_alone() {
        let mut row = record("h1", Some("u1"));
        row.finalized_at = None;
        assert_eq!(
            computer_read_tier(&user("u1"), &row),
            Some(ReadTier::Privileged)
        );
        assert_eq!(computer_read_tier(&user("u2"), &row), None);
    }

    #[test]
    fn a_foreign_row_in_the_narrowing_is_a_named_defect_not_a_quiet_drop() {
        let candidates = vec![record("h1", Some("u1")), record("h2", Some("u2"))];
        let mismatch =
            owned_computers(&user("u1"), &candidates).expect_err("the narrowing drifted");
        assert_eq!(mismatch.computer().as_str(), "h2");
    }
}
