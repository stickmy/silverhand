//! The computer write axis: delete a computer and everything rooted on it.
//!
//! One `BEGIN IMMEDIATE` reserves the single writer, then the three authorizations
//! are re-asked UNDER the lock in the order the witness chain fixes — is it theirs
//! ([`can_access_computer`]), may its lifecycle end ([`hold_computer_lifecycle`]),
//! does its dependency set admit the cascade ([`admit_cascade`]) — from facts read
//! inside the lock, never from a pre-flight hint. Access and lifecycle refusals both
//! collapse to `ComputerGone`: the two are indistinguishable to the requester, and
//! the conditional host-row delete re-states the same legality in its own `WHERE`, so
//! a lost race there rolls the whole scope back rather than reporting a delete this
//! request did not cause.
//!
//! Foreign keys do the fan-out. Every admitted agent row is deleted through the
//! single-agent primitive, and only then the host row — `agents.host_id` is
//! `ON DELETE RESTRICT`, so the host cannot go first. A full-teardown dependent has
//! its post-commit work captured before its delete erases the rows that work reads.

use silverhand_backend_core::{
    application_orchestration::teardown::{
        cascade::{
            CascadeAdmission, CascadeConsent, ComputerLifecycle, admit_cascade,
            hold_computer_lifecycle,
        },
        contract::TeardownFailure,
        eligibility::{AgentTeardownEligibility, EligibilityVerdict, hold_eligibility},
        write::{AgentTornDown, ComputerCascade, ComputerCascadeCommit, ComputerTeardownKey},
    },
    foundation::persistence::PersistenceFailure,
    identity_access::capability::computer_access::{ComputerAccess, can_access_computer},
};

use crate::foundation::host_agent_store::SqliteHostAgentStore;

use super::{
    facts::{
        capture_agent, select_agents_on_host, select_computer_lifecycle, select_computer_ownership,
    },
    unit_of_work::DELETE_AGENT,
};

/// Re-states the legality the witness chain established, so a computer that stopped
/// being a user-managed remote this requester owns matches nothing here and the
/// scope rolls back. The witness answers are the optimization; this `WHERE` is the
/// guarantee.
const DELETE_OWNED_USER_INVITE: &str = "DELETE FROM hosts \
     WHERE id = ? AND user_id = ? AND kind = 'remote' AND provisioning_source = 'user_invite'";

