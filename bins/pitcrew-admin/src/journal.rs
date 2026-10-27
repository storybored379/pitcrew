//! Independent admin sessions and durable audited requests. No media DB writes.
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{path::Path, time::Duration};

#[derive(Clone)]
pub struct Journal {
    pub(crate) pool: SqlitePool,
}
pub struct Session {
    pub account: String,
    pub csrf: String,
}
pub struct Credentials {
    pub token: String,
    pub csrf: String,
}
pub fn token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Error::Storage)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub(crate) fn integer(value: u64) -> Result<i64> {
    value.try_into().map_err(|_| Error::Input)
}
impl Journal {
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::raw_sql(include_str!("schema.sql"))
            .execute(&pool)
            .await?;
        Ok(Self { pool })
    }
    pub async fn reserve_login(&self, now: u64) -> Result<()> {
        let bucket = integer(now / 60)?;
        let count: i64 = sqlx::query_scalar("INSERT INTO admin_rates(bucket,count) VALUES(?,1) ON CONFLICT(bucket) DO UPDATE SET count=count+1 RETURNING count")
            .bind(bucket).fetch_one(&self.pool).await?;
        if count > 30 {
            return Err(Error::Rate);
        }
        Ok(())
    }
    pub async fn begin(&self, pin: u64, now: u64) -> Result<String> {
        let state = token()?;
        sqlx::query("INSERT INTO admin_logins VALUES(?,?,?)")
            .bind(digest(&state))
            .bind(integer(pin)?)
            .bind(integer(now.checked_add(600).ok_or(Error::Input)?)?)
            .execute(&self.pool)
            .await?;
        Ok(state)
    }
    pub async fn pin(&self, state: &str, now: u64) -> Result<u64> {
        let value: i64 =
            sqlx::query_scalar("SELECT pin FROM admin_logins WHERE digest=? AND expires>?")
                .bind(digest(state))
                .bind(integer(now)?)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(Error::Unauthorized)?;
        value.try_into().map_err(|_| Error::Storage)
    }
    pub async fn complete(&self, state: &str, account: &str, now: u64) -> Result<Credentials> {
        let result = Credentials {
            token: token()?,
            csrf: token()?,
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let deleted = sqlx::query("DELETE FROM admin_logins WHERE digest=? AND expires>?")
            .bind(digest(state))
            .bind(integer(now)?)
            .execute(&mut *tx)
            .await?;
        if deleted.rows_affected() != 1 {
            return Err(Error::Unauthorized);
        }
        sqlx::query("INSERT INTO admin_sessions VALUES(?,?,?,?)")
            .bind(digest(&result.token))
            .bind(account)
            .bind(&result.csrf)
            .bind(integer(now.checked_add(28_800).ok_or(Error::Input)?)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn session(&self, token: &str, now: u64) -> Result<Session> {
        let row =
            sqlx::query("SELECT account,csrf FROM admin_sessions WHERE digest=? AND expires>?")
                .bind(digest(token))
                .bind(integer(now)?)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(Error::Unauthorized)?;
        Ok(Session {
            account: row.get("account"),
            csrf: row.get("csrf"),
        })
    }
    pub async fn logout(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM admin_sessions WHERE digest=?")
            .bind(digest(token))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod digest_tests {
    // Stored digests must survive crypto crate upgrades unchanged.
    #[test]
    fn digest_matches_the_published_vector() {
        assert_eq!(
            super::digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
