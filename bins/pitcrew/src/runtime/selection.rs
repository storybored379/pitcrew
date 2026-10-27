//! Conservative shadow proposals. Searches require explicit quota consent;
//! release titles never establish quality, identity or permission.
use super::*;
use pitcrew_domain::{
    Access, Event, QualityRelation, ReleaseCandidate, State,
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
impl<T: Transport> Runtime<T> {
    pub(super) async fn select(&self, lease: &Lease) -> Result<(), &'static str> {
        if !self.search_enabled {
            return Ok(());
        }
        let stored = self
            .store
            .load_job(lease.job)
            .await
            .map_err(|_| "selection job unavailable")?;
        if !matches!(stored.job.state(), State::Picked | State::Rejected) {
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
        let candidates: Vec<Candidate> = arr
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
        for candidate in candidates {
            let release = candidate.release;
            if release.guid.trim().is_empty()
                || release.indexer_id == 0
                || release.approved != Some(true)
                || release.rejected != Some(false)
                || release.download_allowed != Some(true)
                || !release.rejections.as_ref().is_some_and(Vec::is_empty)
                || !release.size.is_some_and(|size| size >= 70 * 1024 * 1024)
                || candidate
                    .quality
                    .as_ref()
                    .map(serde_json::Value::to_string)
                    .as_deref()
                    != Some(&registered.fingerprint.quality)
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
            self.store
                .apply_event(
                    lease,
                    stored.revision,
                    &format!("runtime:select:{id}"),
                    Event::SelectCandidate {
                        candidate: ReleaseCandidate {
                            release: id,
                            identity,
                            quality: QualityRelation::SameOrBetter,
                        },
                        access: *access,
                    },
                    self.now_seconds(),
                )
                .await
                .map_err(|_| "selection rejected")?;
            return Ok(());
        }
        Ok(())
    }
}
