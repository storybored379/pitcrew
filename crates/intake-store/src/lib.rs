//! Local intake journal and browser/channel identity proofs. A proof is never a
//! library grant: callers must obtain fresh access before accepting a report.
pub mod diagnoses;
pub mod model_attempts;
pub mod progress;
pub mod routes;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{path::Path, time::Duration};
use thiserror::Error;

pub const LOGIN_TTL: u64 = 600;
pub const SESSION_TTL: u64 = 7 * 86400;
pub const LINK_TTL: u64 = 600;

#[derive(Debug, Error)]
pub enum Error {
    #[error("intake storage unavailable")]
    Database(#[from] sqlx::Error),
    #[error("invalid intake data")]
    Invalid,
    #[error("proof missing, expired, or already used")]
    Proof,
    #[error("intake rate limit exceeded; try again later")]
    RateLimited,
    #[error("secure randomness unavailable")]
    Random,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    link_key: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub account_id: String,
    pub csrf: String,
}
pub struct NewSession {
    pub token: String,
    pub session: Session,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredIntake {
    pub id: i64,
    pub channel_id: String,
    pub message_id: String,
    pub conversation_id: String,
    pub response: serde_json::Value,
    pub created_at: u64,
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let link_key = load_key(path.as_ref())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path.as_ref())
                .map_err(|_| Error::Invalid)?;
            if file
                .metadata()
                .map_err(|_| Error::Invalid)?
                .permissions()
                .mode()
                & 0o077
                != 0
            {
                return Err(Error::Invalid);
            }
        }
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .busy_timeout(Duration::from_secs(5)),
            )
            .await?;
        sqlx::raw_sql(include_str!("schema.sql"))
            .execute(&pool)
            .await?;
        Ok(Self { pool, link_key })
    }
    /// Opaque browser state is hashed at rest and bound to a server-created PIN.
    pub async fn begin_login(&self, pin: u64, now: u64) -> Result<String> {
        let state = random(32)?;
        sqlx::query("INSERT INTO web_logins (digest,pin,expires) VALUES (?,?,?)")
            .bind(digest(&state))
            .bind(integer(pin)?)
            .bind(expiry(now, LOGIN_TTL)?)
            .execute(&self.pool)
            .await?;
        Ok(state)
    }
    pub async fn login_pin(&self, state: &str, now: u64) -> Result<Option<u64>> {
        let pin: Option<i64> =
            sqlx::query_scalar("SELECT pin FROM web_logins WHERE digest=? AND expires>?")
                .bind(digest(state))
                .bind(integer(now)?)
                .fetch_optional(&self.pool)
                .await?;
        Ok(pin.map(|p| p as u64))
    }
    /// Call after Plex has authenticated the account and fresh membership. The
    /// consumed browser challenge, account token, and new session commit together.
    pub async fn complete_login(
        &self,
        state: &str,
        account: &str,
        token: &str,
        now: u64,
    ) -> Result<NewSession> {
        let raw = random(32)?;
        let csrf = random(32)?;
        let expires = expiry(now, SESSION_TTL)?;
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("DELETE FROM web_logins WHERE digest=? AND expires>?")
            .bind(digest(state))
            .bind(integer(now)?)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if changed != 1 {
            return Err(Error::Proof);
        }
        sqlx::query("INSERT INTO web_accounts (account,token) VALUES (?,?) ON CONFLICT(account) DO UPDATE SET token=excluded.token")
            .bind(account).bind(token).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO web_sessions (digest,account,csrf,expires) VALUES (?,?,?,?)")
            .bind(digest(&raw))
            .bind(account)
            .bind(&csrf)
            .bind(expires)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(NewSession {
            token: raw,
            session: Session {
                account_id: account.into(),
                csrf,
            },
        })
    }
    pub async fn session(&self, token: &str, now: u64) -> Result<Option<Session>> {
        let row = sqlx::query("SELECT account,csrf FROM web_sessions WHERE digest=? AND expires>?")
            .bind(digest(token))
            .bind(integer(now)?)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| Session {
            account_id: r.get(0),
            csrf: r.get(1),
        }))
    }
    pub async fn logout(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM web_sessions WHERE digest=?")
            .bind(digest(token))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn account_token(&self, account: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT token FROM web_accounts WHERE account=?")
                .bind(account)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    /// Fresh membership must be checked by caller. New code invalidates all older
    /// codes for this account; no token/code plaintext is stored in this table.
    pub async fn issue_link(&self, account: &str, now: u64) -> Result<String> {
        let code = random(6)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM web_link_codes WHERE account=?")
            .bind(account)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO web_link_codes (digest,account,expires) VALUES (?,?,?)")
            .bind(self.link_digest(&code)?)
            .bind(account)
            .bind(expiry(now, LINK_TTL)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(code)
    }
    /// The transport must authenticate `channel` and `user`; neither comes from
    /// untrusted message text. Rate reservations, consumption and link are atomic.
    pub async fn redeem_link(
        &self,
        code: &str,
        channel: &str,
        user: &str,
        now: u64,
    ) -> Result<String> {
        let bucket = integer(now / LINK_TTL)?;
        let actor = serde_json::to_string(&(channel, user)).map_err(|_| Error::Invalid)?;
        let mut tx = self.pool.begin().await?;
        for (key, limit) in [(actor, 5_i64), ("global".into(), 100_i64)] {
            let count: i64 = sqlx::query_scalar("INSERT INTO web_link_attempts (actor,bucket,count) VALUES (?,?,1) ON CONFLICT(actor) DO UPDATE SET bucket=excluded.bucket,count=CASE WHEN bucket=excluded.bucket THEN count+1 ELSE 1 END RETURNING count")
                .bind(key).bind(bucket).fetch_one(&mut *tx).await?;
            if count > limit {
                tx.commit().await?;
                return Err(Error::RateLimited);
            }
        }
        let account: Option<String> =
            sqlx::query_scalar("SELECT account FROM web_link_codes WHERE digest=? AND expires>?")
                .bind(self.link_digest(code)?)
                .bind(integer(now)?)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(account) = account else {
            tx.commit().await?;
            return Err(Error::Proof);
        };
        let existing: Option<String> =
            sqlx::query_scalar("SELECT account FROM web_links WHERE channel=? AND user=?")
                .bind(channel)
                .bind(user)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_some_and(|linked| linked != account) {
            tx.commit().await?;
            return Err(Error::Proof);
        }
        sqlx::query("DELETE FROM web_link_codes WHERE digest=?")
            .bind(self.link_digest(code)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO web_links (channel,user,account) VALUES (?,?,?) ON CONFLICT(channel,user) DO NOTHING")
            .bind(channel).bind(user).bind(&account).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(account)
    }
    /// Read-only proof lookup with a durable global attempt limit. Callers then
    /// obtain fresh membership before the atomic redemption; lookup grants none.
    pub async fn link_account(&self, code: &str, now: u64) -> Result<Option<String>> {
        if !self
            .reserve_rate("link_lookup", "global", 100, LINK_TTL, now)
            .await?
        {
            return Err(Error::RateLimited);
        }
        Ok(
            sqlx::query_scalar("SELECT account FROM web_link_codes WHERE digest=? AND expires>?")
                .bind(self.link_digest(code)?)
                .bind(integer(now)?)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    fn link_digest(&self, code: &str) -> Result<String> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.link_key).map_err(|_| Error::Invalid)?;
        mac.update(code.trim().to_ascii_lowercase().as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }
    pub async fn linked_account(&self, channel: &str, user: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT account FROM web_links WHERE channel=? AND user=?")
                .bind(channel)
                .bind(user)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    /// This journal records intake outcomes, not a repair or a claim of readiness.
    /// Adapter delivery identity makes replay idempotent even after restart.
    pub async fn persist_intake(
        &self,
        channel: &str,
        message: &str,
        account: &str,
        conversation: &str,
        response: &serde_json::Value,
        now: u64,
    ) -> Result<bool> {
        let changed = sqlx::query("INSERT INTO web_intakes (channel,message,account,conversation,response,created) VALUES (?,?,?,?,?,?) ON CONFLICT(channel,message) DO NOTHING")
            .bind(channel).bind(message).bind(account).bind(conversation).bind(response.to_string()).bind(integer(now)?)
            .execute(&self.pool).await?.rows_affected();
        Ok(changed == 1)
    }
    pub async fn reports(&self, account: &str) -> Result<Vec<StoredIntake>> {
        sqlx::query("SELECT id,channel,message,conversation,response,created FROM web_intakes WHERE account=? ORDER BY id DESC")
            .bind(account).fetch_all(&self.pool).await?.into_iter().map(intake_row).collect()
    }
    /// Browser retries are deduplicated before reserving the five-report daily
    /// quota. Reservation and report commit together, including concurrent retries.
    pub async fn persist_web_report(
        &self,
        account: &str,
        nonce: &str,
        response: &serde_json::Value,
        now: u64,
    ) -> Result<bool> {
        let message = serde_json::to_string(&(account, nonce)).map_err(|_| Error::Invalid)?;
        let mut tx = self.pool.begin().await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM web_intakes WHERE channel='web' AND message=?)",
        )
        .bind(&message)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            return Ok(false);
        }
        let count: i64 = sqlx::query_scalar("INSERT INTO web_rates (action,actor,bucket,count) VALUES ('reports',?,?,1) ON CONFLICT(action,actor) DO UPDATE SET bucket=excluded.bucket,count=CASE WHEN bucket=excluded.bucket THEN count+1 ELSE 1 END RETURNING count")
            .bind(account).bind(integer(now / 86400)?).fetch_one(&mut *tx).await?;
        if count > 5 {
            return Err(Error::RateLimited);
        }
        sqlx::query("INSERT INTO web_intakes (channel,message,account,conversation,response,created) VALUES ('web',?,?,?,?,?)")
            .bind(message).bind(account).bind(format!("web:{account}"))
            .bind(response.to_string()).bind(integer(now)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }
    /// A durable send reservation is deliberately never retried automatically.
    /// Crash or lost response leaves `uncertain` for later admin reconciliation.
    pub async fn claim_delivery(
        &self,
        channel: &str,
        message: &str,
        account: &str,
    ) -> Result<bool> {
        let changed = sqlx::query("INSERT INTO web_intake_deliveries (channel,message,account,state) VALUES (?,?,?,'uncertain') ON CONFLICT(channel,message,account) DO NOTHING")
            .bind(channel).bind(message).bind(account).execute(&self.pool).await?.rows_affected();
        Ok(changed == 1)
    }
    pub async fn observe_delivery(
        &self,
        channel: &str,
        message: &str,
        account: &str,
        outcome: &str,
    ) -> Result<()> {
        if !matches!(
            outcome,
            "confirmed" | "not_accepted" | "uncertain" | "permanent_failure"
        ) {
            return Err(Error::Invalid);
        }
        sqlx::query(
            "UPDATE web_intake_deliveries SET state=? WHERE channel=? AND message=? AND account=?",
        )
        .bind(outcome)
        .bind(channel)
        .bind(message)
        .bind(account)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn delivery_state(
        &self,
        channel: &str,
        message: &str,
        account: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT state FROM web_intake_deliveries WHERE channel=? AND message=? AND account=?",
        )
        .bind(channel)
        .bind(message)
        .bind(account)
        .fetch_optional(&self.pool)
        .await?)
    }
    /// Fixed-window, atomic reservation used before expensive public operations.
    pub async fn reserve_rate(
        &self,
        action: &str,
        actor: &str,
        limit: u64,
        seconds: u64,
        now: u64,
    ) -> Result<bool> {
        if seconds == 0 {
            return Err(Error::Invalid);
        }
        let bucket = integer(now / seconds)?;
        let limit = integer(limit)?;
        let count:i64=sqlx::query_scalar("INSERT INTO web_rates (action,actor,bucket,count) VALUES (?,?,?,1) ON CONFLICT(action,actor) DO UPDATE SET bucket=excluded.bucket,count=CASE WHEN bucket=excluded.bucket THEN count+1 ELSE 1 END RETURNING count")
            .bind(action).bind(actor).bind(bucket).fetch_one(&self.pool).await?;
        Ok(count <= limit)
    }
    pub async fn stored_intake(
        &self,
        channel: &str,
        message: &str,
        account: &str,
    ) -> Result<Option<StoredIntake>> {
        sqlx::query("SELECT id,channel,message,conversation,response,created FROM web_intakes WHERE channel=? AND message=? AND account=?")
            .bind(channel).bind(message).bind(account).fetch_optional(&self.pool).await?.map(intake_row).transpose()
    }
    pub async fn latest_intake(
        &self,
        channel: &str,
        conversation: &str,
        account: &str,
    ) -> Result<Option<StoredIntake>> {
        sqlx::query("SELECT id,channel,message,conversation,response,created FROM web_intakes WHERE channel=? AND conversation=? AND account=? ORDER BY id DESC LIMIT 1")
            .bind(channel).bind(conversation).bind(account).fetch_optional(&self.pool).await?.map(intake_row).transpose()
    }
}
fn intake_row(r: sqlx::sqlite::SqliteRow) -> Result<StoredIntake> {
    Ok(StoredIntake {
        id: r.get(0),
        channel_id: r.get(1),
        message_id: r.get(2),
        conversation_id: r.get(3),
        response: serde_json::from_str(r.get(4)).map_err(|_| Error::Invalid)?,
        created_at: r.get::<i64, _>(5) as u64,
    })
}
fn integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invalid)
}
fn expiry(now: u64, ttl: u64) -> Result<i64> {
    integer(now.checked_add(ttl).ok_or(Error::Invalid)?)
}
fn digest(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}
fn random(bytes: usize) -> Result<String> {
    let mut value = vec![0; bytes];
    getrandom::fill(&mut value).map_err(|_| Error::Random)?;
    Ok(value.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn load_key(database: &Path) -> Result<[u8; 32]> {
    use std::io::{Read, Write};
    let mut path = database.as_os_str().to_owned();
    path.push(".link-key");
    let path = std::path::PathBuf::from(path);
    if !path.exists() {
        // Do not silently replace the key for existing journaled challenges.
        if database.metadata().is_ok_and(|m| m.len() > 0) {
            return Err(Error::Invalid);
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // A concurrent first initializer may need to retry after the winner has
        // synced its key. Never read a possibly incomplete key after a create race.
        let mut file = options.open(&path).map_err(|_| Error::Invalid)?;
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|_| Error::Random)?;
        file.write_all(&key).map_err(|_| Error::Invalid)?;
        file.sync_all().map_err(|_| Error::Invalid)?;
    }
    let mut file = std::fs::File::open(path).map_err(|_| Error::Invalid)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file
            .metadata()
            .map_err(|_| Error::Invalid)?
            .permissions()
            .mode()
            & 0o077
            != 0
        {
            return Err(Error::Invalid);
        }
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(33)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Invalid)?;
    bytes.try_into().map_err(|_| Error::Invalid)
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    // Stored keys and link digests must survive crypto crate upgrades unchanged.
    #[test]
    fn digests_match_published_vectors() {
        assert_eq!(
            digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // RFC 4231 test case 2.
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac.finalize().into_bytes()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
