//! Certified, durable effect routing. The executable constructs this
//! router offline but refuses dispatch until runtime prerequisites and certification
//! are complete. Tests use fake transports.
//! No library-file deletion or automatic uncertain-write resend exists here.
use crate::{CallError, CallFuture, Outcome, Upstream, notification};
use pitcrew_arr::{Client, ImportFile, Kind, MediaFile, Page, QueueRecord};
use pitcrew_domain::{
    Constraints, DeleteResolution, Effect, GrabResolution, ImportResolution, Intent, IntentKey,
    Observation,
    media::{MediaEvidence, ProbeOutcome, ReleaseGuid, ReleaseIdentity},
};
use pitcrew_store::{
    OutboxStatus, Store, StoredJob,
    policy::{ExclusionKind, IdentityFingerprint, NewExclusion},
};
use pitcrew_upstream::{LivePermit, Mode, Transport, agent};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Certification {
    pub id: String,
    pub client: String,
    pub kind: Kind,
    /// Full exact version reported by system/status, including the build suffix.
    pub version: String,
    pub fixture_digest: String,
    pub certified_at: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerAcknowledgement {
    pub certification: Certification,
    pub acknowledge_live_mutations: bool,
}

/// Config and acknowledgement must name the complete same certification. This
/// object is not a substitute for current access, budgets, ownership or identity.
pub struct CertificationGate {
    configured: Certification,
    record: Certification,
    acknowledgement: OwnerAcknowledgement,
    enabled: bool,
}
impl CertificationGate {
    /// Compose a gate from records already read and bounded by the host. Neither
    /// construction path authorizes any write until `authorize` succeeds.
    pub fn from_records(
        configured: Certification,
        enabled: bool,
        record: Certification,
        acknowledgement: OwnerAcknowledgement,
    ) -> Self {
        Self {
            configured,
            enabled,
            record,
            acknowledgement,
        }
    }

    pub fn from_files(
        configured: Certification,
        enabled: bool,
        record: &Path,
        acknowledgement: &Path,
    ) -> Result<Self, CallError> {
        Ok(Self::from_records(
            configured,
            enabled,
            serde_json::from_slice(&std::fs::read(record).map_err(unavailable)?)
                .map_err(unavailable)?,
            serde_json::from_slice(&std::fs::read(acknowledgement).map_err(unavailable)?)
                .map_err(unavailable)?,
        ))
    }

    pub fn authorize(&self, version: &str, now: u64) -> Result<LivePermit, CallError> {
        let certificate = &self.record;
        let release = match certificate.kind {
            Kind::Sonarr => "4.0.19.2979",
            Kind::Radarr => "6.3.0.10514",
        };
        if !self.enabled
            || !self.acknowledgement.acknowledge_live_mutations
            || self.configured != *certificate
            || self.acknowledgement.certification != *certificate
            || certificate.version != version
            || version != release
            || certificate.id.is_empty()
            || certificate.client.is_empty()
            || certificate.fixture_digest.is_empty()
            || certificate.certified_at > now
            || certificate.expires_at <= now
        {
            return Err(CallError::Unavailable);
        }
        LivePermit::authorize(Mode::Live, true).map_err(unavailable)
    }
}

/// Local-only transport correlation, durably bound to one exact outbox intent.
/// Creation belongs to authenticated observation/intake code, never viewer text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub media_id: u64,
    pub series_id: Option<u64>,
    pub original_arr_file: u64,
    pub release: Option<ReleaseRoute>,
    pub download: Option<DownloadRoute>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRoute {
    pub release: u64,
    pub guid: String,
    pub indexer: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadRoute {
    pub download: u64,
    pub external_id: String,
    pub queue_id: u64,
    pub path: String,
}

/// Persist before dispatch; conflicting rebinding is rejected by the store's
/// observation key. An outbox row is still required before this can execute.
pub async fn bind(
    store: &Store,
    intent: &Intent,
    route: &Route,
    now: u64,
) -> Result<(), CallError> {
    if !store
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
            "live_route",
            &key(intent),
            &serde_json::to_value(route).map_err(unavailable)?,
            now,
        )
        .await
        .map_err(unavailable)?;
    Ok(())
}

/// Fallback owns notifications and Plex target verification. Unsupported effects
/// fail closed. Each HTTP client is independently bounded by its transport limits.
pub struct Router<A, G, F> {
    store: Store,
    arr: Client<A>,
    agent: agent::Client<G>,
    fallback: F,
    gate: CertificationGate,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    max_identity_age: u64,
}
impl<A: Transport, G: Transport, F: Upstream> Router<A, G, F> {
    pub fn new(
        store: Store,
        arr: Client<A>,
        agent: agent::Client<G>,
        fallback: F,
        gate: CertificationGate,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
        max_identity_age: u64,
    ) -> Result<Self, CallError> {
        if arr.kind() != gate.record.kind || max_identity_age == 0 {
            return Err(CallError::Unavailable);
        }
        Ok(Self {
            store,
            arr,
            agent,
            fallback,
            gate,
            clock,
            max_identity_age,
        })
    }

    async fn context(&self, intent: &Intent) -> Result<(StoredJob, Route), CallError> {
        let job = self.store.load_job(intent.job).await.map_err(unavailable)?;
        if job.job.expected_observation()
            != Some(IntentKey {
                job: intent.job,
                sequence: intent.sequence,
            })
            || !crate::dispatchable(&intent.effect, job.job.state())
            || !self
                .store
                .outbox(intent.job)
                .await
                .map_err(unavailable)?
                .iter()
                .any(|row| {
                    row.intent == *intent
                        && (row.status == OutboxStatus::Started
                            || (row.status == OutboxStatus::Uncertain
                                && !crate::is_write(&intent.effect)))
                })
        {
            return Err(CallError::Unavailable);
        }
        let row = self
            .store
            .observations()
            .await
            .map_err(unavailable)?
            .into_iter()
            .find(|row| row.source == "live_route" && row.key == key(intent))
            .ok_or(CallError::Unavailable)?;
        let route = serde_json::from_value(row.payload).map_err(unavailable)?;
        Ok((job, route))
    }

    async fn evidence(&self, path: &str, decode: bool) -> Result<MediaEvidence, CallError> {
        let evidence = self
            .agent
            .evidence(path, decode)
            .await
            .map_err(unavailable)?;
        if !(self.clock)()
            .checked_sub(evidence.observed_at_unix_ms / 1000)
            .is_some_and(|age| age <= self.max_identity_age)
        {
            return Err(CallError::Unavailable);
        }
        Ok(evidence)
    }

    async fn fingerprint(
        &self,
        id: u64,
        decode: bool,
    ) -> Result<(IdentityFingerprint, MediaEvidence), CallError> {
        let file = self.arr.file(id).await.map_err(unavailable)?;
        if file.id != id {
            return Err(CallError::Unavailable);
        }
        let evidence = self
            .evidence(file.path.as_deref().ok_or(CallError::Unavailable)?, decode)
            .await?;
        let fingerprint = file_fingerprint(&file, evidence.file)?;
        Ok((fingerprint, evidence))
    }

    async fn original(&self, job: &StoredJob, route: &Route) -> Result<(), CallError> {
        if self
            .arr
            .installed_file(route.media_id)
            .await
            .map_err(unavailable)?
            != route.original_arr_file
        {
            return Err(CallError::Unavailable);
        }
        let (current, _) = self.fingerprint(route.original_arr_file, false).await?;
        self.store
            .validate_file(
                &self.gate.record.client,
                &job.spec.resource,
                job.job.current_original(),
                &current,
                (self.clock)(),
                self.max_identity_age,
            )
            .await
            .map_err(unavailable)
    }

    async fn permit(&self, job: &StoredJob) -> Result<LivePermit, CallError> {
        let version = self.arr.version().await.map_err(unavailable)?;
        let now = (self.clock)();
        // Version changes immediately clear capabilities, even if certification
        // subsequently denies this write. An admin must recertify the new client.
        self.store
            .observe_client_version(&self.gate.record.client, &version, now)
            .await
            .map_err(unavailable)?;
        if self
            .store
            .capabilities(&self.gate.record.client, &version)
            .await
            .map_err(unavailable)?
            .and_then(|ledger| ledger.capabilities)
            .is_none()
        {
            return Err(CallError::Unavailable);
        }
        if !job.job.mutation_authorized(now) {
            return Err(CallError::Unavailable);
        }
        self.gate.authorize(&version, now)
    }

    async fn queue(&self, route: &Route, download: u64) -> Result<QueueRecord, CallError> {
        let binding = route.download.as_ref().ok_or(CallError::Unavailable)?;
        if binding.download != download {
            return Err(CallError::Unavailable);
        }
        let page = self.arr.queue(Page::default()).await.map_err(unavailable)?;
        let mut matches = page.records.into_iter().filter(|row| {
            row.id == binding.queue_id && row.download_id.as_deref() == Some(&binding.external_id)
        });
        let row = matches.next().ok_or(CallError::Unavailable)?;
        let media = match self.gate.record.kind {
            Kind::Sonarr => row.episode_id,
            Kind::Radarr => row.movie_id,
        };
        if matches.next().is_some() || media != Some(route.media_id) {
            return Err(CallError::Unavailable);
        }
        Ok(row)
    }

    fn release_identity(&self, release: &ReleaseRoute) -> ReleaseIdentity {
        ReleaseIdentity {
            guid: Some(ReleaseGuid {
                source: format!("{}:{}", self.gate.record.client, release.indexer),
                value: release.guid.clone(),
            }),
            ..ReleaseIdentity::default()
        }
    }

    async fn execute_write(&self, intent: &Intent) -> Result<Outcome, CallError> {
        if notification(&intent.effect) {
            // Owner approval gates every external write. Terminal alerts still
            // need to work while arr is unavailable, so this check is local.
            self.gate
                .authorize(&self.gate.record.version, (self.clock)())?;
            return self.fallback.write(intent).await;
        }
        let (job, route) = self.context(intent).await?;
        self.original(&job, &route).await?;
        let permit = self.permit(&job).await?;
        let observation = match &intent.effect {
            Effect::Grab {
                release,
                reservation,
            } => {
                let candidate = route
                    .release
                    .as_ref()
                    .filter(|candidate| candidate.release == *release)
                    .ok_or(CallError::Unavailable)?;
                if *reservation == 0
                    || *reservation > job.job.reserved_budget()
                    || self
                        .store
                        .release_is_excluded(
                            &self.gate.record.client,
                            *release,
                            &self.release_identity(candidate),
                            (self.clock)(),
                        )
                        .await
                        .map_err(unavailable)?
                {
                    return Err(CallError::Unavailable);
                }
                let releases = self
                    .arr
                    .releases(route.media_id)
                    .await
                    .map_err(unavailable)?;
                if !releases.iter().any(|found| {
                    found.guid == candidate.guid
                        && found.indexer_id == candidate.indexer
                        && found.approved == Some(true)
                        && found.rejected == Some(false)
                        && found.download_allowed == Some(true)
                }) {
                    return Err(CallError::Unavailable);
                }
                // A search can outlive a concurrent replacement. The original
                // observed before the search must still be installed at dispatch.
                self.original(&job, &route).await?;
                self.final_access(intent).await?;
                let accepted = self
                    .arr
                    .grab(&permit, route.media_id, &candidate.guid, candidate.indexer)
                    .await
                    .map_err(unavailable)?;
                // Record only an explicit accepted download receipt. Absence is
                // unknown; a release GUID/title is never substituted for it.
                if let Some(download) = accepted
                    .get("downloadId")
                    .and_then(serde_json::Value::as_str)
                {
                    crate::observer::record_dispatch(
                        &self.store,
                        intent,
                        &self.gate.record.client,
                        download,
                        (self.clock)(),
                    )
                    .await
                    .map_err(unavailable)?;
                }
                // Arr's release response is not an ownership receipt. Queue/history
                // correlation is required even after HTTP 200.
                Observation::GrabAmbiguous
            }
            Effect::RemoveRejectedDownload {
                download,
                blocklist,
                skip_redownload,
            } => {
                if !blocklist || !skip_redownload {
                    return Err(CallError::Unavailable);
                }
                let queue = self.queue(&route, *download).await?;
                let release = route.release.as_ref().ok_or(CallError::Unavailable)?;
                self.store.exclude_release(&NewExclusion {
                    client: self.gate.record.client.clone(), release: release.release,
                    identity: self.release_identity(release), kind: ExclusionKind::Quarantine,
                    reason: "download failed exact-file verification".into(),
                    evidence: serde_json::json!({"job":intent.job,"intent":intent.sequence,"download":download}),
                    pinned: false, created_at:(self.clock)(),
                }, &key(intent)).await.map_err(unavailable)?;
                self.final_access(intent).await?;
                self.arr
                    .blocklist_queue(&permit, queue.id)
                    .await
                    .map_err(unavailable)?;
                crate::observer::record_dispatch(
                    &self.store,
                    intent,
                    &self.gate.record.client,
                    &route
                        .download
                        .as_ref()
                        .ok_or(CallError::Unavailable)?
                        .external_id,
                    (self.clock)(),
                )
                .await
                .map_err(unavailable)?;
                Observation::DeleteAcknowledged
            }
            Effect::ImportVerifiedDownload {
                download,
                file,
                original,
            } => {
                let queue = self.queue(&route, *download).await?;
                let binding = route.download.as_ref().ok_or(CallError::Unavailable)?;
                if queue.status.as_deref() != Some("completed")
                    || !queue.output_path.as_deref().is_some_and(|output| {
                        !output.is_empty() && Path::new(&binding.path).starts_with(output)
                    })
                {
                    return Err(CallError::Unavailable);
                }
                // Import immediately after verification. No category changes or
                // sleeps race an external queue actor such as InfiniDysk.
                let evidence = self.evidence(&binding.path, true).await?;
                if !source_constraints(&job.spec.constraints, &evidence)? {
                    return Err(CallError::Unavailable);
                }
                let candidates = self
                    .arr
                    .manual_import_candidates(&binding.external_id)
                    .await
                    .map_err(unavailable)?;
                let candidate = candidates.first().ok_or(CallError::Unavailable)?;
                let exact_target = match self.gate.record.kind {
                    Kind::Sonarr => {
                        candidate
                            .series
                            .as_ref()
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_u64())
                            == route.series_id
                            && route.series_id.is_some()
                            && candidate.episodes.as_ref().is_some_and(|episodes| {
                                episodes.len() == 1
                                    && episodes[0].get("id").and_then(|v| v.as_u64())
                                        == Some(route.media_id)
                            })
                    }
                    Kind::Radarr => {
                        candidate
                            .movie
                            .as_ref()
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_u64())
                            == Some(route.media_id)
                    }
                };
                if candidate.path != binding.path
                    || candidate.download_id.as_deref() != Some(&binding.external_id)
                {
                    return Err(CallError::Unavailable);
                }
                pitcrew_arbiter::replacement(&pitcrew_arbiter::ReplacementProof {
                    original: *original,
                    current: job.job.current_original(),
                    source: *file,
                    decoded_source: decoded(&evidence)?.then_some(evidence.file),
                    owned_download: binding.download,
                    download: *download,
                    exact_target,
                    candidate_count: candidates.len(),
                    rejection_count: candidate.rejections.as_ref().map(Vec::len),
                    filename: Path::new(&candidate.path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or(CallError::Unavailable)?
                        .to_owned(),
                    bytes: candidate.size,
                })
                .map_err(unavailable)?;
                let file = ImportFile {
                    path: candidate.path.clone(),
                    download_id: binding.external_id.clone(),
                    series_id: route
                        .series_id
                        .filter(|_| self.gate.record.kind == Kind::Sonarr),
                    movie_id: (self.gate.record.kind == Kind::Radarr).then_some(route.media_id),
                    episode_ids: if self.gate.record.kind == Kind::Sonarr {
                        vec![route.media_id]
                    } else {
                        vec![]
                    },
                    quality: candidate.quality.clone().ok_or(CallError::Unavailable)?,
                    languages: candidate.languages.clone().ok_or(CallError::Unavailable)?,
                    release_group: candidate.release_group.clone().unwrap_or_default(),
                };
                self.original(&job, &route).await?;
                let current = self.evidence(&binding.path, false).await?;
                if current.file != evidence.file
                    || !source_constraints(&job.spec.constraints, &current)?
                {
                    return Err(CallError::Unavailable);
                }
                self.final_access(intent).await?;
                self.arr
                    .manual_import(&permit, &[file])
                    .await
                    .map_err(unavailable)?;
                crate::observer::record_dispatch(
                    &self.store,
                    intent,
                    &self.gate.record.client,
                    &binding.external_id,
                    (self.clock)(),
                )
                .await
                .map_err(unavailable)?;
                Observation::ImportAccepted
            }
            _ => return Err(CallError::Unavailable),
        };
        Ok(Outcome::Observed(observation))
    }

    async fn final_access(&self, intent: &Intent) -> Result<(), CallError> {
        let (current, _) = self.context(intent).await?;
        // Admin reset or a newly observed version may revoke this capability
        // while the upstream preflight reads are in flight.
        if self
            .store
            .capabilities(&self.gate.record.client, &self.gate.record.version)
            .await
            .map_err(unavailable)?
            .and_then(|ledger| ledger.capabilities)
            .is_none()
        {
            return Err(CallError::Unavailable);
        }
        if !current.job.mutation_authorized((self.clock)()) {
            return Err(CallError::Unavailable);
        }
        self.gate
            .authorize(&self.gate.record.version, (self.clock)())?;
        Ok(())
    }

    async fn execute_read(&self, intent: &Intent) -> Result<Outcome, CallError> {
        if matches!(intent.effect, Effect::CheckTarget { .. }) {
            return self.fallback.read(intent).await;
        }
        let (job, route) = self.context(intent).await?;
        let observation = match intent.effect {
            Effect::DecodeDownload { download, file } => {
                self.queue(&route, download).await?;
                let binding = route.download.as_ref().ok_or(CallError::Unavailable)?;
                let evidence = self.evidence(&binding.path, true).await?;
                if evidence.file != file {
                    return Err(CallError::Unavailable);
                }
                Observation::DownloadDecoded {
                    file,
                    good: decoded(&evidence)?
                        && source_constraints(&job.spec.constraints, &evidence)?,
                }
            }
            Effect::DecodeImported { file } => {
                let id = self
                    .arr
                    .installed_file(route.media_id)
                    .await
                    .map_err(unavailable)?;
                let (fingerprint, evidence) = self.fingerprint(id, true).await?;
                // A fresh read may renew the observation age only when the
                // complete tuple still yields this exact registered revision.
                if self
                    .store
                    .register_file(
                        &self.gate.record.client,
                        &job.spec.resource,
                        &fingerprint,
                        (self.clock)(),
                    )
                    .await
                    .map_err(unavailable)?
                    != file
                {
                    return Err(CallError::Unavailable);
                }
                Observation::ImportedDecoded {
                    file,
                    good: decoded(&evidence)?,
                }
            }
            // A download binding comes only from trusted correlated intake.
            // Missing queue rows never prove absence: an external actor can
            // remove them before the engine sees completion.
            Effect::ReconcileGrab { release } => {
                if route.release.as_ref().map(|r| r.release) != Some(release) {
                    return Err(CallError::Unavailable);
                }
                let binding = route.download.as_ref().ok_or(CallError::Unavailable)?;
                let resolution = match self.queue(&route, binding.download).await {
                    Ok(_) => GrabResolution::Accepted {
                        download: binding.download,
                    },
                    Err(_) => GrabResolution::Unknown,
                };
                Observation::GrabReconciled(resolution)
            }
            Effect::ReconcileDelete { download } => {
                let resolution = match self.queue(&route, download).await {
                    Ok(_) => DeleteResolution::Present,
                    Err(_) => DeleteResolution::Unknown,
                };
                Observation::DeleteReconciled(resolution)
            }
            Effect::ReconcileImport { download } => {
                let binding = route
                    .download
                    .as_ref()
                    .filter(|d| d.download == download)
                    .ok_or(CallError::Unavailable)?;
                let id = self
                    .arr
                    .installed_file(route.media_id)
                    .await
                    .map_err(unavailable)?;
                let file = self.arr.file(id).await.map_err(unavailable)?;
                if file.id != id {
                    return Err(CallError::Unavailable);
                }
                let history = self
                    .arr
                    .history(Page::default())
                    .await
                    .map_err(unavailable)?;
                let found = id != route.original_arr_file
                    && history.records.iter().any(|event| {
                        event.event_type == "downloadFolderImported"
                            && event.download_id.as_deref() == Some(&binding.external_id)
                            && match self.gate.record.kind {
                                Kind::Sonarr => event.episode_id,
                                Kind::Radarr => event.movie_id,
                            } == Some(route.media_id)
                            && file.date_added.as_deref() == Some(&event.date)
                            && event
                                .data
                                .as_ref()
                                .and_then(|data| data.get("importedPath"))
                                .and_then(|v| v.as_str())
                                == file.path.as_deref()
                            && file.path.is_some()
                    });
                let resolution = if found {
                    // Build identity from the same file record matched to
                    // history. A later record must not silently replace that
                    // evidence while the agent or history reads are in flight.
                    let evidence = self
                        .evidence(file.path.as_deref().ok_or(CallError::Unavailable)?, false)
                        .await?;
                    let fingerprint = file_fingerprint(&file, evidence.file)?;
                    let current = self.arr.file(id).await.map_err(unavailable)?;
                    if current.path != file.path
                        || file_fingerprint(&current, evidence.file)? != fingerprint
                        || self
                            .arr
                            .installed_file(route.media_id)
                            .await
                            .map_err(unavailable)?
                            != id
                    {
                        return Err(CallError::Unavailable);
                    }
                    let file = self
                        .store
                        .register_file(
                            &self.gate.record.client,
                            &job.spec.resource,
                            &fingerprint,
                            (self.clock)(),
                        )
                        .await
                        .map_err(unavailable)?;
                    ImportResolution::Imported { download, file }
                } else {
                    ImportResolution::Unknown
                };
                Observation::ImportReconciled(resolution)
            }
            _ => return Err(CallError::Unavailable),
        };
        Ok(Outcome::Observed(observation))
    }
}

