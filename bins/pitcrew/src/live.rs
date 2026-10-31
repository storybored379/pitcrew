//! Offline live composition. Valid certificates are necessary but not sufficient:
//! the executable refuses dispatch until trusted ownership and scheduling exist.
use crate::{live_delivery, runtime::Shared};
use pitcrew_arr::Kind;
use pitcrew_config::{ArrKind, Config, EngineMode};
use pitcrew_engine::{
    delivery::Notifications,
    live::{Certification, CertificationGate, OwnerAcknowledgement, Router},
    target::TargetVerifier,
};
use pitcrew_store::{Store, policy::ClientCapabilities};
use pitcrew_upstream::{Auth, Endpoint, Limits, Transport};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, path::Path, sync::Arc};

pub const UNAVAILABLE: &str = "live runtime requires a trusted grab ownership receipt producer, per-transition current access refresh, per-intent download/import and Plex target binding, and authoritative interference/dead-release policy polling; these prerequisites are not implemented";

/// Digest of the exact bundled, synthetic read contract. This is not proof of
/// write behavior; the owner must separately certify the configured client.
pub fn fixture_digest(kind: Kind) -> String {
    let bytes: &[u8] = match kind {
        Kind::Sonarr => {
            include_bytes!("../../../crates/arr/tests/fixtures/sonarr-4.0.19.2979.json")
        }
        Kind::Radarr => {
            include_bytes!("../../../crates/arr/tests/fixtures/radarr-6.3.0.10514.json")
        }
    };
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityEvidence {
    pub certification: Certification,
    pub capabilities: ClientCapabilities,
    pub evidence: serde_json::Value,
}

struct CertifiedClient {
    gate: CertificationGate,
    evidence: CapabilityEvidence,
}

pub struct Validated<'a> {
    config: &'a Config,
    store: Store,
    clients: BTreeMap<String, CertifiedClient>,
}

fn record<T: DeserializeOwned>(path: &Path) -> Result<T, &'static str> {
    // Refuse symlinks/devices/FIFOs before opening and bound the actual read too.
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "record unavailable")?;
    if !metadata.file_type().is_file() {
        return Err("record must be a regular file");
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "record unavailable")?
        .take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| "record unreadable")?;
    if bytes.len() > 65_536 {
        return Err("record exceeds 64 KiB");
    }
    serde_json::from_slice(&bytes).map_err(|_| "record JSON invalid")
}

/// Checks local configuration and files only. It does not resolve credentials,
/// contact an upstream, create certification, or restore a reset capability.
pub async fn validate<'a>(
    config: &'a Config,
    store: Option<Store>,
    now: u64,
) -> Result<Validated<'a>, &'static str> {
    if config.engine.mode != EngineMode::Live
        || !config.engine.acknowledge_live_mutations
        || !config.live.enabled
    {
        return Err(
            "live requires engine.mode=live, engine.acknowledge_live_mutations=true and live.enabled=true",
        );
    }
    let store = store.ok_or("live requires a durable engine store")?;
    store
        .health()
        .await
        .map_err(|_| "live engine store is unavailable")?;
    if !config.stack.enabled {
        return Err("live requires stack.enabled=true for authenticated intake and diagnosis");
    }
    if config.plex.is_none() {
        return Err("live requires Plex target and current access verification");
    }
    if config.agent.is_none() {
        return Err("live requires the exact-file media agent");
    }
    if config.arr.is_empty() {
        return Err("live requires at least one configured Sonarr or Radarr client");
    }
    if !config.runtime.search_enabled {
        return Err("live requires runtime.search_enabled=true for budgeted candidate selection");
    }
    if config.live.certifications.len() != config.arr.len() {
        return Err("live requires exactly one certification per configured arr client");
    }
    let mut clients = BTreeMap::new();
    for arr in &config.arr {
        let entries: Vec<_> = config
            .live
            .certifications
            .iter()
            .filter(|c| c.client == arr.id)
            .collect();
        let [input] = entries.as_slice() else {
            return Err(
                "live certification client IDs must match configured arr clients exactly once",
            );
        };
        let kind = match arr.kind {
            ArrKind::Sonarr => Kind::Sonarr,
            ArrKind::Radarr => Kind::Radarr,
        };
        let configured = Certification {
            id: input.id.clone(),
            client: input.client.clone(),
            kind,
            version: input.version.clone(),
            fixture_digest: input.fixture_digest.clone(),
            certified_at: input.certified_at,
            expires_at: input.expires_at,
        };
        if configured.fixture_digest != fixture_digest(kind) {
            return Err("live certification fixture_digest does not match the bundled contract");
        }
        let certificate: Certification = record(&input.certificate_file).map_err(
            |_| "live certificate_file must contain a bounded regular certification record",
        )?;
        let acknowledgement: OwnerAcknowledgement = record(&input.acknowledgement_file).map_err(
            |_| "live acknowledgement_file must contain a separate bounded owner acknowledgement",
        )?;
        let gate =
            CertificationGate::from_records(configured.clone(), true, certificate, acknowledgement);
        gate.authorize(&configured.version, now)
            .map_err(|_| "live certificate and owner acknowledgement must match configuration and be current")?;
        let evidence: CapabilityEvidence = record(&input.capabilities_file).map_err(
            |_| "live capabilities_file must contain bounded reviewed capability evidence",
        )?;
        if evidence.certification != configured || evidence.evidence.is_null() {
            return Err(
                "live capability evidence must name the exact certification and include evidence",
            );
        }
        if evidence.capabilities.held_category.is_some() {
            return Err(
                "live held-category preservation transport is not implemented or certified",
            );
        }
        // Read only: a restart must never turn a reset ledger into a grant.
        let ledger = store
            .capabilities(&arr.id, &configured.version)
            .await
            .map_err(|_| "live capability ledger is unavailable")?
            .ok_or("live requires a reviewed version-bound capability ledger")?;
        if ledger.capabilities.as_ref() != Some(&evidence.capabilities)
            || ledger.evidence != evidence.evidence
            || ledger.updated_at > now
        {
            return Err(
                "live capability ledger is reset or differs from reviewed evidence; a new audited review is required",
            );
        }
        clients.insert(arr.id.clone(), CertifiedClient { gate, evidence });
    }
    Ok(Validated {
        config,
        store,
        clients,
    })
}

