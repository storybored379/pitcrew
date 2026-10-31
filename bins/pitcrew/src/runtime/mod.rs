//! Supervised, bounded read-only composition. Only trusted upstream observations
//! enter authority; reports select catalog IDs but cannot supply identities/access.
mod catalog;
#[cfg(test)]
mod consumer_tests;
mod reports;
mod selection;
#[cfg(test)]
mod tests;
use pitcrew_config::Config;
use pitcrew_diagnosis::collectors::AgentCollector;
use pitcrew_intake::pipeline::{Clock, collectors::Collectors};
use pitcrew_store::Store;
use pitcrew_sync::{CatalogSnapshot, LibraryBinding, reader::Reader};
use pitcrew_upstream::{Auth, Endpoint, Limits, Request, ResponseFuture, Transport};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

pub struct Shared<T>(Arc<T>);
impl<T> Shared<T> {
    pub(crate) fn new(transport: T) -> Self {
        Self(Arc::new(transport))
    }
}
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Transport> Transport for Shared<T> {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.0.send(request)
    }
}
struct Arr<T> {
    client: pitcrew_arr::Client<Shared<T>>,
    reader: Reader<Shared<T>>,
    search: pitcrew_upstream::JsonClient<Shared<T>>,
}

#[derive(Default, Debug, Serialize)]
pub struct Tick {
    pub reports_examined: usize,
    pub authority_recorded: usize,
    pub reports_held: usize,
    pub effects_shadowed: usize,
}

pub struct Runtime<T> {
    store: Store,
    intake: pitcrew_intake_store::Store,
    pub bridge: pitcrew_bridge::Bridge,
    transport: Shared<T>,
    plex: Reader<Shared<T>>,
    arr: BTreeMap<String, Arr<T>>,
    collectors: Collectors<Shared<T>>,
    access_url: url::Url,
    owner_url: url::Url,
    token: String,
    bindings: Vec<LibraryBinding>,
    catalog: CatalogSnapshot,
    snapshot_path: PathBuf,
    clock: Arc<Clock>,
    ttl: u64,
    max_age: u64,
    batch: u32,
    max_pages: u32,
    max_series: usize,
    report_cursor: i64,
    job_cursor: u64,
    search_enabled: bool,
    authorities: BTreeMap<(String, String), Prepared>,
}

struct Prepared {
    target: pitcrew_intake::pipeline::ResolvedTarget,
    access: pitcrew_domain::Access,
}

pub fn snapshot_path(config: &Config) -> PathBuf {
    config
        .stack
        .catalog_snapshot_file
        .clone()
        .unwrap_or_else(|| config.stack.intake_database.with_extension("catalog.json"))
}

impl<T: Transport> Runtime<T> {
    fn now_seconds(&self) -> u64 {
        (self.clock)() / 1000
    }

