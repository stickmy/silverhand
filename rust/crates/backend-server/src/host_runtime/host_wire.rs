//! The one projection from a computer to its wire DTO.
//!
//! Three surfaces return a computer — the create-invite response, the owner list, and
//! the by-id read — and every one of them renders it here. A field's wire shape has a
//! single definition, and the derived `status`/`placeable` pair is read off the
//! lifecycle stamps by [`lifecycle_status`] exactly once, so the two can never disagree
//! between surfaces.
//!
//! The fixed defaults a fresh invitation carries (a remote, user-invited computer with
//! no stamps) live here in the adapter rather than in the core fact, which is why the
//! invite path materializes them next to the projector instead of the application layer
//! threading them through. `status`/`placeable` are still DERIVED for that path, from
//! all-absent stamps, so "a fresh invite is pending and not placeable" is a consequence
//! of the same rule the read path uses, not a second hand-written pair.

use serde::Serialize;
use silverhand_backend_core::host_runtime::{
    computer::{ComputerRecord, HostKind, ProvisioningSource},
    invite::CreatedUserInviteHost,
    lifecycle::{HostLifecycleStamps, HostLifecycleStatus, lifecycle_status},
};

#[derive(Serialize)]
pub(crate) struct HostDto {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub activated_at: Option<i64>,
    pub finalized_at: Option<i64>,
    pub connected_host_name: Option<String>,
    pub provisioning_source: &'static str,
    pub status: &'static str,
    pub placeable: bool,
    pub creator_user_id: Option<String>,
    pub display_label: Option<String>,
}

/// The public tier of the by-id read: what any signed-in human may see. A separate
/// type rather than nulled fields, so a privileged column cannot leak by omission.
#[derive(Serialize)]
pub(crate) struct HostPublicDto {
    pub id: String,
    pub name: String,
    pub display_label: Option<String>,
    pub kind: &'static str,
    pub status: &'static str,
    pub creator_user_id: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

impl From<HostDto> for HostPublicDto {
    fn from(dto: HostDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name,
            display_label: dto.display_label,
            kind: dto.kind,
            status: dto.status,
            creator_user_id: dto.creator_user_id,
            created_at: dto.created_at,
            last_seen_at: dto.last_seen_at,
        }
    }
}

/// Project a persisted computer record for the wire.
///
/// `connected_host_name` is left absent on purpose: it is diagnostic liveness the
/// record does not carry, and the one route that shows it overlays a live attachment
/// name over this base. Every other field is projected as stored, except the
/// status/placeable pair, which is derived from the stamps.
pub(crate) fn host_wire(record: &ComputerRecord) -> HostDto {
    let status = lifecycle_status(&record.lifecycle());
    HostDto {
        id: record.id.as_str().to_owned(),
        name: record.name.clone(),
        kind: host_kind_wire(record.kind),
        created_at: record.created_at,
        revoked_at: record.revoked_at,
        last_seen_at: record.last_seen_at,
        activated_at: record.activated_at,
        finalized_at: record.finalized_at,
        connected_host_name: None,
        provisioning_source: provisioning_source_wire(record.provisioning_source),
        status: status_wire(status),
        placeable: status.placeable(),
        creator_user_id: record.owner.as_ref().map(|owner| owner.as_str().to_owned()),
        display_label: record.display_label.clone(),
    }
}

impl From<CreatedUserInviteHost> for HostDto {
    fn from(host: CreatedUserInviteHost) -> Self {
        // A fresh invitation is a remote, user-invited computer that has not been heard
        // from: no stamps, so the lifecycle rule places it Pending and unplaceable. It
        // is derived here rather than written down so it cannot drift from the read path.
        let status = lifecycle_status(&HostLifecycleStamps {
            activated_at: None,
            finalized_at: None,
            revoked_at: None,
        });
        Self {
            id: host.id().to_owned(),
            name: host.name().to_owned(),
            kind: host_kind_wire(HostKind::Remote),
            created_at: host.created_at(),
            revoked_at: None,
            last_seen_at: None,
            activated_at: None,
            finalized_at: None,
            connected_host_name: None,
            provisioning_source: provisioning_source_wire(ProvisioningSource::UserInvite),
            status: status_wire(status),
            placeable: status.placeable(),
            creator_user_id: Some(host.user_id().as_str().to_owned()),
            display_label: None,
        }
    }
}

