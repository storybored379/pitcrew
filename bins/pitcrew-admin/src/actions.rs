//! Durable intent before HTTP. An ambiguous response is never retried by a page
//! refresh. Pending controller commands do not bypass engine authorization.
use crate::{
    Error, Result,
    auth::bounded,
    config::endpoint,
    journal::{Journal, integer},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{future::Future, pin::Pin, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Release {
        id: u64,
    },
    Reset {
        client: String,
    },
    Resend {
        job: u64,
        sequence: u64,
    },
    Approve {
        report: u64,
        decided_at: u64,
    },
    Deny {
        report: u64,
        decided_at: u64,
    },
    LowerQuality {
        job: u64,
        revision: u64,
    },
    Close {
        report: u64,
    },
    Message {
        report: u64,
        template: MessageTemplate,
    },
    Pin {
        id: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTemplate {
    Received,
    NeedDetails,
    UnderReview,
}
impl MessageTemplate {
    pub fn text(&self) -> &'static str {
        match self {
            Self::Received => "We received your report and will review it.",
            Self::NeedDetails => {
                "Please reply with the episode or movie, your device, and what happened when you tried to play it."
            }
            Self::UnderReview => {
                "Your report is under review. We will update you when we have a verified result."
            }
        }
    }
}
impl Command {
    pub fn api(&self, key: &str, note: &str) -> Option<(&'static str, Value)> {
        match self {
            Self::Release { id } => {
                Some(("/v1/admin/exclusions/release", json!({"id":id,"note":note})))
            }
            Self::Reset { client } => Some((
                "/v1/admin/capabilities/reset",
                json!({"client":client,"note":note}),
            )),
            Self::Resend { job, sequence } => Some((
                "/v1/admin/notifications/resend",
                json!({"job":job,"sequence":sequence,"request_key":key,"note":note}),
            )),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Rejected,
    Uncertain,
}
impl Outcome {
    pub fn state(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
        }
    }
}
pub type ApiFuture<'a> = Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>;
pub trait AdminApi: Send + Sync {
    fn submit<'a>(&'a self, command: &'a Command, key: &'a str, note: &'a str) -> ApiFuture<'a>;
}
pub struct HttpAdminApi {
    client: reqwest::Client,
    base: url::Url,
    secret: String,
}
impl HttpAdminApi {
    pub fn new(base: &str, secret: &str) -> Result<Self> {
        if !(32..=4096).contains(&secret.len()) || !secret.bytes().all(|b| (33..=126).contains(&b))
        {
            return Err(Error::Input);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Input)?;
        Ok(Self {
            client,
            base: endpoint(base)?,
            secret: secret.into(),
        })
    }
    async fn send(&self, command: &Command, key: &str, note: &str) -> Result<Outcome> {
        let (path, body) = command.api(key, note).ok_or(Error::Input)?;
        let response = self
            .client
            .post(self.base.join(path).map_err(|_| Error::Input)?)
            .bearer_auth(&self.secret)
            .json(&body)
            .send()
            .await
            .map_err(|_| Error::Upstream)?;
        let status = response.status();
        let bytes = bounded(response).await?;
        if status == reqwest::StatusCode::OK {
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Upstream)?;
            let field = match command {
                Command::Release { .. } => "released",
                Command::Reset { .. } => "reset",
                _ => "scheduled",
            };
            if value.get(field).and_then(Value::as_bool).is_none() {
                return Err(Error::Upstream);
            }
            Ok(Outcome::Accepted)
        } else if status.is_client_error() {
            Ok(Outcome::Rejected)
        } else {
            Ok(Outcome::Uncertain)
        }
    }
}
impl AdminApi for HttpAdminApi {
    fn submit<'a>(&'a self, command: &'a Command, key: &'a str, note: &'a str) -> ApiFuture<'a> {
        Box::pin(async move {
            self.send(command, key, note)
                .await
                .unwrap_or(Outcome::Uncertain)
        })
    }
}
impl Journal {
    pub async fn existing_request(
        &self,
        actor: &str,
        key: &str,
        command: &Command,
        note: &str,
    ) -> Result<bool> {
        if let Some(row) =
            sqlx::query("SELECT actor,command,note FROM admin_requests WHERE request_key=?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?
        {
            if row.get::<String, _>("actor") != actor
                || row.get::<String, _>("command") != serde_json::to_string(command)?
                || row.get::<String, _>("note") != note
            {
                return Err(Error::Conflict);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
    /// Returns true only to the first submitter. A repeated request never sends
    /// again, including after a crash between durable intent and HTTP response.
    pub async fn request(
        &self,
        actor: &str,
        key: &str,
        command: &Command,
        note: &str,
        now: u64,
    ) -> Result<bool> {
        if key.is_empty()
            || key.len() > 128
            || note.trim().is_empty()
            || note.len() > 1000
            || actor.is_empty()
        {
            return Err(Error::Input);
        }
        let command_json = serde_json::to_string(command)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) =
            sqlx::query("SELECT actor,command,note FROM admin_requests WHERE request_key=?")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?
        {
            if row.get::<String, _>("actor") != actor
                || row.get::<String, _>("command") != command_json
                || row.get::<String, _>("note") != note
            {
                return Err(Error::Conflict);
            }
            return Ok(false);
        }
        let state = if command.api(key, note).is_some() {
            "dispatching"
        } else {
            "pending_controller"
        };
        sqlx::query("INSERT INTO admin_requests VALUES(?,?,?,?,?,?,?)")
            .bind(key)
            .bind(actor)
            .bind(command_json)
            .bind(note)
            .bind(state)
            .bind(integer(now)?)
            .bind(integer(now)?)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO admin_audit(request_key,actor,event,at) VALUES(?,?,?,?)")
            .bind(key)
            .bind(actor)
            .bind(state)
            .bind(integer(now)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn finish(&self, key: &str, outcome: Outcome, now: u64) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor: String = sqlx::query_scalar("UPDATE admin_requests SET state=?,updated_at=? WHERE request_key=? AND state='dispatching' RETURNING actor")
            .bind(outcome.state()).bind(integer(now)?).bind(key).fetch_optional(&mut *tx).await?.ok_or(Error::Conflict)?;
        sqlx::query("INSERT INTO admin_audit(request_key,actor,event,at) VALUES(?,?,?,?)")
            .bind(key)
            .bind(actor)
            .bind(outcome.state())
            .bind(integer(now)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn recover(&self, now: u64) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO admin_audit(request_key,actor,event,at) SELECT request_key,actor,'uncertain',? FROM admin_requests WHERE state='dispatching'")
            .bind(integer(now)?).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE admin_requests SET state='uncertain',updated_at=? WHERE state='dispatching'",
        )
        .bind(integer(now)?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn requests(&self) -> Result<Vec<Value>> {
        crate::read::rows(&self.pool,"SELECT request_key,actor,command,note,state,created_at,updated_at FROM admin_requests ORDER BY created_at DESC,request_key LIMIT 100").await
    }
    pub async fn audit(&self) -> Result<Vec<Value>> {
        crate::read::rows(
            &self.pool,
            "SELECT id,request_key,actor,event AS audit_event,at FROM admin_audit ORDER BY id DESC LIMIT 100",
        )
        .await
    }
}