pub(super) async fn cascade_computer(
    store: &SqliteHostAgentStore,
    key: ComputerTeardownKey,
    consent: CascadeConsent,
) -> Result<ComputerCascadeCommit, TeardownFailure> {
    let mut tx = store
        .begin_root_write()
        .await
        .map_err(PersistenceFailure::from_source)?;

    let ownership = select_computer_ownership(&mut tx, &key.host_id)
        .await
        .map_err(PersistenceFailure::from_source)?;
    let ComputerAccess::Held(access) = can_access_computer(&key.user_id, ownership.as_ref()) else {
        return Ok(ComputerCascadeCommit::ComputerGone);
    };

    let lifecycle = match select_computer_lifecycle(&mut tx, &key.host_id)
        .await
        .map_err(PersistenceFailure::from_source)?
    {
        Some(facts) => facts,
        None => return Ok(ComputerCascadeCommit::ComputerGone),
    };
    let ComputerLifecycle::Held(lifecycle) = hold_computer_lifecycle(access, lifecycle)? else {
        return Ok(ComputerCascadeCommit::ComputerGone);
    };

    let dependents = select_agents_on_host(&mut tx, &key.host_id)
        .await
        .map_err(PersistenceFailure::from_source)?;
    let admitted = match admit_cascade(lifecycle, dependents, consent)? {
        CascadeAdmission::Refused(refused) => return Ok(ComputerCascadeCommit::Refused(refused)),
        CascadeAdmission::Admitted(admitted) => admitted,
    };

    let expected = AgentTeardownEligibility::ActiveOn {
        host_id: key.host_id.clone(),
    };
    let mut torn_down = Vec::new();
    for agent in admitted.full_teardowns() {
        let id = agent.id.clone();
        let EligibilityVerdict::Held(held) = hold_eligibility(&expected, agent) else {
            unreachable!("admit_cascade classified this dependent as active on this computer");
        };
        let capture = capture_agent(&mut tx, &id)
            .await
            .map_err(PersistenceFailure::from_source)?;
        sqlx::query(DELETE_AGENT)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
        torn_down.push(AgentTornDown::seal(&id, held, capture)?);
    }
    for agent in admitted.row_only() {
        sqlx::query(DELETE_AGENT)
            .bind(agent.id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(PersistenceFailure::from_source)?;
    }

    let deleted = sqlx::query(DELETE_OWNED_USER_INVITE)
        .bind(key.host_id.as_str())
        .bind(key.user_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(PersistenceFailure::from_source)?;
    if deleted.rows_affected() != 1 {
        return Ok(ComputerCascadeCommit::ComputerGone);
    }

    let cascade = ComputerCascade::seal(&key, admitted, torn_down)?;
    tx.commit().await.map_err(PersistenceFailure::from_source)?;
    Ok(ComputerCascadeCommit::Committed(cascade))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use silverhand_backend_core::{
        application_orchestration::teardown::cascade::CascadeRefused,
        foundation::identity::{HostId, UserId},
    };
    use sqlx::Connection;
    use tempfile::TempDir;

    use super::*;

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn key(host_id: &str, user_id: &str) -> ComputerTeardownKey {
        ComputerTeardownKey {
            host_id: host(host_id),
            user_id: user(user_id),
        }
    }

    async fn seed_schema(dir: &TempDir, name: &str, seed: &[&'static str]) -> PathBuf {
        let path = dir.path().join(name);
        let mut connection = crate::current_schema::create_current_schema(&path).await;
        for statement in seed {
            sqlx::query(*statement)
                .execute(&mut connection)
                .await
                .expect("seed statement");
        }
        connection.close().await.expect("close seed connection");
        path
    }

    /// The reservation fails a contended cascade with SQLITE_BUSY (code 5), walking
    /// the source chain because the axis hands the driver error over as
    /// `PersistenceFailure`'s source inside `TeardownFailure`.
    fn is_sqlite_busy(error: &(dyn std::error::Error + 'static)) -> bool {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(err) = current {
            if let Some(sqlx_error) = err.downcast_ref::<sqlx::Error>()
                && sqlx_error
                    .as_database_error()
                    .and_then(|db| db.code().map(|code| code.into_owned()))
                    .as_deref()
                    == Some("5")
            {
                return true;
            }
            current = err.source();
        }
        false
    }

    const USER: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u1', 'u1', 'U1', 1, 1)";
    const OTHER_USER: &str = "INSERT INTO users (id, name, display_name, created_at, updated_at) \
         VALUES ('u2', 'u2', 'U2', 1, 1)";
    /// The one deletable shape: a remote machine this user invited.
    const INVITED_REMOTE: &str = "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
         VALUES ('h1', 'h1', 'remote', 1, 'u1', 'user_invite')";
    /// Remote, but installed rather than invited — the provisioning-source axis alone
    /// takes it out of the user-managed set.
    const APP_INSTALL_REMOTE: &str = "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
         VALUES ('h1', 'h1', 'remote', 1, 'u1', 'app_install')";
    /// Invited, but local — the kind axis alone takes it out of the user-managed set.
    const LOCAL_USER_INVITE: &str = "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
         VALUES ('h1', 'h1', 'local', 1, 'u1', 'user_invite')";
    /// A user-managed remote, but owned by someone other than the requester.
    const OTHER_OWNED_REMOTE: &str = "INSERT INTO hosts (id, name, kind, created_at, user_id, provisioning_source) \
         VALUES ('h1', 'h1', 'remote', 1, 'u2', 'user_invite')";
    const DEPENDENT: &str = seed_agent!(id = "a1", host_id = "h1", created_by_user_id = "u1");

    async fn count(store: &SqliteHostAgentStore, statement: &'static str) -> i64 {
        sqlx::query_scalar(statement)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    /// The deterministic writer-reservation gate. A no-write answer (`ComputerGone`,
    /// from an absent host) succeeds while nothing is held, then fails at
    /// `begin_root_write` with SQLITE_BUSY once the single writer is held.
    ///
    /// Falsifier: change the store's `BEGIN IMMEDIATE` to `BEGIN DEFERRED` and this
    /// test turns RED — a deferred begin takes no writer lock, so the no-write answer
    /// returns `Ok(ComputerGone)` even while a writer is held, and `unwrap_err` panics.
    #[tokio::test]
    async fn cascade_reserves_the_writer_before_its_first_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(&dir, "cascade-reservation.db", &[USER]).await;
        let store = SqliteHostAgentStore::open_with_busy_timeout(&path, Duration::from_millis(50))
            .await
            .unwrap();

        let idle = cascade_computer(&store, key("missing", "u1"), CascadeConsent::Confirmed)
            .await
            .unwrap();
        assert!(matches!(idle, ComputerCascadeCommit::ComputerGone));

        let held = store.begin_root_write().await.unwrap();
        let contended = cascade_computer(&store, key("missing", "u1"), CascadeConsent::Confirmed)
            .await
            .unwrap_err();
        assert!(
            is_sqlite_busy(&contended),
            "the cascade did not reserve the writer before its first read"
        );
        held.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn an_owned_invited_remote_with_confirmed_consent_cascades() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(
            &dir,
            "cascade-commit.db",
            &[USER, INVITED_REMOTE, DEPENDENT],
        )
        .await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let commit = cascade_computer(&store, key("h1", "u1"), CascadeConsent::Confirmed)
            .await
            .unwrap();
        assert!(matches!(commit, ComputerCascadeCommit::Committed(_)));

        // The agent went first (its FK to hosts is ON DELETE RESTRICT), then the host.
        assert_eq!(
            count(&store, "SELECT count(*) FROM agents WHERE id = 'a1'").await,
            0
        );
        assert_eq!(
            count(&store, "SELECT count(*) FROM hosts WHERE id = 'h1'").await,
            0
        );
    }

    /// `kind` and `provisioning_source` are two columns the adapter decodes
    /// independently, and the deletable computer is their conjunction. Moving either
    /// axis alone off the user-managed value yields `ComputerGone` and writes nothing,
    /// which a single pre-merged token could not distinguish.
    #[tokio::test]
    async fn the_two_user_managed_axes_are_read_and_required_independently() {
        for (name, seed) in [
            ("app-install.db", APP_INSTALL_REMOTE),
            ("local-invite.db", LOCAL_USER_INVITE),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = seed_schema(&dir, name, &[USER, seed]).await;
            let store = SqliteHostAgentStore::open(&path).await.unwrap();

            let commit = cascade_computer(&store, key("h1", "u1"), CascadeConsent::Confirmed)
                .await
                .unwrap();
            assert!(
                matches!(commit, ComputerCascadeCommit::ComputerGone),
                "{name} should not be a user-managed computer"
            );
            assert_eq!(
                count(&store, "SELECT count(*) FROM hosts WHERE id = 'h1'").await,
                1,
                "{name} must not have been deleted"
            );
        }
    }

    #[tokio::test]
    async fn an_unconfirmed_cascade_with_a_dependent_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(
            &dir,
            "cascade-consent.db",
            &[USER, INVITED_REMOTE, DEPENDENT],
        )
        .await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        let commit = cascade_computer(&store, key("h1", "u1"), CascadeConsent::Absent)
            .await
            .unwrap();
        let ComputerCascadeCommit::Refused(CascadeRefused::ConsentRequired { dependents }) = commit
        else {
            panic!("expected a consent-required refusal, got {commit:?}");
        };
        assert_eq!(
            dependents
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a1"]
        );
        // A refusal writes nothing: the host and its dependent are still here.
        assert_eq!(
            count(&store, "SELECT count(*) FROM hosts WHERE id = 'h1'").await,
            1
        );
        assert_eq!(
            count(&store, "SELECT count(*) FROM agents WHERE id = 'a1'").await,
            1
        );
    }

    #[tokio::test]
    async fn a_computer_owned_by_another_user_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_schema(
            &dir,
            "cascade-access.db",
            &[USER, OTHER_USER, OTHER_OWNED_REMOTE],
        )
        .await;
        let store = SqliteHostAgentStore::open(&path).await.unwrap();

        // Ownership is read from the real row: u1 is not the owner, so the access
        // witness never holds and the axis answers `ComputerGone` before any write.
        let commit = cascade_computer(&store, key("h1", "u1"), CascadeConsent::Confirmed)
            .await
            .unwrap();
        assert!(matches!(commit, ComputerCascadeCommit::ComputerGone));
        assert_eq!(
            count(&store, "SELECT count(*) FROM hosts WHERE id = 'h1'").await,
            1
        );
    }
}