fn host_kind_wire(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Local => "local",
        HostKind::Remote => "remote",
    }
}

fn provisioning_source_wire(source: ProvisioningSource) -> &'static str {
    match source {
        ProvisioningSource::AppInstall => "app_install",
        ProvisioningSource::UserInvite => "user_invite",
        ProvisioningSource::System => "system",
    }
}

fn status_wire(status: HostLifecycleStatus) -> &'static str {
    match status {
        HostLifecycleStatus::Pending => "pending",
        HostLifecycleStatus::Attached => "attached",
        HostLifecycleStatus::Active => "active",
        HostLifecycleStatus::Revoked => "revoked",
    }
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::foundation::identity::{HostId, UserId};

    use super::*;

    fn record() -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse("h1").unwrap(),
            name: "shiki-box".to_owned(),
            kind: HostKind::Remote,
            created_at: 10,
            owner: Some(UserId::parse("u1").unwrap()),
            installation_id: None,
            last_seen_at: Some(20),
            activated_at: Some(11),
            finalized_at: Some(12),
            revoked_at: None,
            provisioning_source: ProvisioningSource::UserInvite,
            display_label: Some("Shiki's box".to_owned()),
        }
    }

    #[test]
    fn an_active_record_projects_the_derived_status_and_the_stored_fields() {
        let dto = host_wire(&record());
        assert_eq!(dto.id, "h1");
        assert_eq!(dto.name, "shiki-box");
        assert_eq!(dto.kind, "remote");
        assert_eq!(dto.created_at, 10);
        assert_eq!(dto.last_seen_at, Some(20));
        assert_eq!(dto.activated_at, Some(11));
        assert_eq!(dto.finalized_at, Some(12));
        assert_eq!(dto.provisioning_source, "user_invite");
        assert_eq!(dto.status, "active");
        assert!(dto.placeable);
        assert_eq!(dto.creator_user_id.as_deref(), Some("u1"));
        assert_eq!(dto.display_label.as_deref(), Some("Shiki's box"));
    }

    #[test]
    fn a_revoke_stamp_outranks_the_handshake_and_denies_placement() {
        let revoked = ComputerRecord {
            revoked_at: Some(30),
            ..record()
        };
        let dto = host_wire(&revoked);
        assert_eq!(dto.status, "revoked");
        assert!(!dto.placeable);
        assert_eq!(dto.revoked_at, Some(30));
    }

    #[test]
    fn an_unowned_record_names_no_creator() {
        let unowned = ComputerRecord {
            owner: None,
            ..record()
        };
        assert_eq!(host_wire(&unowned).creator_user_id, None);
    }

    #[test]
    fn the_wire_spells_each_kind_and_source_as_the_schema_does() {
        let local_install = ComputerRecord {
            kind: HostKind::Local,
            provisioning_source: ProvisioningSource::AppInstall,
            ..record()
        };
        let local = host_wire(&local_install);
        assert_eq!(local.kind, "local");
        assert_eq!(local.provisioning_source, "app_install");

        let system = ComputerRecord {
            provisioning_source: ProvisioningSource::System,
            ..record()
        };
        assert_eq!(host_wire(&system).provisioning_source, "system");
    }

    #[test]
    fn a_stampless_record_is_pending_and_unplaceable_like_a_fresh_invite() {
        let pending = ComputerRecord {
            activated_at: None,
            finalized_at: None,
            revoked_at: None,
            ..record()
        };
        let dto = host_wire(&pending);
        assert_eq!(dto.status, "pending");
        assert!(!dto.placeable);
    }
}
