//! Shadow service boundary: bounded local intake, journal replay and report export.
pub mod admin;
pub mod doctor;
pub mod python;
pub mod runtime;
/// Read-only external snapshot importer.
pub use python as legacy;
pub mod stack;
use base64::Engine as _;
use pitcrew_domain::{Event, Intent};
use pitcrew_engine::{
    CallError, CallFuture, Options, Upstream, Worker,
    observer::{self, Action, Activity, Actor, Diff, Safety},
    webhook::{ArrObservation, Intake, IntakeError, Kind, MAX_BODY, Secret},
};
use pitcrew_store::{BudgetLimits, NewJob, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Structural defense: this executable intentionally has no effect executor.
/// A live deployment must supply a certified adapter in a later phase.
pub struct DisabledEffects;
impl Upstream for DisabledEffects {
    fn read<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        Box::pin(async { Err(CallError::Unavailable) })
    }
    fn write<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        Box::pin(async { Err(CallError::Unavailable) })
    }
}

pub struct App {
    pub store: Store,
    intake: Intake,
    control: Option<Secret>,
    admin: Option<admin::AdminApi>,
    limits: BudgetLimits,
    per_job: u32,
    profiles: BTreeMap<String, pitcrew_domain::StageTimeouts>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replay {
    pub key: String,
    pub job: NewJob,
    pub client: Option<String>,
    pub events: Vec<ReplayEvent>,
    #[serde(default)]
    pub observed: BTreeMap<u64, Activity>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayEvent {
    pub at: u64,
    pub key: String,
    pub event: Event,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportReplay {
    key: String,
    observation: pitcrew_arbiter::shadow::ImportObservation,
}

impl App {
    pub fn new(
        store: Store,
        instances: Vec<(String, Secret)>,
        control: Option<Secret>,
        limits: BudgetLimits,
        per_job: u32,
        profiles: BTreeMap<String, pitcrew_domain::StageTimeouts>,
    ) -> Result<Self, IntakeError> {
        Ok(Self {
            intake: Intake::new(store.clone(), instances)?,
            store,
            control,
            admin: None,
            limits,
            per_job,
            profiles,
        })
    }
    pub fn with_admin(mut self, secret: Secret) -> Self {
        self.admin = Some(admin::AdminApi { secret });
        self
    }

    fn control_authorized(&self, credential: &str) -> bool {
        self.control
            .as_ref()
            .is_some_and(|secret| secret.accepts(credential))
    }
    pub async fn handle(
        &self,
        method: &str,
        target: &str,
        credential: &str,
        body: &[u8],
        now: u64,
    ) -> (u16, Value) {
        if method == "GET" && target == "/healthz" {
            return match self.store.health().await {
                Ok(()) => (200, json!({"status":"ok","mode":"shadow"})),
                Err(_) => (503, json!({"error":"store_unavailable"})),
            };
        }
        if let Some(instance) = target.strip_prefix("/v1/webhooks/") {
            if method != "POST" {
                return (405, json!({"error":"post_required"}));
            }
            return match self.intake.ingest(instance, credential, body, now).await {
                Ok(inserted) => (202, json!({"accepted":true,"duplicate":!inserted})),
                Err(IntakeError::Unauthorized) => (401, json!({"error":"unauthorized"})),
                Err(IntakeError::TooLarge) => (413, json!({"error":"payload_too_large"})),
                Err(IntakeError::Payload) => (400, json!({"error":"invalid_payload"})),
                Err(_) => (503, json!({"error":"store_unavailable"})),
            };
        }
        if target.starts_with("/v1/admin/") {
            return match &self.admin {
                Some(admin) => {
                    admin
                        .handle(&self.store, method, target, credential, body, now)
                        .await
                }
                None => (401, json!({"error":"unauthorized"})),
            };
        }
        if !self.control_authorized(credential) {
            return (401, json!({"error":"unauthorized"}));
        }
        if method == "POST" && target == "/v1/shadow/imports" {
            if body.len() > MAX_BODY {
                return (413, json!({"error":"payload_too_large"}));
            }
            let input: ImportReplay = match serde_json::from_slice(body) {
                Ok(value) => value,
                Err(_) => return (400, json!({"error":"invalid_import_observation"})),
            };
            if input.key.is_empty() || input.key.len() > 256 {
                return (400, json!({"error":"invalid_observation_key"}));
            }
            let report = pitcrew_arbiter::shadow::compare(&input.observation);
            let payload = match serde_json::to_value(report) {
                Ok(value) => value,
                Err(_) => return (500, json!({"error":"report_encoding"})),
            };
            return match self
                .store
                .ingest_observation("arbiter:recorded", &input.key, &payload, now)
                .await
            {
                Ok(inserted) => (200, json!({"duplicate":!inserted,"report":payload})),
                Err(_) => (409, json!({"error":"observation_conflict"})),
            };
        }
        if method == "POST" && target == "/v1/shadow/replay" {
            if body.len() > MAX_BODY {
                return (413, json!({"error":"payload_too_large"}));
            }
            let replay = match serde_json::from_slice(body) {
                Ok(value) => value,
                Err(_) => return (400, json!({"error":"invalid_replay"})),
            };
            return match self.replay(replay).await {
                Ok(reports) => (
                    200,
                    json!({"reports":reports,"summary":observer::summary(&reports)}),
                ),
                Err(error) => (409, json!({"error":error})),
            };
        }
        (404, json!({"error":"not_found"}))
    }
    pub async fn replay(&self, mut replay: Replay) -> Result<Vec<Diff>, &'static str> {
        if replay.key.is_empty()
            || replay.events.is_empty()
            || replay.events.len() > 512
            || replay.job.policy.max_grabs > self.per_job
        {
            return Err("invalid_replay_limits");
        }
        if let Some(client) = &replay.client {
            replay.job.timeouts = *self.profiles.get(client).ok_or("unknown_client_profile")?;
        }
        // Validate the whole transcript before reserving budgets or publishing
        // any input. A malformed later step must not leave a ghost active job.
        let spec = &replay.job;
        let mut check = pitcrew_domain::FixJob::new_with_timeouts(
            spec.id,
            spec.library,
            spec.original,
            spec.constraints.clone(),
            spec.policy,
            spec.timeouts,
            spec.created_at,
        )
        .map_err(|_| "invalid_job")?;
        let mut keys = std::collections::BTreeSet::new();
        for step in &replay.events {
            if step.key.is_empty() || !keys.insert(&step.key) {
                return Err("invalid_event_keys");
            }
            check = check
                .apply(step.event.clone(), step.at)
                .map_err(|_| "invalid_or_stale_observation")?
                .job;
        }
        // Freeze the selected client profile into the durable job; recovery
        // must not re-resolve a subsequently edited profile.
        replay.client = None;
        let input = serde_json::to_value(&replay).map_err(|_| "input_encoding")?;
        self.store
            .create_job(&replay.job, &replay.key, self.limits)
            .await
            .map_err(|_| "job_conflict_or_budget")?;
        // Rejected job specs never publish a recoverable envelope. If a process
        // dies between job creation and this insert, no event/effect has started;
        // resubmitting the unacknowledged request fills in the same input.
        self.store
            .ingest_observation("replay_input", &replay.key, &input, wall_time())
            .await
            .map_err(|_| "replay_input_conflict")?;
        let lease = self
            .store
            .acquire_lease(replay.job.id, "shadow-replay", wall_time(), 30)
            .await
            .map_err(|_| "job_lease_busy")?;
        let worker = Worker::new(
            Some(self.store.clone()),
            DisabledEffects,
            Options::default(),
        )
        .map_err(|_| "engine_configuration")?;
        for step in replay.events {
            worker
                .apply_recorded(&lease, &step.key, step.event, step.at, wall_time())
                .await
                .map_err(|_| "invalid_or_stale_observation")?;
            worker
                .record_decisions(&lease, wall_time())
                .await
                .map_err(|_| "engine_storage")?;
        }
        // Project the authoritative outbox, including intents committed before
        // a crash. Duplicate input events deliberately return no new intents.
        let mut reports = vec![];
        for row in self
            .store
            .outbox(replay.job.id)
            .await
            .map_err(|_| "report_storage")?
        {
            let intent = row.intent;
            let observed = replay
                .observed
                .get(&intent.sequence)
                .cloned()
                .unwrap_or_else(unknown_activity);
            let report =
                observer::compare(replay.key.clone(), observed, proposed_activity(&intent));
            self.store
                .ingest_observation(
                    "diff",
                    &format!("job:{}:{}", intent.job, intent.sequence),
                    &serde_json::to_value(&report).map_err(|_| "report_encoding")?,
                    wall_time(),
                )
                .await
                .map_err(|_| "report_storage")?;
            reports.push(report);
        }
        self.store
            .ingest_observation(
                "replay_complete",
                &replay.key,
                &json!({"reports":reports.len()}),
                wall_time(),
            )
            .await
            .map_err(|_| "report_storage")?;
        self.store
            .release_lease(&lease, wall_time())
            .await
            .map_err(|_| "lease_release")?;
        Ok(reports)
    }
    /// Restart-safe projection. Repeated scanning is harmless: report keys derive
    /// from durable source row IDs, never from wall-clock arrival order.
    pub async fn project(&self, after: u64) -> Result<u64, &'static str> {
        let records = self
            .store
            .observations_after(after, 1000)
            .await
            .map_err(|_| "store_unavailable")?;
        let mut cursor = after;
        for record in records {
            cursor = record.id;
            if record.source == "replay_input" {
                // On restart, retry the durable transcript after its previous
                // worker lease expires. Idempotent events/outbox projection make
                // already completed transcripts harmless to revisit.
                let replay: Replay =
                    serde_json::from_value(record.payload).map_err(|_| "invalid_stored_replay")?;
                self.replay(replay).await?;
                continue;
            }
            if record.source.starts_with("python_replay:") {
                let replay: legacy::LegacyReplay =
                    serde_json::from_value(record.payload).map_err(|_| "invalid_python_replay")?;
                let report = replay.comparison();
                self.store
                    .ingest_observation(
                        "diff",
                        &format!("observation:{}", record.id),
                        &serde_json::to_value(report).map_err(|_| "report_encoding")?,
                        record.at,
                    )
                    .await
                    .map_err(|_| "report_storage")?;
                continue;
            }
            let instance = record
                .source
                .strip_prefix("webhook:")
                .or_else(|| record.source.strip_prefix("history:"));
            if let Some(instance) = instance {
                let observation: ArrObservation = serde_json::from_value(record.payload)
                    .map_err(|_| "invalid_stored_observation")?;
                let action = match observation.kind {
                    Kind::Grab => Action::Grab,
                    Kind::Download => Action::Import,
                    Kind::EpisodeFileDelete | Kind::MovieFileDelete => Action::DeleteLibrary,
                    _ => Action::Hold,
                };
                // Upstream status does not establish the missing
                // access/decode/job facts. Never infer them from arrival order.
                let observed = Activity {
                    action,
                    actor: Actor::Unknown,
                    correlated: false,
                    safety: Safety::default(),
                };
                let proposed = Activity {
                    action: Action::Hold,
                    actor: Actor::Engine,
                    correlated: false,
                    safety: Safety::default(),
                };
                let fix = format!(
                    "{instance}:{}",
                    observation.download.as_deref().unwrap_or("uncorrelated")
                );
                let report = observer::compare(fix, observed, proposed);
                self.store
                    .ingest_observation(
                        "diff",
                        &format!("observation:{}", record.id),
                        &serde_json::to_value(report).map_err(|_| "report_encoding")?,
                        record.at,
                    )
                    .await
                    .map_err(|_| "report_storage")?;
            }
        }
        Ok(cursor)
    }
    pub async fn reports(&self) -> Result<Vec<Diff>, &'static str> {
        let mut cursor = 0;
        let mut result = vec![];
        loop {
            let records = self
                .store
                .observations_after(cursor, 1000)
                .await
                .map_err(|_| "store_unavailable")?;
            if records.is_empty() {
                break;
            }
            for record in records {
                cursor = record.id;
                if record.source == "diff" {
                    result.push(
                        serde_json::from_value(record.payload).map_err(|_| "invalid_report")?,
                    );
                }
            }
        }
        Ok(result)
    }
}
fn unknown_activity() -> Activity {
    Activity {
        action: Action::Hold,
        actor: Actor::Unknown,
        correlated: false,
        safety: Safety::default(),
    }
}
fn proposed_activity(intent: &Intent) -> Activity {
    // These describe the reducer's proposal at the committed event time. They
    // are not upstream observations or evidence of a remotely completed action.
    Activity {
        action: Action::from_effect(&intent.effect),
        actor: Actor::Engine,
        correlated: true,
        safety: Safety {
            access: Some(true),
            budget: Some(true),
            replacement_verified: Some(true),
            exact_import_decoded: Some(true),
            notification_unique: Some(true),
            no_auto_redownload: Some(true),
            reconciled: Some(true),
        },
    }
}

