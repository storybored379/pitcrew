//! Authenticated local policy administration. Webhook and replay secrets are not
//! administrator credentials; callers cannot choose their own role or actor.
use pitcrew_domain::IntentKey;
use pitcrew_engine::webhook::{MAX_BODY, Secret};
use pitcrew_store::{
    Store, StoreError,
    policy::{AdminAction, PolicyRole},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub struct AdminApi {
    pub secret: Secret,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    id: u64,
    note: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reset {
    client: String,
    note: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resend {
    job: u64,
    sequence: u64,
    request_key: String,
    note: String,
}
fn authority(note: String) -> AdminAction {
    AdminAction {
        actor: "configured-admin".into(),
        role: PolicyRole::Admin,
        note,
    }
}
impl AdminApi {
    pub async fn handle(
        &self,
        store: &Store,
        method: &str,
        target: &str,
        credential: &str,
        body: &[u8],
        now: u64,
    ) -> (u16, Value) {
        if !self.secret.accepts(credential) {
            return (401, json!({"error":"unauthorized"}));
        }
        if body.len() > MAX_BODY {
            return (413, json!({"error":"payload_too_large"}));
        }
        match self.dispatch(store, method, target, body, now).await {
            Ok(Some(value)) => (200, value),
            Ok(None) => (404, json!({"error":"not_found"})),
            Err(StoreError::Missing) => (404, json!({"error":"not_found"})),
            Err(StoreError::Json(_) | StoreError::InvalidInput | StoreError::AdminRequired) => {
                (400, json!({"error":"invalid_admin_request"}))
            }
            Err(StoreError::IdempotencyConflict | StoreError::RevisionConflict) => {
                (409, json!({"error":"request_conflict"}))
            }
            Err(_) => (503, json!({"error":"store_unavailable"})),
        }
    }
    async fn dispatch(
        &self,
        store: &Store,
        method: &str,
        target: &str,
        body: &[u8],
        now: u64,
    ) -> Result<Option<Value>, StoreError> {
        if method == "GET" {
            if let Some(client) = target.strip_prefix("/v1/admin/exclusions/") {
                return Ok(Some(
                    json!({"exclusions":store.exclusions(client,now).await?}),
                ));
            }
            if let Some(client) = target.strip_prefix("/v1/admin/capabilities/") {
                return Ok(Some(json!({"ledger":store.client_ledger(client).await?})));
            }
            if let Some(job) = target.strip_prefix("/v1/admin/deliveries/") {
                let job = job.parse::<u64>().map_err(|_| StoreError::InvalidInput)?;
                store.load_job(job).await?;
                let rows = store
                    .outbox(job)
                    .await?
                    .into_iter()
                    .filter(|row| pitcrew_engine::notification(&row.intent.effect))
                    .map(|row| json!({"intent":row.intent,"status":row.status}))
                    .collect::<Vec<_>>();
                return Ok(Some(json!({"deliveries":rows})));
            }
        }
        if method == "POST" {
            match target {
                "/v1/admin/exclusions/release" => {
                    let input: Release = serde_json::from_slice(body)?;
                    return Ok(Some(
                        json!({"released":store.release_exclusion(input.id,&authority(input.note),now).await?}),
                    ));
                }
                "/v1/admin/capabilities/reset" => {
                    let input: Reset = serde_json::from_slice(body)?;
                    store
                        .reset_capabilities(&input.client, &authority(input.note), now)
                        .await?;
                    return Ok(Some(json!({"reset":true})));
                }
                "/v1/admin/notifications/resend" => {
                    let input: Resend = serde_json::from_slice(body)?;
                    return Ok(Some(
                        json!({"scheduled":store.resend_notification(&authority(input.note),IntentKey{job:input.job,sequence:input.sequence},&input.request_key,now).await?}),
                    ));
                }
                _ => {}
            }
        }
        Ok(None)
    }
}
