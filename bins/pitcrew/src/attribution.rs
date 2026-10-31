//! Action-specific joins from trusted dispatch journals. An arr observation can
//! describe what happened, but cannot supply its own actor or safety authority.
use pitcrew_engine::{
    observer::{self, Action, Activity, Actor, Diff, DispatchReceipt, Safety},
    webhook::{ArrObservation, Kind},
};
use pitcrew_store::{ObservationRecord, OutboxStatus, Store};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Read only from an administrator-prepared external action log. The importer
/// hashes exact instance/download identifiers before persisting this evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAction {
    pub key: String,
    pub correlation: String,
    pub action: Action,
    pub actor: Actor,
}

#[derive(Clone, PartialEq, Eq)]
struct Attribution {
    actor: Actor,
    owner: String,
    proposal: Option<pitcrew_domain::Intent>,
}

#[derive(Default)]
pub struct Index {
    // None is a conflict, not a vacant slot. Later records cannot erase it.
    actions: BTreeMap<(String, Action), Option<Attribution>>,
}

impl Index {
    pub async fn load(store: &Store) -> Result<Self, &'static str> {
        let records = store
            .action_receipts()
            .await
            .map_err(|_| "store_unavailable")?;
        Self::from_records(store, &records).await
    }

    pub async fn from_records(
        store: &Store,
        records: &[ObservationRecord],
    ) -> Result<Self, &'static str> {
        let mut index = Self::default();
        let mut outboxes = BTreeMap::new();
        for record in records {
            if let Some(source) = record.source.strip_prefix("legacy_arr_action:") {
                let action: ExternalAction = serde_json::from_value(record.payload.clone())
                    .map_err(|_| "invalid_action_record")?;
                if !matches!(action.actor, Actor::ExternalTool | Actor::ExternalImporter) {
                    return Err("invalid_action_actor");
                }
                index.insert(
                    action.correlation,
                    action.action,
                    Attribution {
                        actor: action.actor,
                        owner: source.into(),
                        proposal: None,
                    },
                );
            } else if record.source == "engine_dispatch" {
                let receipt: DispatchReceipt = serde_json::from_value(record.payload.clone())
                    .map_err(|_| "invalid_dispatch_receipt")?;
                // Also validate on read: a forged/pending/shadow proposal does
                // not become a real engine action by using this source name.
                let rows = match outboxes.entry(receipt.intent.job) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
                        store
                            .outbox(receipt.intent.job)
                            .await
                            .map_err(|_| "store_unavailable")?,
                    ),
                };
                if rows.iter().any(|row| {
                    row.intent == receipt.intent
                        && matches!(
                            row.status,
                            OutboxStatus::Started
                                | OutboxStatus::Completed
                                | OutboxStatus::Uncertain
                        )
                }) {
                    index.insert(
                        receipt.correlation,
                        Action::from_effect(&receipt.intent.effect),
                        Attribution {
                            actor: Actor::Engine,
                            owner: receipt.intent.job.to_string(),
                            proposal: Some(receipt.intent),
                        },
                    );
                }
            }
        }
        Ok(index)
    }

    fn insert(&mut self, correlation: String, action: Action, attribution: Attribution) {
        self.actions
            .entry((correlation, action))
            .and_modify(|existing| {
                if existing.as_ref() != Some(&attribution) {
                    *existing = None;
                }
            })
            .or_insert(Some(attribution));
    }

    pub fn compare(&self, fix: String, correlation: Option<String>, action: Action) -> Diff {
        let attribution = correlation
            .and_then(|key| self.actions.get(&(key, action)))
            .and_then(Option::as_ref);
        let observed = Activity {
            action,
            actor: attribution.map_or(Actor::Unknown, |a| a.actor),
            correlated: attribution.is_some(),
            safety: Safety::default(),
        };
        let proposed = attribution.and_then(|a| a.proposal.as_ref()).map_or(
            Activity {
                action: Action::Hold,
                actor: Actor::Engine,
                correlated: false,
                safety: Safety::default(),
            },
            crate::proposed_activity,
        );
        observer::compare(fix, observed, proposed)
    }

    pub fn arr(&self, record: &ObservationRecord) -> Result<Option<Diff>, &'static str> {
        let instance = record
            .source
            .strip_prefix("history:")
            .or_else(|| record.source.strip_prefix("webhook:"));
        let Some(instance) = instance else {
            return Ok(None);
        };
        let observation: ArrObservation = serde_json::from_value(record.payload.clone())
            .map_err(|_| "invalid_stored_observation")?;
        let action = match observation.kind {
            Kind::Grab => Action::Grab,
            Kind::Download => Action::Import,
            Kind::EpisodeFileDelete | Kind::MovieFileDelete => Action::DeleteLibrary,
            _ => Action::Hold,
        };
        Ok(Some(
            self.compare(
                format!("observation:{}", record.id),
                observation
                    .download
                    .as_deref()
                    .and_then(|download| observer::download_correlation(instance, download)),
                action,
            ),
        ))
    }

    /// Queue state is not an action receipt. Report the exact download's recorded
    /// grab owner separately; an import comparison needs its own import receipt.
    pub fn queue(&self, payload: &mut serde_json::Value) {
        // Preserve malformed/older journal rows verbatim during export. They
        // cannot acquire actor fields, and indexing a non-object would panic.
        if !payload.is_object() {
            return;
        }
        let correlation = payload
            .get("download_correlation")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let grab = self.compare(String::new(), correlation.clone(), Action::Grab);
        let imported = self.compare(String::new(), correlation, Action::Import);
        payload["download_actor"] =
            serde_json::to_value(grab.observed.actor).expect("actor serialization");
        payload["import_actor"] =
            serde_json::to_value(imported.observed.actor).expect("actor serialization");
        // This is the arbiter's conservative proposal from the queue facts, not
        // an authorization derived from actor ownership.
        payload["observed_import"] = serde_json::json!(imported.observed.correlated);
        if payload.get("download_correlation").is_some()
            && let Ok(decision) = serde_json::from_value::<pitcrew_arbiter::shadow::Decision>(
                payload["decision"].clone(),
            )
        {
            let action = if imported.observed.correlated {
                pitcrew_arbiter::shadow::LegacyAction::Import
            } else {
                pitcrew_arbiter::shadow::LegacyAction::Unknown
            };
            payload["legacy_action"] = serde_json::json!(action);
            payload["divergence"] =
                serde_json::json!(pitcrew_arbiter::shadow::divergence(action, decision));
        }
    }
}
