//! Durable safety policy. Callers must derive `PolicyRole` from authenticated
//! authority, never from viewer-controlled request fields. All clocks are Unix
//! seconds; evidence and opaque identifiers are local data, not cloud prompts.

use crate::{FileIdentity, Result, Store, StoreError, integer, unsigned};
use pitcrew_domain::media::ReleaseIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqliteConnection};

const EXCLUSION_SECONDS: u64 = 30 * 86400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyRole {
    Admin,
    Viewer,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminAction {
    pub actor: String,
    pub role: PolicyRole,
    pub note: String,
}

impl AdminAction {
    pub fn validate(&self) -> Result<()> {
        if self.role != PolicyRole::Admin {
            return Err(StoreError::AdminRequired);
        }
        text(&self.actor)?;
        text(&self.note)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyAudit {
    pub id: u64,
    pub actor: String,
    pub action: String,
    pub subject: String,
    pub note: String,
    pub at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExclusionKind {
    Quarantine,
    DeadRelease,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewExclusion {
    /// Configured opaque arr instance identity, not its hostname.
    pub client: String,
    pub release: u64,
    pub identity: ReleaseIdentity,
    pub kind: ExclusionKind,
    pub reason: String,
    pub evidence: Value,
    pub pinned: bool,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseExclusion {
    pub id: u64,
    pub spec: NewExclusion,
    pub expires_at: u64,
    pub released_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// A recorded outside queue actor can remove completed downloads quickly.
    pub external_queue_actor: bool,
    /// Only a certified held category permits verification before import.
    /// None requires verification after import; it never permits skipping it.
    pub held_category: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilityLedger {
    pub client: String,
    pub version: String,
    /// None means unknown/reset; the router must not infer certification.
    pub capabilities: Option<ClientCapabilities>,
    pub evidence: Value,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityFingerprint {
    pub arr_file_id: u64,
    pub size: u64,
    pub date_added: String,
    /// Canonical serialized arr quality, including revision metadata.
    pub quality: String,
    /// Agent identity includes the filesystem's device/inode/metadata revision.
    pub agent: FileIdentity,
}

fn text(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StoreError::InvalidInput);
    }
    Ok(())
}

impl Store {
    /// Preserve the original exclusion specification and audit the effective
    /// policy change in the same transaction. Expired/released rows cannot be
    /// revived by pinning an old owner-page snapshot.
    pub async fn pin_exclusion(&self, id: u64, admin: &AdminAction, now: u64) -> Result<bool> {
        admin.validate()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT * FROM release_exclusions WHERE id=?")
            .bind(integer(id)?)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::Missing)?;
        let pinned: bool = row.get("pinned");
        let existing = exclusion_row(row)?;
        if now < existing.spec.created_at
            || existing.released_at.is_some()
            || (!pinned && now >= existing.expires_at)
        {
            return Err(StoreError::InvalidInput);
        }
        if pinned {
            return Ok(false);
        }
        sqlx::query("UPDATE release_exclusions SET pinned=1 WHERE id=?")
            .bind(integer(id)?)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, admin, "pin_exclusion", &id.to_string(), now).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn exclude_release(
        &self,
        spec: &NewExclusion,
        key: &str,
    ) -> Result<ReleaseExclusion> {
        text(key)?;
        text(&spec.client)?;
        text(&spec.reason)?;
        if spec.release == 0 || spec.evidence.is_null() {
            return Err(StoreError::InvalidInput);
        }
        let expires_at = spec
            .created_at
            .checked_add(EXCLUSION_SECONDS)
            .ok_or(StoreError::InvalidInput)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) = sqlx::query("SELECT * FROM release_exclusions WHERE exclusion_key=?")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?
        {
            let existing = exclusion_row(row)?;
            if existing.spec != *spec {
                return Err(StoreError::IdempotencyConflict);
            }
            return Ok(existing);
        }
        let id = sqlx::query("INSERT INTO release_exclusions(exclusion_key,client,spec,expires_at,pinned) VALUES(?,?,?,?,?)")
            .bind(key).bind(&spec.client).bind(serde_json::to_string(spec)?).bind(integer(expires_at)?)
            .bind(spec.pinned).execute(&mut *tx).await?.last_insert_rowid();
        tx.commit().await?;
        Ok(ReleaseExclusion {
            id: unsigned(id)?,
            spec: spec.clone(),
            expires_at,
            released_at: None,
        })
    }

    /// Active exclusions include pinned rows indefinitely. The deadline itself
    /// is expired. Released rows remain available through the audit journal.
    pub async fn exclusions(&self, client: &str, now: u64) -> Result<Vec<ReleaseExclusion>> {
        text(client)?;
        sqlx::query("SELECT * FROM release_exclusions WHERE client=? AND released_at IS NULL AND (pinned=1 OR expires_at>?) ORDER BY id")
            .bind(client).bind(integer(now)?).fetch_all(&self.pool).await?
            .into_iter().map(exclusion_row).collect()
    }

    pub async fn release_is_excluded(
        &self,
        client: &str,
        release: u64,
        identity: &ReleaseIdentity,
        now: u64,
    ) -> Result<bool> {
        Ok(self
            .exclusions(client, now)
            .await?
            .iter()
            .any(|row| row.spec.release == release || row.spec.identity.overlaps(identity)))
    }

    pub async fn release_exclusion(&self, id: u64, admin: &AdminAction, now: u64) -> Result<bool> {
        admin.validate()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT * FROM release_exclusions WHERE id=?")
            .bind(integer(id)?)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::Missing)?;
        let existing = exclusion_row(row)?;
        if existing.released_at.is_some() {
            return Ok(false);
        }
        if now < existing.spec.created_at {
            return Err(StoreError::InvalidInput);
        }
        sqlx::query("UPDATE release_exclusions SET released_at=? WHERE id=?")
            .bind(integer(now)?)
            .bind(integer(id)?)
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, admin, "release_exclusion", &id.to_string(), now).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn policy_audit(&self) -> Result<Vec<PolicyAudit>> {
        sqlx::query("SELECT * FROM policy_audit ORDER BY id")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok(PolicyAudit {
                    id: unsigned(row.get("id"))?,
                    actor: row.get("actor"),
                    action: row.get("action"),
                    subject: row.get("subject"),
                    note: row.get("note"),
                    at: unsigned(row.get("at"))?,
                })
            })
            .collect()
    }

    /// A version change discards every learned capability; an unchanged version
    /// preserves its original observation clock and evidence.
    pub async fn observe_client_version(
        &self,
        client: &str,
        version: &str,
        now: u64,
    ) -> Result<ClientCapabilityLedger> {
        text(client)?;
        text(version)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) = ledger(&mut tx, client).await? {
            if now < existing.updated_at {
                return Err(StoreError::InvalidInput);
            }
            if existing.version == version {
                return Ok(existing);
            }
        }
        let next = ClientCapabilityLedger {
            client: client.into(),
            version: version.into(),
            capabilities: None,
            evidence: Value::Null,
            updated_at: now,
        };
        save_ledger(&mut tx, &next).await?;
        tx.commit().await?;
        Ok(next)
    }

    pub async fn capabilities(
        &self,
        client: &str,
        version: &str,
    ) -> Result<Option<ClientCapabilityLedger>> {
        text(version)?;
        Ok(self
            .client_ledger(client)
            .await?
            .filter(|row| row.version == version))
    }

    /// Admin inspection does not require the caller to know the client version.
    /// Effect authorization must use the exact-version `capabilities` query.
    pub async fn client_ledger(&self, client: &str) -> Result<Option<ClientCapabilityLedger>> {
        text(client)?;
        let mut connection = self.pool.acquire().await?;
        ledger(&mut connection, client).await
    }

    pub async fn record_capabilities(
        &self,
        client: &str,
        version: &str,
        capabilities: &ClientCapabilities,
        evidence: &Value,
        now: u64,
    ) -> Result<ClientCapabilityLedger> {
        text(client)?;
        text(version)?;
        if let Some(category) = &capabilities.held_category {
            text(category)?;
        }
        if evidence.is_null() {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut current = ledger(&mut tx, client).await?.ok_or(StoreError::Missing)?;
        if current.version != version {
            return Err(StoreError::RevisionConflict);
        }
        if now < current.updated_at {
            return Err(StoreError::InvalidInput);
        }
        // Observations may reduce trust, but only an audited reset or a client
        // version change can restore it. A later quiet queue is not evidence
        // that an outside actor disappeared or that a lost hold works again.
        let mut next = capabilities.clone();
        if let Some(previous) = &current.capabilities {
            next.external_queue_actor |= previous.external_queue_actor;
            if next.held_category != previous.held_category {
                next.held_category = None;
            }
        }
        current.capabilities = Some(next);
        current.evidence = evidence.clone();
        current.updated_at = now;
        save_ledger(&mut tx, &current).await?;
        tx.commit().await?;
        Ok(current)
    }

    pub async fn reset_capabilities(
        &self,
        client: &str,
        admin: &AdminAction,
        now: u64,
    ) -> Result<()> {
        admin.validate()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut current = ledger(&mut tx, client).await?.ok_or(StoreError::Missing)?;
        if now < current.updated_at {
            return Err(StoreError::InvalidInput);
        }
        current.capabilities = None;
        current.evidence = Value::Null;
        current.updated_at = now;
        save_ledger(&mut tx, &current).await?;
        audit(&mut tx, admin, "reset_capabilities", client, now).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns a store-minted revision, never a lossy metadata hash. Every
    /// changed tuple mints a new revision, including a return to an old tuple.
    pub async fn register_file(
        &self,
        client: &str,
        resource: &str,
        fingerprint: &IdentityFingerprint,
        now: u64,
    ) -> Result<FileIdentity> {
        text(client)?;
        text(resource)?;
        text(&fingerprint.date_added)?;
        text(&fingerprint.quality)?;
        if fingerprint.arr_file_id == 0
            || fingerprint.size == 0
            || fingerprint.agent.id == 0
            || fingerprint.agent.revision == 0
        {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let json = serde_json::to_string(fingerprint)?;
        if let Some(row) = current_file(&mut tx, client, resource).await? {
            if now < unsigned(row.get("observed_at"))? {
                return Err(StoreError::InvalidInput);
            }
            if row.get::<&str, _>("fingerprint") == json {
                let revision = unsigned(row.get("revision"))?;
                sqlx::query("UPDATE file_revisions SET observed_at=? WHERE revision=?")
                    .bind(integer(now)?)
                    .bind(integer(revision)?)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(FileIdentity {
                    id: fingerprint.arr_file_id,
                    revision,
                });
            }
        }
        let revision = sqlx::query(
            "INSERT INTO file_revisions(client,resource,fingerprint,observed_at) VALUES(?,?,?,?)",
        )
        .bind(client)
        .bind(resource)
        .bind(json)
        .bind(integer(now)?)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        sqlx::query("INSERT INTO current_file_revisions(client,resource,revision) VALUES(?,?,?) ON CONFLICT(client,resource) DO UPDATE SET revision=excluded.revision")
            .bind(client).bind(resource).bind(revision).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(FileIdentity {
            id: fingerprint.arr_file_id,
            revision: unsigned(revision)?,
        })
    }

    /// A current read from both arr and the agent must exactly equal the stored
    /// tuple, and the caller's opaque revision must still be current.
    pub async fn validate_file(
        &self,
        client: &str,
        resource: &str,
        expected: FileIdentity,
        current: &IdentityFingerprint,
        now: u64,
        max_age: u64,
    ) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let row = current_file(&mut connection, client, resource)
            .await?
            .ok_or(StoreError::IdentityMismatch)?;
        let observed_at = unsigned(row.get("observed_at"))?;
        let fingerprint: IdentityFingerprint = serde_json::from_str(row.get("fingerprint"))?;
        if expected.id != fingerprint.arr_file_id
            || expected.revision != unsigned(row.get("revision"))?
            || fingerprint != *current
            || now < observed_at
            || now - observed_at > max_age
        {
            return Err(StoreError::IdentityMismatch);
        }
        Ok(())
    }
}

pub(crate) async fn audit(
    connection: &mut SqliteConnection,
    admin: &AdminAction,
    action: &str,
    subject: &str,
    now: u64,
) -> Result<()> {
    sqlx::query("INSERT INTO policy_audit(actor,action,subject,note,at) VALUES(?,?,?,?,?)")
        .bind(&admin.actor)
        .bind(action)
        .bind(subject)
        .bind(&admin.note)
        .bind(integer(now)?)
        .execute(connection)
        .await?;
    Ok(())
}

fn exclusion_row(row: sqlx::sqlite::SqliteRow) -> Result<ReleaseExclusion> {
    Ok(ReleaseExclusion {
        id: unsigned(row.get("id"))?,
        spec: serde_json::from_str(row.get("spec"))?,
        expires_at: unsigned(row.get("expires_at"))?,
        released_at: row
            .get::<Option<i64>, _>("released_at")
            .map(unsigned)
            .transpose()?,
    })
}

async fn ledger(
    connection: &mut SqliteConnection,
    client: &str,
) -> Result<Option<ClientCapabilityLedger>> {
    sqlx::query_scalar::<_, String>("SELECT ledger FROM client_capabilities WHERE client=?")
        .bind(client)
        .fetch_optional(connection)
        .await?
        .map(|json| serde_json::from_str(&json).map_err(StoreError::from))
        .transpose()
}

async fn save_ledger(
    connection: &mut SqliteConnection,
    ledger: &ClientCapabilityLedger,
) -> Result<()> {
    integer(ledger.updated_at)?;
    sqlx::query("INSERT INTO client_capabilities(client,ledger) VALUES(?,?) ON CONFLICT(client) DO UPDATE SET ledger=excluded.ledger")
        .bind(&ledger.client).bind(serde_json::to_string(ledger)?).execute(connection).await?;
    Ok(())
}

async fn current_file(
    connection: &mut SqliteConnection,
    client: &str,
    resource: &str,
) -> Result<Option<sqlx::sqlite::SqliteRow>> {
    Ok(sqlx::query("SELECT f.* FROM file_revisions f JOIN current_file_revisions c ON c.revision=f.revision WHERE c.client=? AND c.resource=?")
        .bind(client).bind(resource).fetch_optional(connection).await?)
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
