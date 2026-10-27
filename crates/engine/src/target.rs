//! Exact Plex destination verification composed with the notification executor.
use crate::{CallError, CallFuture, Outcome, Upstream};
use pitcrew_domain::{
    Effect, Intent, IntentKey, Observation, State,
    media::{ProbeOutcome, StreamFacts},
};
use pitcrew_plex::Client;
use pitcrew_store::{OutboxStatus, Store, policy::IdentityFingerprint};
use pitcrew_upstream::{Transport, agent};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetBinding {
    pub client: String,
    pub rating_key: u64,
    pub path: String,
    pub fingerprint: IdentityFingerprint,
}

/// A trusted arr/agent observation binds the imported revision and Plex target.
/// This method itself conveys no library access or proof of a successful decode.
pub async fn bind(
    store: &Store,
    intent: &Intent,
    target: &TargetBinding,
    now: u64,
) -> Result<(), CallError> {
    if !matches!(intent.effect, Effect::CheckTarget { .. })
        || !store
            .outbox(intent.job)
            .await
            .map_err(unavailable)?
            .iter()
            .any(|row| row.intent == *intent)
    {
        return Err(CallError::Unavailable);
    }
    store
        .ingest_observation(
            "target_route",
            &key(intent),
            &serde_json::to_value(target).map_err(unavailable)?,
            now,
        )
        .await
        .map_err(unavailable)?;
    Ok(())
}

pub struct TargetVerifier<P, A, N> {
    pub store: Store,
    pub plex: Client<P>,
    pub agent: agent::Client<A>,
    pub notifications: N,
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub max_identity_age: u64,
}
impl<P: Transport, A: Transport, N: Upstream> Upstream for TargetVerifier<P, A, N> {
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        self.notifications.write(intent)
    }
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(async move {
            let Effect::CheckTarget { file, constraints } = &intent.effect else {
                return Err(CallError::Unavailable);
            };
            let stored = self.store.load_job(intent.job).await.map_err(unavailable)?;
            if stored.job.state() != State::Imported
                || stored.job.expected_observation()
                    != Some(IntentKey {
                        job: intent.job,
                        sequence: intent.sequence,
                    })
                || !self
                    .store
                    .outbox(intent.job)
                    .await
                    .map_err(unavailable)?
                    .iter()
                    .any(|row| {
                        row.intent == *intent
                            && matches!(row.status, OutboxStatus::Started | OutboxStatus::Uncertain)
                    })
            {
                return Err(CallError::Unavailable);
            }
            let record = self
                .store
                .observations()
                .await
                .map_err(unavailable)?
                .into_iter()
                .find(|row| row.source == "target_route" && row.key == key(intent))
                .ok_or(CallError::Unavailable)?;
            let binding: TargetBinding =
                serde_json::from_value(record.payload).map_err(unavailable)?;
            let metadata = self
                .plex
                .metadata(binding.rating_key)
                .await
                .map_err(unavailable)?;
            // A multi-version/part title needs explicit selection support before
            // it can be called ready. Never accept a matching path among ghosts.
            if metadata.len() != 1 || metadata[0].rating_key != binding.rating_key.to_string() {
                return Err(CallError::Unavailable);
            }
            let media = metadata[0].media.as_ref().ok_or(CallError::Unavailable)?;
            if media.len() != 1 {
                return Err(CallError::Unavailable);
            }
            let parts = media[0]
                .get("Part")
                .and_then(|v| v.as_array())
                .ok_or(CallError::Unavailable)?;
            if parts.len() != 1
                || parts[0].get("file").and_then(|v| v.as_str()) != Some(binding.path.as_str())
            {
                return Err(CallError::Unavailable);
            }
            let evidence = self
                .agent
                .evidence(&binding.path, false)
                .await
                .map_err(unavailable)?;
            // A cached or future-dated agent response cannot certify the current
            // destination even when its file tuple matches the registry.
            let now = (self.clock)();
            let observed_at = evidence.observed_at_unix_ms / 1000;
            if observed_at > now || now - observed_at > self.max_identity_age {
                return Err(CallError::Unavailable);
            }
            let current = IdentityFingerprint {
                agent: evidence.file,
                ..binding.fingerprint
            };
            self.store
                .validate_file(
                    &binding.client,
                    &stored.spec.resource,
                    *file,
                    &current,
                    now,
                    self.max_identity_age,
                )
                .await
                .map_err(unavailable)?;
            let mut satisfied = true;
            if constraints.require_sdr
                || constraints.forbid_dolby_vision_profile5
                || constraints.language.is_some()
            {
                let probe = evidence.header.as_ref().ok_or(CallError::Unavailable)?;
                if probe.outcome != ProbeOutcome::Passed || !probe.faults.is_empty() {
                    return Err(CallError::Unavailable);
                }
                if constraints.forbid_dolby_vision_profile5 {
                    satisfied &= probe
                        .excludes_dolby_vision_profile5()
                        .ok_or(CallError::Unavailable)?;
                }
                if let Some(language) = &constraints.language {
                    let languages: Vec<_> = probe
                        .streams
                        .iter()
                        .filter_map(|stream| match stream {
                            StreamFacts::Audio { language, .. } => Some(language.as_deref()),
                            _ => None,
                        })
                        .collect();
                    if languages.is_empty() || languages.contains(&None) {
                        return Err(CallError::Unavailable);
                    }
                    satisfied &= languages
                        .iter()
                        .any(|v| v.is_some_and(|v| v.eq_ignore_ascii_case(language)));
                }
                if constraints.require_sdr {
                    let ranges: Vec<_> = probe
                        .streams
                        .iter()
                        .filter_map(|stream| match stream {
                            StreamFacts::Video { dynamic_range, .. } => {
                                Some(dynamic_range.as_deref())
                            }
                            _ => None,
                        })
                        .collect();
                    if ranges.is_empty() || ranges.contains(&None) {
                        return Err(CallError::Unavailable);
                    }
                    satisfied &= ranges.iter().all(|v| *v == Some("SDR"));
                }
            }
            Ok(Outcome::Observed(Observation::TargetChecked {
                file: *file,
                plex_file: *file,
                constraints_satisfied: satisfied,
            }))
        })
    }
}
fn key(intent: &Intent) -> String {
    format!("{}:{}", intent.job, intent.sequence)
}
fn unavailable<E>(_: E) -> CallError {
    CallError::Unavailable
}