pub type LiveRouter<T> =
    Router<Shared<T>, Shared<T>, TargetVerifier<Shared<T>, Shared<T>, Notifications>>;

pub struct Composition<T> {
    pub routers: BTreeMap<String, LiveRouter<T>>,
    pub admin_notifier: String,
}

impl Validated<'_> {
    /// Build the real adapter graph without dispatching it. No worker/scheduler
    /// is returned: runtime ownership/access producers remain prerequisites.
    pub fn compose<T: Transport>(
        self,
        transport: T,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
        delivery: &dyn live_delivery::TransportFactory,
    ) -> Result<Composition<T>, &'static str> {
        let config = self.config;
        let shared = Shared::new(transport);
        let plex = config.plex.as_ref().expect("validated Plex configuration");
        let plex_token = plex
            .owner_token
            .resolve("plex.owner_token")
            .map_err(|_| "live Plex credential unavailable")?;
        let agent = config
            .agent
            .as_ref()
            .expect("validated agent configuration");
        let agent_token = agent
            .shared_secret
            .resolve("agent.shared_secret")
            .map_err(|_| "live agent credential unavailable")?;
        let endpoint = |url: &str, auth| {
            Endpoint::new(url, auth, Limits::default())
                .map_err(|_| "live upstream endpoint invalid")
        };
        let mut routers = BTreeMap::new();
        let mut admin_notifier = String::new();
        for (id, certified) in self.clients {
            let arr = config
                .arr
                .iter()
                .find(|a| a.id == id)
                .expect("validated arr configuration");
            let key = arr
                .api_key
                .resolve("arr.api_key")
                .map_err(|_| "live arr credential unavailable")?;
            let adapters =
                live_delivery::compose_with(config, self.store.clone(), clock.clone(), delivery)?;
            admin_notifier = adapters.admin_notifier;
            let target = TargetVerifier {
                store: self.store.clone(),
                plex: pitcrew_plex::Client::new(
                    endpoint(
                        &plex.url,
                        Auth::plex(plex_token.expose())
                            .map_err(|_| "live Plex credential invalid")?,
                    )?,
                    shared.clone(),
                ),
                agent: pitcrew_upstream::agent::Client::new(
                    endpoint(
                        &agent.url,
                        Auth::bearer(agent_token.expose())
                            .map_err(|_| "live agent credential invalid")?,
                    )?,
                    shared.clone(),
                ),
                notifications: adapters.notifications,
                clock: clock.clone(),
                max_identity_age: config.stack.evidence_max_age_seconds,
            };
            let router = Router::new(
                self.store.clone(),
                pitcrew_arr::Client::new(
                    endpoint(
                        &arr.url,
                        Auth::arr(key.expose()).map_err(|_| "live arr credential invalid")?,
                    )?,
                    shared.clone(),
                    certified.evidence.certification.kind,
                ),
                pitcrew_upstream::agent::Client::new(
                    endpoint(
                        &agent.url,
                        Auth::bearer(agent_token.expose())
                            .map_err(|_| "live agent credential invalid")?,
                    )?,
                    shared.clone(),
                ),
                target,
                certified.gate,
                clock.clone(),
                config.stack.evidence_max_age_seconds,
            )
            .map_err(|_| "live router configuration invalid")?;
            routers.insert(id, router);
        }
        Ok(Composition {
            routers,
            admin_notifier,
        })
    }
}