pub async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    app: Arc<App>,
    mut stream: S,
    now: u64,
) -> Result<(), std::io::Error> {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_request(&app, &mut stream, now),
    )
    .await
    .unwrap_or((408, json!({"error":"request_timeout"})));
    let body = serde_json::to_vec(&response.1)?;
    let headers = format!(
        "HTTP/1.1 {} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.0,
        body.len()
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        stream.write_all(headers.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| std::io::ErrorKind::TimedOut)?
}
async fn read_request<S: AsyncRead + Unpin>(app: &App, stream: &mut S, now: u64) -> (u16, Value) {
    let mut raw = vec![];
    loop {
        let mut part = [0; 4096];
        let read = match stream.read(&mut part).await {
            Ok(0) | Err(_) => return (400, json!({"error":"incomplete_request"})),
            Ok(n) => n,
        };
        raw.extend_from_slice(&part[..read]);
        if raw.len() > MAX_BODY + 8192 {
            return (413, json!({"error":"payload_too_large"}));
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        let used = match request.parse(&raw) {
            Ok(httparse::Status::Complete(n)) if n <= 8192 => n,
            Ok(httparse::Status::Partial) if raw.len() <= 8192 => continue,
            _ => return (400, json!({"error":"invalid_headers"})),
        };
        let lengths: Vec<_> = request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("content-length"))
            .collect();
        if lengths.len() > 1
            || request
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
        {
            return (400, json!({"error":"invalid_framing"}));
        }
        let length = if let Some(header) = lengths.first() {
            match std::str::from_utf8(header.value)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
            {
                Some(n) if n <= MAX_BODY => n,
                _ => return (413, json!({"error":"invalid_length"})),
            }
        } else {
            0
        };
        if raw.len() < used + length {
            continue;
        }
        if raw.len() != used + length {
            return (400, json!({"error":"invalid_framing"}));
        }
        let auth: Vec<_> = request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("authorization"))
            .collect();
        if auth.len() > 1 {
            return (401, json!({"error":"unauthorized"}));
        }
        let credential = auth
            .first()
            .and_then(|h| std::str::from_utf8(h.value).ok())
            .and_then(credential)
            .unwrap_or_default();
        return app
            .handle(
                request.method.unwrap_or(""),
                request.path.unwrap_or(""),
                &credential,
                &raw[used..],
                now,
            )
            .await;
    }
}
fn credential(value: &str) -> Option<String> {
    if let Some(bearer) = value.strip_prefix("Bearer ") {
        return Some(bearer.to_owned());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.strip_prefix("Basic ")?)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    Some(decoded.strip_prefix("pitcrew:")?.to_owned())
}

fn wall_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
