//! SQLite custody of `avatar_designs`: the in-transaction insert the two avatar
//! writers share, and the per-principal reads behind `GET …/avatar/design`.
//!
//! Release of a replaced or cleared design is the schema's job (triggers on
//! `users`/`agents`), so writers only ever insert and repoint.

use std::{path::Path, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use silverhand_backend_core::{
    foundation::identity::{AgentId, UserId},
    foundation::persistence::PersistenceFailure,
    identity_access::avatar_design::{AgentAvatarDesignRead, AvatarDesign, UserAvatarDesignRead},
};
use sqlx::{Row, SqliteConnection};
use thiserror::Error;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const ID_BYTES: usize = 12;

const INSERT_DESIGN: &str =
    "INSERT INTO avatar_designs (id, parts_version, config, created_at) VALUES (?, ?, ?, ?)";
const SELECT_USER_DESIGN: &str = "SELECT d.parts_version, d.config FROM users u \
     JOIN avatar_designs d ON d.id = u.avatar_design_id WHERE u.id = ?";
const SELECT_AGENT_DESIGN: &str = "SELECT d.parts_version, d.config FROM agents a \
     JOIN avatar_designs d ON d.id = a.avatar_design_id WHERE a.id = ?";

#[derive(Debug, Error)]
pub(crate) enum DesignInsertFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error("avatar design id entropy unavailable")]
    Entropy(getrandom::Error),
}

#[derive(Debug, Error)]
#[error("avatar_designs holds a row the write path could not have produced")]
struct CorruptStoredDesign;

/// Insert `design` on the caller's open transaction and return its id, or `None`
/// for a manual upload (the caller then writes `avatar_design_id = NULL`).
pub(crate) async fn insert_design(
    tx: &mut SqliteConnection,
    design: Option<&AvatarDesign>,
    now_ms: i64,
) -> Result<Option<String>, DesignInsertFailure> {
    let Some(design) = design else {
        return Ok(None);
    };
    let mut bytes = [0_u8; ID_BYTES];
    getrandom::fill(&mut bytes).map_err(DesignInsertFailure::Entropy)?;
    let id = URL_SAFE_NO_PAD.encode(bytes);
    sqlx::query(INSERT_DESIGN)
        .bind(&id)
        .bind(i64::from(design.parts_version()))
        .bind(design.config_json())
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    Ok(Some(id))
}

/// Read-only adapter for both principals' current avatar design.
#[derive(Clone)]
pub struct SqliteAvatarDesignRead {
    pool: crate::foundation::database::SqliteDatabase,
}

impl SqliteAvatarDesignRead {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, sqlx::Error> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    async fn read(
        &self,
        sql: &'static str,
        id: &str,
    ) -> Result<Option<AvatarDesign>, PersistenceFailure> {
        let row = sqlx::query(sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(PersistenceFailure::from_source)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let parts_version: i64 = row
            .try_get("parts_version")
            .map_err(PersistenceFailure::from_source)?;
        let config: String = row
            .try_get("config")
            .map_err(PersistenceFailure::from_source)?;
        AvatarDesign::from_stored(parts_version, &config)
            .map(Some)
            .map_err(|_| PersistenceFailure::from_source(CorruptStoredDesign))
    }
}

impl UserAvatarDesignRead for SqliteAvatarDesignRead {
    async fn user_avatar_design(
        &self,
        user_id: &UserId,
    ) -> Result<Option<AvatarDesign>, PersistenceFailure> {
        self.read(SELECT_USER_DESIGN, user_id.as_str()).await
    }
}

impl AgentAvatarDesignRead for SqliteAvatarDesignRead {
    async fn agent_avatar_design(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<AvatarDesign>, PersistenceFailure> {
        self.read(SELECT_AGENT_DESIGN, agent_id.as_str()).await
    }
}