impl<A: Transport, G: Transport, F: Upstream> Upstream for Router<A, G, F> {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(self.execute_read(intent))
    }
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(self.execute_write(intent))
    }
}
fn file_fingerprint(
    file: &MediaFile,
    agent: pitcrew_domain::FileIdentity,
) -> Result<IdentityFingerprint, CallError> {
    Ok(IdentityFingerprint {
        arr_file_id: file.id,
        size: file.size.ok_or(CallError::Unavailable)?,
        date_added: file.date_added.clone().ok_or(CallError::Unavailable)?,
        quality: file
            .quality
            .as_ref()
            .ok_or(CallError::Unavailable)?
            .to_string(),
        agent,
    })
}

fn source_constraints(
    constraints: &Constraints,
    evidence: &MediaEvidence,
) -> Result<bool, CallError> {
    if constraints.forbid_dolby_vision_profile5 {
        evidence
            .header
            .as_ref()
            .and_then(|probe| probe.excludes_dolby_vision_profile5())
            .ok_or(CallError::Unavailable)
    } else {
        Ok(true)
    }
}
fn decoded(evidence: &MediaEvidence) -> Result<bool, CallError> {
    let decode = evidence.decode.as_ref().ok_or(CallError::Unavailable)?;
    match decode.outcome {
        ProbeOutcome::Passed => Ok(decode.faults.is_empty() && !decode.tested_ranges.is_empty()),
        ProbeOutcome::Failed => Ok(false),
        _ => Err(CallError::Unavailable),
    }
}
fn key(intent: &Intent) -> String {
    format!("{}:{}", intent.job, intent.sequence)
}
fn unavailable<T>(_: T) -> CallError {
    CallError::Unavailable
}
