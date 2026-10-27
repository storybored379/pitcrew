use super::*;
use pitcrew_diagnosis::{
    collectors::{AgentEvidence, Observation},
    facts::EvidenceFacts,
};
use pitcrew_intake::{
    IntakeOutcome, IntakeResponse,
    pipeline::{
        AccessProvider, CatalogResolver, EvidenceFuture, EvidenceSource, Pipeline, ResolvedTarget,
    },
};
use pitcrew_store::policy::IdentityFingerprint;
use pitcrew_sync::AccessSnapshot;
use std::sync::Mutex;

struct Recorded<'a, T> {
    source: &'a Collectors<Shared<T>>,
    agent: Mutex<Option<AgentEvidence>>,
}
impl<T: Transport> EvidenceSource for Recorded<'_, T> {
    fn collect<'a>(
        &'a self,
        target: &'a ResolvedTarget,
        account: &'a str,
        clock: &'a Clock,
    ) -> EvidenceFuture<'a> {
        Box::pin(async move {
            let evidence = self.source.collect(target, account, clock).await;
            *self.agent.lock().expect("evidence lock") = Some(evidence.agent.clone());
            evidence
        })
    }
}
impl<T: Transport> Runtime<T> {
    async fn access_body(&self, url: url::Url, accept: &str) -> Result<Vec<u8>, &'static str> {
        let request = Request {
            method: pitcrew_upstream::Method::Get,
            url,
            headers: vec![
                ("X-Plex-Token".into(), self.token.clone()),
                ("Accept".into(), accept.into()),
            ],
            body: None,
            limits: Limits {
                timeout: Duration::from_secs(10),
                max_response_bytes: 1024 * 1024,
            },
        };
        let response = tokio::time::timeout(Duration::from_secs(10), self.transport.send(request))
            .await
            .map_err(|_| "access refresh timed out")?
            .map_err(|_| "access refresh unavailable")?;
        if !(200..300).contains(&response.status) || response.body.len() > 1024 * 1024 {
            return Err("access refresh rejected");
        }
        Ok(response.body)
    }

    pub(super) async fn reports(&mut self) -> Result<Tick, &'static str> {
        self.authorities.clear();
        let reports = self
            .intake
            .runtime_reports(self.report_cursor, self.batch)
            .await
            .map_err(|_| "reports unavailable")?;
        if reports.is_empty() {
            self.report_cursor = 0;
            return Ok(Tick::default());
        }
        // First revoke cached authority for this batch, even if refresh fails.
        for report in &reports {
            self.bridge
                .clear_authority(report.intake.id)
                .await
                .map_err(|_| "authority invalidation failed")?;
        }
        let at = self.now_seconds();
        let xml = self
            .access_body(self.access_url.clone(), "application/xml")
            .await;
        let owner = self
            .access_body(self.owner_url.clone(), "application/json")
            .await
            .ok()
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
            .and_then(|value| {
                value
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .filter(|id| *id > 0)
                    .map(|id| id.to_string())
            });
        // Token identity is not a server-library grant. Even the configured
        // owner's account needs a current server-local section observation.
        let owner_libraries = if reports
            .iter()
            .any(|r| owner.as_deref() == Some(&r.account_id))
        {
            self.plex
                .sections()
                .await
                .ok()
                .map(|sections| sections.into_iter().map(|s| s.key).collect::<Vec<_>>())
        } else {
            None
        };
        let mapping: Vec<_> = self
            .bindings
            .iter()
            .map(|b| (b.library_id.clone(), b.library_id.clone()))
            .collect();
        let mut summary = Tick::default();
        for report in reports {
            self.report_cursor = report.intake.id;
            summary.reports_examined += 1;
            let response: IntakeResponse =
                match serde_json::from_value(report.intake.response.clone()) {
                    Ok(r) => r,
                    Err(_) => {
                        summary.reports_held += 1;
                        continue;
                    }
                };
            if response.outcome != IntakeOutcome::Report {
                continue;
            }
            let access = if owner.as_deref() == Some(&report.account_id) {
                owner_libraries.as_ref().map(|libraries| {
                    AccessSnapshot::from_grants(
                        report.account_id.clone(),
                        libraries.clone(),
                        at,
                        at.saturating_add(self.max_age),
                    )
                })
            } else {
                xml.as_ref().ok().and_then(|xml| {
                    AccessSnapshot::from_shared_servers_xml(
                        xml,
                        &report.account_id,
                        &mapping,
                        at,
                        at.saturating_add(self.max_age),
                    )
                    .ok()
                })
            };
            let result = match access {
                Some(access) => self.diagnose(&report, &response, &access).await,
                None => Err("access evidence unavailable"),
            };
            match result {
                Ok((target, access)) => {
                    self.authorities.insert(
                        (report.account_id.clone(), target.resource.clone()),
                        Prepared { target, access },
                    );
                    summary.authority_recorded += 1;
                }
                Err(reason) => {
                    summary.reports_held += 1;
                    let now = self.now_seconds();
                    self.bridge
                        .hold_report(report.intake.id, now)
                        .await
                        .map_err(|_| "held progress unavailable")?;
                    self.store.ingest_observation("admin:health", &format!("report:{}:{now}", report.intake.id),
                        &serde_json::json!({"component":"report","report_id":report.intake.id,"available":false,"reason":reason}), now)
                        .await.map_err(|_| "held reason unavailable")?;
                }
            }
            // Accounting/comparison recovery reads only committed, redacted data.
            self.telemetry(report.intake.id).await?;
        }
        Ok(summary)
    }

    async fn diagnose(
        &self,
        report: &pitcrew_intake_store::diagnoses::PendingReport,
        response: &IntakeResponse,
        access: &AccessSnapshot,
    ) -> Result<(ResolvedTarget, pitcrew_domain::Access), &'static str> {
        let [issue] = response.issues.as_slice() else {
            return Err("one exact catalog item required");
        };
        let now = self.now_seconds();
        let target = self.catalog.resolve(&issue.item_id, now).map_err(|_| "No unique current file mapping. Multipart, multiversion and shared episode files require review.")?;
        let grant = access
            .access(&report.account_id, now)
            .filter(|g| g.permits(&report.account_id, &target.item.library_id, now))
            .ok_or("current access required")?;
        let before = self.fingerprint(&target, None).await?;
        let source = Recorded {
            source: &self.collectors,
            agent: Mutex::new(None),
        };
        let pipeline = Pipeline::new(
            &self.intake,
            &self.catalog,
            access,
            &source,
            self.clock.as_ref(),
        );
        pipeline
            .diagnose(report.intake.id)
            .await
            .map_err(|_| "diagnosis unavailable")?;
        let collected = source.agent.lock().expect("evidence lock").take();
        let evidence = match collected {
            Some(agent) => agent,
            None => {
                self.collectors
                    .collect(&target, &report.account_id, self.clock.as_ref())
                    .await
                    .agent
            }
        };
        let Observation::Available(media) = &evidence.media else {
            return Err("agent evidence unavailable");
        };
        let finished = (self.clock)();
        if media.file.id == 0
            || media.observed_at_unix_ms > finished
            || finished - media.observed_at_unix_ms > self.max_age.saturating_mul(1000)
            || !grant.permits(&report.account_id, &target.item.library_id, finished / 1000)
            || self
                .catalog
                .resolve(&issue.item_id, finished / 1000)
                .ok()
                .as_ref()
                != Some(&target)
        {
            return Err("evidence expired or changed during diagnosis");
        }
        let after = self.fingerprint(&target, None).await?;
        if before != after {
            return Err("installed file changed during diagnosis");
        }
        let diagnosis = self
            .intake
            .diagnosis(report.intake.id)
            .await
            .map_err(|_| "diagnosis unavailable")?
            .ok_or("diagnosis not persisted")?;
        if diagnosis.agent_file != Some(media.file) {
            return Err("diagnosis bytes no longer current");
        }
        let mut facts = EvidenceFacts::default();
        evidence.apply_to(&mut facts);
        // The registry observation time is the oldest fact in this read sequence.
        let observed = now.min(media.observed_at_unix_ms / 1000);
        let fingerprint = IdentityFingerprint {
            agent: media.file,
            ..after
        };
        self.store
            .register_file(
                &target.arr_instance_id,
                &target.resource,
                &fingerprint,
                observed,
            )
            .await
            .map_err(|_| "file registry unavailable")?;
        let library = target
            .item
            .library_id
            .parse()
            .map_err(|_| "library ID invalid")?;
        // Finish all awaited reads before the final authority freshness check.
        let route = self
            .intake
            .report_route(report.intake.id, &report.account_id, self.now_seconds())
            .await
            .map_err(|_| "report route unavailable")?
            .map(|route| pitcrew_bridge::ViewerRoute {
                recipient: route.recipient,
                conversation: route.conversation,
                last_inbound: Some(route.last_inbound),
            });
        let committed_at = (self.clock)();
        if !grant.permits(
            &report.account_id,
            &target.item.library_id,
            committed_at / 1000,
        ) || self
            .catalog
            .resolve(&issue.item_id, committed_at / 1000)
            .ok()
            .as_ref()
            != Some(&target)
            || media.observed_at_unix_ms > committed_at
            || committed_at - media.observed_at_unix_ms > self.max_age.saturating_mul(1000)
        {
            return Err("evidence expired before authority commit");
        }
        self.bridge
            .record_authority(
                report.intake.id,
                &pitcrew_bridge::Authority {
                    account: report.account_id.clone(),
                    resource: target.resource.clone(),
                    library,
                    arr_instance_id: target.arr_instance_id.clone(),
                    arr_item_id: target.arr_item_id,
                    arr_file_id: target.arr_file_id,
                    file: media.file,
                    observed_at: observed,
                    access: pitcrew_domain::Access::Granted {
                        library,
                        expires_at: grant.expires_at.min(target.expires_at),
                    },
                    probe: facts.playback.probe,
                    decode: facts.playback.decode,
                    route,
                },
            )
            .await
            .map_err(|_| "authority persistence failed")?;
        let expires_at = grant.expires_at.min(target.expires_at);
        Ok((
            target,
            pitcrew_domain::Access::Granted {
                library,
                expires_at,
            },
        ))
    }

    pub(super) async fn fingerprint(
        &self,
        target: &ResolvedTarget,
        agent: Option<pitcrew_domain::FileIdentity>,
    ) -> Result<IdentityFingerprint, &'static str> {
        let client = &self
            .arr
            .get(&target.arr_instance_id)
            .ok_or("arr client unavailable")?
            .client;
        let current = client
            .installed_file(target.arr_item_id)
            .await
            .map_err(|_| "current arr file unavailable")?;
        if current != target.arr_file_id {
            return Err("installed arr identity changed");
        }
        let file = client
            .file(current)
            .await
            .map_err(|_| "arr file metadata unavailable")?;
        let binding = self
            .bindings
            .iter()
            .find(|b| {
                b.library_id == target.item.library_id
                    && b.arr_instance_id == target.arr_instance_id
            })
            .ok_or("library binding unavailable")?;
        let path = std::path::Path::new(file.path.as_deref().ok_or("arr file path unavailable")?);
        let relative = path
            .strip_prefix(&binding.arr_root)
            .map_err(|_| "arr file outside mapped root")?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            || std::path::Path::new(&binding.agent_root).join(relative)
                != std::path::Path::new(&target.agent_path)
            || file.id != current
            || (client.kind() == pitcrew_arr::Kind::Radarr
                && file.movie_id != Some(target.arr_item_id))
        {
            return Err("arr file mapping changed");
        }
        Ok(IdentityFingerprint {
            arr_file_id: current,
            size: file
                .size
                .filter(|n| *n > 0)
                .ok_or("arr file size unavailable")?,
            date_added: file
                .date_added
                .filter(|s| !s.trim().is_empty())
                .ok_or("arr file date unavailable")?,
            quality: file
                .quality
                .filter(|v| {
                    v.get("quality")
                        .and_then(|q| q.get("id"))
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|id| id > 0)
                })
                .ok_or("arr quality unavailable")?
                .to_string(),
            agent: agent.unwrap_or(pitcrew_domain::FileIdentity { id: 0, revision: 0 }),
        })
    }

    async fn telemetry(&self, report: i64) -> Result<(), &'static str> {
        let Some(comparison) = self
            .intake
            .diagnosis_comparison(report)
            .await
            .map_err(|_| "comparison unavailable")?
        else {
            return Ok(());
        };
        let now = self.now_seconds();
        self.store
            .ingest_observation("decider:comparison", &report.to_string(), &comparison, now)
            .await
            .map_err(|_| "comparison projection unavailable")?;
        if let Some(entries) = comparison.get("entries").and_then(|v| v.as_array()) {
            for (index, entry) in entries.iter().enumerate() {
                self.store.ingest_observation("decider:usage", &format!("{report}:{index}"),
                    &serde_json::json!({"provider":entry["provider"], "usage":entry["result"].get("usage"), "failed":entry["failed"]}), now)
                    .await.map_err(|_| "usage projection unavailable")?;
            }
        }
        Ok(())
    }
}