    /// Constructs clients without contacting any upstream. Explicit stack opt-in
    /// and Plex/arr configuration are necessary for autonomous observations.
    pub async fn open(
        config: &Config,
        store: Store,
        transport: T,
        clock: Arc<Clock>,
    ) -> Result<Option<Self>, &'static str> {
        if !config.stack.enabled || config.plex.is_none() || config.arr.is_empty() {
            return Ok(None);
        }
        let plex = config.plex.as_ref().ok_or("runtime requires Plex")?;
        let token = plex
            .owner_token
            .resolve("plex.owner_token")
            .map_err(|_| "plex.owner_token unavailable")?;
        let transport = Shared(Arc::new(transport));
        // Whole-library reads outgrow the 4 MiB default: Radarr `/movie` is ~1.5 KB a title,
        // so 7,500 films is ~11 MB. Every catalog request is still bounded.
        let limits = Limits {
            timeout: std::time::Duration::from_secs(30),
            max_response_bytes: 64 * 1024 * 1024,
        };
        let endpoint = |url: &str, auth| {
            Endpoint::new(url, auth, limits).map_err(|_| "runtime upstream endpoint invalid")
        };
        let plex_reader = Reader::new(
            endpoint(
                &plex.url,
                Auth::plex(token.expose()).map_err(|_| "Plex credential invalid")?,
            )?,
            transport.clone(),
            100_000,
        )
        .map_err(|_| "Plex reader invalid")?;
        let mut arr = BTreeMap::new();
        let mut bindings = Vec::new();
        let mut libraries = BTreeSet::new();
        for entry in &config.arr {
            let key = entry
                .api_key
                .resolve("arr.api_key")
                .map_err(|_| "arr.api_key unavailable")?;
            let kind = match entry.kind {
                pitcrew_config::ArrKind::Sonarr => pitcrew_arr::Kind::Sonarr,
                pitcrew_config::ArrKind::Radarr => pitcrew_arr::Kind::Radarr,
            };
            let client = pitcrew_arr::Client::new(
                endpoint(
                    &entry.url,
                    Auth::arr(key.expose()).map_err(|_| "arr credential invalid")?,
                )?,
                transport.clone(),
                kind,
            );
            let reader = Reader::new(
                endpoint(
                    &entry.url,
                    Auth::arr(key.expose()).map_err(|_| "arr credential invalid")?,
                )?,
                transport.clone(),
                100_000,
            )
            .map_err(|_| "arr reader invalid")?;
            let search = pitcrew_upstream::JsonClient::new(
                endpoint(
                    &entry.url,
                    Auth::arr(key.expose()).map_err(|_| "arr credential invalid")?,
                )?,
                transport.clone(),
            );
            arr.insert(
                entry.id.clone(),
                Arr {
                    client,
                    reader,
                    search,
                },
            );
            for library in &entry.libraries {
                let id: u64 = library
                    .parse()
                    .map_err(|_| "arr.libraries must be numeric Plex section keys")?;
                if id == 0 || id.to_string() != *library || !libraries.insert(library.clone()) {
                    return Err(
                        "arr.libraries require unique canonical positive Plex section keys",
                    );
                }
                let paths = config
                    .runtime
                    .libraries
                    .iter()
                    .find(|p| p.library_id == *library);
                bindings.push(LibraryBinding {
                    server_scope: "primary".into(),
                    section_id: id,
                    library_id: library.clone(),
                    library_name: String::new(),
                    arr_instance_id: entry.id.clone(),
                    plex_root: paths.map_or("/", |p| &p.plex_root).into(),
                    arr_root: paths.map_or("/", |p| &p.arr_root).into(),
                    agent_root: paths.map_or("/", |p| &p.agent_root).into(),
                });
            }
        }
        if config
            .runtime
            .libraries
            .iter()
            .any(|p| !libraries.contains(&p.library_id))
        {
            return Err("runtime.libraries contains a library absent from arr.libraries");
        }
        let agent = config
            .agent
            .as_ref()
            .map(|agent| {
                let secret = agent
                    .shared_secret
                    .resolve("agent.shared_secret")
                    .map_err(|_| "agent.shared_secret unavailable")?;
                Ok::<_, &'static str>(AgentCollector::new(
                    endpoint(
                        &agent.url,
                        Auth::bearer(secret.expose()).map_err(|_| "agent credential invalid")?,
                    )?,
                    transport.clone(),
                ))
            })
            .transpose()?;
        let mut access_url =
            url::Url::parse(&config.stack.plex_auth_url).map_err(|_| "Plex auth URL invalid")?;
        access_url
            .path_segments_mut()
            .map_err(|_| "Plex auth URL invalid")?
            .pop_if_empty()
            .extend(["api", "servers", &plex.server_id, "shared_servers"]);
        let owner_url = url::Url::parse(&config.stack.plex_auth_url)
            .map_err(|_| "Plex auth URL invalid")?
            .join("api/v2/user")
            .map_err(|_| "Plex auth URL invalid")?;
        let intake = pitcrew_intake_store::Store::open(&config.stack.intake_database)
            .await
            .map_err(|_| "runtime intake unavailable")?;
        let bridge = pitcrew_bridge::Bridge::open(
            &config.stack.intake_database,
            store.clone(),
            crate::stack::bridge_options(config),
        )
        .await
        .map_err(|_| "runtime bridge unavailable")?;
        Ok(Some(Self {
            store,
            intake,
            bridge,
            transport,
            plex: plex_reader,
            arr,
            collectors: Collectors {
                agent,
                tautulli: None,
                tracearr: None,
            },
            access_url,
            owner_url,
            token: token.expose().into(),
            bindings,
            catalog: CatalogSnapshot::default(),
            snapshot_path: snapshot_path(config),
            clock,
            ttl: config.runtime.snapshot_ttl_seconds,
            max_age: config.stack.evidence_max_age_seconds,
            batch: config.runtime.batch_size,
            max_pages: config.runtime.max_pages,
            max_series: config.runtime.max_series,
            report_cursor: 0,
            job_cursor: 0,
            search_enabled: config.runtime.search_enabled,
            authorities: BTreeMap::new(),
        }))
    }

    pub async fn tick(&mut self) -> Result<Tick, &'static str> {
        self.tick_with_consumer(None).await
    }

    pub async fn tick_with_consumer(
        &mut self,
        consumer: Option<&pitcrew_bridge::admin::Consumer>,
    ) -> Result<Tick, &'static str> {
        let mut summary = self.reports().await?;
        if let Some(consumer) = consumer {
            consumer
                .tick(self.now_seconds())
                .await
                .map_err(|_| "admin consumer failed")?;
        }
        self.bridge
            .tick(self.now_seconds())
            .await
            .map_err(|_| "bridge tick failed")?;
        // Match proposals to this batch's trusted account/resource evidence.
        // Independent report/job cursors must not have to align to make progress.
        for prepared in self.authorities.values() {
            let Some(job) = self
                .store
                .owned_job(&prepared.target.resource)
                .await
                .map_err(|_| "resource ownership unavailable")?
            else {
                continue;
            };
            let now = self.now_seconds();
            let Ok(lease) = self
                .store
                .acquire_lease(job, "runtime-selection", now, 60)
                .await
            else {
                continue;
            };
            let selected = self.select(&lease).await;
            self.store
                .release_lease(&lease, self.now_seconds())
                .await
                .map_err(|_| "selection lease release failed")?;
            if let Err(reason) = selected {
                self.store.ingest_observation("admin:health", &format!("selection:{job}:{now}"),
                    &serde_json::json!({"component":"selection","job":job,"available":false,"reason":reason}), now)
                    .await.map_err(|_| "selection observation unavailable")?;
            }
        }
        let worker = pitcrew_engine::Worker::new(
            Some(self.store.clone()),
            crate::DisabledEffects,
            pitcrew_engine::Options::default(),
        )
        .map_err(|_| "shadow worker unavailable")?;
        let jobs = self
            .store
            .jobs_page(self.job_cursor, self.batch)
            .await
            .map_err(|_| "worker jobs unavailable")?;
        if jobs.is_empty() {
            self.job_cursor = 0;
        }
        for job in jobs {
            self.job_cursor = job;
            self.bridge
                .prepare_notifications(job, None)
                .await
                .map_err(|_| "notification preparation failed")?;
            let now = self.now_seconds();
            let Ok(lease) = self
                .store
                .acquire_lease(job, "runtime-shadow", now, 60)
                .await
            else {
                continue;
            };
            let driven = worker.drive(&lease, self.now_seconds()).await;
            self.store
                .release_lease(&lease, self.now_seconds())
                .await
                .map_err(|_| "worker lease release failed")?;
            summary.effects_shadowed += driven.map_err(|_| "shadow worker failed")?;
        }
        self.bridge
            .tick(self.now_seconds())
            .await
            .map_err(|_| "bridge projection failed")?;
        self.bridge
            .prepare_lifecycle_notifications(self.now_seconds())
            .await
            .map_err(|_| "lifecycle preparation failed")?;
        self.bridge
            .dispatch_lifecycle(&BTreeMap::new(), self.now_seconds(), true)
            .await
            .map_err(|_| "shadow lifecycle recording failed")?;
        Ok(summary)
    }

    pub async fn health(&self, component: &str, healthy: bool) -> Result<(), &'static str> {
        let now = self.now_seconds();
        self.store.ingest_observation("admin:health", &format!("{component}:{now}"),
            &serde_json::json!({"component":component,"available":healthy,"observed_at":now,"mode":"shadow"}), now)
            .await.map_err(|_| "health observation unavailable")?;
        Ok(())
    }
}
