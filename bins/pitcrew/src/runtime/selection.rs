//! Conservative shadow proposals. Searches require explicit quota consent;
//! release titles never establish quality, identity or permission.
use super::*;
use pitcrew_domain::{
    Access, Event, LowerQualityBound, QualityRelation, ReleaseCandidate, State,
    media::{ReleaseGuid, ReleaseIdentity},
};
use pitcrew_store::Lease;
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Candidate {
    #[serde(flatten)]
    release: pitcrew_arr::Release,
    quality: Option<serde_json::Value>,
}
fn resolution(quality: &serde_json::Value) -> Option<u32> {
    u32::try_from(quality.get("quality")?.get("resolution")?.as_u64()?)
        .ok()
        .filter(|value| *value > 0)
}

impl<T: Transport> Runtime<T> {
    pub(super) async fn refresh_lower_quality(&self) -> Result<(), &'static str> {
        self.bridge
            .clear_lower_quality_proofs()
            .await
            .map_err(|_| "lower-quality evidence unavailable")?;
        for prepared in self.authorities.values() {
            let Some(job) = self
                .store
                .owned_job(&prepared.target.resource)
                .await
                .map_err(|_| "resource ownership unavailable")?
            else {
                continue;
            };
            let stored = self
                .store
                .load_job(job)
                .await
                .map_err(|_| "selection job unavailable")?;
            if stored.job.state() != State::AwaitingQualityApproval {
                continue;
            }
            self.bridge
                .clear_lower_quality(job)
                .await
                .map_err(|_| "lower-quality evidence unavailable")?;
            let now = self.now_seconds();
            let Ok(lease) = self
                .store
                .acquire_lease(job, "runtime-lower-quality", now, 60)
                .await
            else {
                continue;
            };
            // A failed release refresh leaves no proof for the consumer.
            let _ = self.select(&lease).await;
            self.store
                .release_lease(&lease, self.now_seconds())
                .await
                .map_err(|_| "selection lease release failed")?;
        }
        Ok(())
    }

    pub(super) async fn select(&self, lease: &Lease) -> Result<(), &'static str> {
        let stored = self
            .store
            .load_job(lease.job)
            .await
            .map_err(|_| "selection job unavailable")?;
        let pending = stored.job.state() == State::AwaitingQualityApproval;
        if pending {
            self.bridge
                .clear_lower_quality(lease.job)
                .await
                .map_err(|_| "lower-quality evidence unavailable")?;
        }
        if !self.search_enabled
            || !matches!(
                stored.job.state(),
                State::Picked | State::Rejected | State::AwaitingQualityApproval
            )
        {
            return Ok(());
        }
        let Some(Prepared { target, access }) = self
            .authorities
            .get(&(stored.spec.viewer.clone(), stored.spec.resource.clone()))
        else {
            return Ok(());
        };
        let now = self.now_seconds();
        let registered = self
            .store
            .registered_file(&target.arr_instance_id, &target.resource)
            .await
            .map_err(|_| "selection registry unavailable")?
            .ok_or("selection registry missing")?;
        if registered.identity != stored.job.current_original() {
            return Err("original file changed before selection");
        }
        self.store
            .validate_file(
                &target.arr_instance_id,
                &target.resource,
                registered.identity,
                &registered.fingerprint,
                now,
                self.max_age,
            )
            .await
            .map_err(|_| "selection identity expired")?;
        let arr = self
            .arr
            .get(&target.arr_instance_id)
            .ok_or("selection arr unavailable")?;
        let media_key = match arr.client.kind() {
            pitcrew_arr::Kind::Sonarr => "episodeId",
            pitcrew_arr::Kind::Radarr => "movieId",
        };
        let mut candidates: Vec<Candidate> = arr
            .search
            .get(
                "api/v3/release",
                vec![(media_key.into(), target.arr_item_id.to_string())],
            )
            .await
            .map_err(|_| "release search unavailable")?;
        if candidates.len() > 1000 {
            return Err("release search exceeds row bound");
        }
        // Preserve same-quality preference even if arr returns a downgrade first.
        candidates.sort_by_key(|candidate| {
            candidate
                .quality
                .as_ref()
                .map(serde_json::Value::to_string)
                .as_deref()
                != Some(&registered.fingerprint.quality)
        });
        for candidate in candidates {
            let same_quality = candidate
                .quality
                .as_ref()
                .map(serde_json::Value::to_string)
                .as_deref()
                == Some(&registered.fingerprint.quality);
            let bound = self.lower_quality_minimum.and_then(|minimum_resolution| {
                let original: serde_json::Value =
                    serde_json::from_str(&registered.fingerprint.quality).ok()?;
                let bound = LowerQualityBound {
                    minimum_resolution,
                    candidate_resolution: resolution(candidate.quality.as_ref()?)?,
                    original_resolution: resolution(&original)?,
                };
                bound.valid().then_some(bound)
            });
            if !same_quality && bound.is_none() {
                continue;
            }
            let release = candidate.release;
            if release.guid.trim().is_empty()
                || release.indexer_id == 0
                || release.approved != Some(true)
                || release.rejected != Some(false)
                || release.download_allowed != Some(true)
                || !release.rejections.as_ref().is_some_and(Vec::is_empty)
                || !release.size.is_some_and(|size| size >= 70 * 1024 * 1024)
            {
                continue;
            }
            let source = format!("{}:{}", target.arr_instance_id, release.indexer_id);
            let digest = Sha256::digest(
                serde_json::to_vec(&(&source, &release.guid))
                    .map_err(|_| "release identity invalid")?,
            );
            let id = (u64::from_be_bytes(digest[..8].try_into().expect("digest width"))
                & i64::MAX as u64)
                .max(1);
            let identity = ReleaseIdentity {
                guid: Some(ReleaseGuid {
                    source,
                    value: release.guid,
                }),
                ..ReleaseIdentity::default()
            };
            if self
                .store
                .release_is_excluded(&target.arr_instance_id, id, &identity, now)
                .await
                .map_err(|_| "release exclusions unavailable")?
                || stored
                    .job
                    .excluded_releases()
                    .iter()
                    .any(|r| r.release == id || r.identity.overlaps(&identity))
            {
                continue;
            }
            let candidate = ReleaseCandidate {
                release: id,
                identity,
                quality: if same_quality {
                    QualityRelation::SameOrBetter
                } else {
                    QualityRelation::Lower
                },
            };
            if pending
                && stored.job.bounded_lower_quality() != bound.map(|bound| (&candidate, bound))
            {
                continue;
            }
            let finished = self.now_seconds();
            if !matches!(access, Access::Granted { library, expires_at } if *library == stored.spec.library && *expires_at > finished)
                || self
                    .fingerprint(target, Some(registered.fingerprint.agent))
                    .await?
                    != registered.fingerprint
            {
                return Err("access or installed identity changed during selection");
            }
            self.store
                .validate_file(
                    &target.arr_instance_id,
                    &target.resource,
                    registered.identity,
                    &registered.fingerprint,
                    self.now_seconds(),
                    self.max_age,
                )
                .await
                .map_err(|_| "selection registry changed or expired during search")?;
            if let Some(bound) = bound.filter(|_| !same_quality) {
                let revision = if pending {
                    stored.revision
                } else {
                    self.store
                        .apply_event(
                            lease,
                            stored.revision,
                            &format!("runtime:lower-quality:{id}"),
                            Event::RequestBoundedLowerQuality {
                                candidate: candidate.clone(),
                                bound,
                                access: *access,
                            },
                            self.now_seconds(),
                        )
                        .await
                        .map_err(|_| "lower-quality proposal rejected")?
                        .stored
                        .revision
                };
                self.bridge
                    .record_lower_quality(
                        lease.job,
                        &pitcrew_bridge::lower_quality::ReleaseEvidence {
                            revision,
                            arr_instance_id: target.arr_instance_id.clone(),
                            arr_item_id: target.arr_item_id,
                            original: registered.identity,
                            candidate,
                            bound,
                            observed_at: self.now_seconds(),
                        },
                    )
                    .await
                    .map_err(|_| "lower-quality evidence unavailable")?;
            } else if !pending {
                self.store
                    .apply_event(
                        lease,
                        stored.revision,
                        &format!("runtime:select:{id}"),
                        Event::SelectCandidate {
                            candidate,
                            access: *access,
                        },
                        self.now_seconds(),
                    )
                    .await
                    .map_err(|_| "selection rejected")?;
            }
            return Ok(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod quality_metadata_tests {
    use super::resolution;
    use serde_json::json;

    #[test]
    fn structured_resolution_never_infers_from_names_ids_or_invalid_numbers() {
        for value in [
            json!(null),
            json!({"quality":{"id":1080,"name":"WEB-1080p"}}),
            json!({"resolution":1080}),
            json!({"quality":{"resolution":0}}),
            json!({"quality":{"resolution":-1}}),
            json!({"quality":{"resolution":1080.5}}),
            json!({"quality":{"resolution":"1080"}}),
            json!({"quality":{"resolution":u64::from(u32::MAX) + 1}}),
        ] {
            assert_eq!(resolution(&value), None, "{value}");
        }
        for value in [1, 720, 1080, 2160, u32::MAX] {
            assert_eq!(
                resolution(&json!({"quality":{"resolution":value}})),
                Some(value)
            );
        }
    }
}
