//! Fresh trusted release evidence for bounded lower-quality shadow approvals.
use crate::{Authority, Bridge, Error, Result, integer};
use pitcrew_diagnosis::facts::{DecodeOutcome, ProbeOutcome};
use pitcrew_domain::{Access, AdminApproval, FileIdentity, LowerQualityBound, ReleaseCandidate};
use pitcrew_store::{StoreError, StoredJob};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseEvidence {
    pub revision: u64,
    pub arr_instance_id: String,
    pub arr_item_id: u64,
    pub original: FileIdentity,
    pub candidate: ReleaseCandidate,
    pub bound: LowerQualityBound,
    pub observed_at: u64,
}

impl Bridge {
    /// Only a trusted collector may call this after reading an eligible exact
    /// release. Viewer/admin JSON can neither supply nor renew this evidence.
    pub async fn record_lower_quality(&self, job: u64, evidence: &ReleaseEvidence) -> Result<()> {
        sqlx::query("INSERT INTO bridge_lower_quality(job_id,evidence) VALUES (?,?) ON CONFLICT(job_id) DO UPDATE SET evidence=excluded.evidence")
            .bind(integer(job)?).bind(serde_json::to_string(evidence)?).execute(&self.pool).await?;
        Ok(())
    }

    /// A failed or unavailable refresh must revoke the previous release proof.
    pub async fn clear_lower_quality(&self, job: u64) -> Result<()> {
        sqlx::query("DELETE FROM bridge_lower_quality WHERE job_id=?")
            .bind(integer(job)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Start a controller refresh batch. Jobs absent from its newly authorized
    /// account/resource set must not retain a proof from an earlier tick.
    pub async fn clear_lower_quality_proofs(&self) -> Result<()> {
        sqlx::query("DELETE FROM bridge_lower_quality")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn lower_quality_approval(
        &self,
        stored: &StoredJob,
        minimum_resolution: u32,
        now: u64,
    ) -> Result<Option<(AdminApproval, Access)>> {
        let Some((candidate, bound)) = stored.job.bounded_lower_quality() else {
            return Ok(None);
        };
        if bound.minimum_resolution != minimum_resolution {
            return Ok(None);
        }
        let evidence: Option<String> =
            sqlx::query_scalar("SELECT evidence FROM bridge_lower_quality WHERE job_id=?")
                .bind(integer(stored.spec.id)?)
                .fetch_optional(&self.pool)
                .await?;
        let Some(evidence) = evidence else {
            return Ok(None);
        };
        let evidence: ReleaseEvidence = serde_json::from_str(&evidence)?;
        if evidence.revision != stored.revision
            || evidence.original != stored.job.current_original()
            || evidence.candidate != *candidate
            || evidence.bound != bound
            || evidence.observed_at > now
            || stored.job.reserved_budget() >= self.options.policy.max_grabs
        {
            return Ok(None);
        }
        match self
            .engine
            .revalidate_budget(stored.spec.id, self.options.budgets, now)
            .await
        {
            Ok(()) => {}
            Err(StoreError::BudgetExhausted) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let authorities: Vec<String> = sqlx::query_scalar("SELECT a.authority FROM bridge_authority a JOIN report_progress p ON p.report_id=a.report_id JOIN web_intakes i ON i.id=a.report_id WHERE p.job_id=? AND i.account=?")
            .bind(integer(stored.spec.id)?).bind(&stored.spec.viewer).fetch_all(&self.pool).await?;
        for authority in authorities {
            let a: Authority = serde_json::from_str(&authority)?;
            if a.account != stored.spec.viewer
                || a.arr_instance_id != evidence.arr_instance_id
                || a.arr_item_id != evidence.arr_item_id
                || a.resource != stored.spec.resource
                || a.library != stored.spec.library
                || a.observed_at > now
                || now - a.observed_at >= self.options.evidence_max_age
                || evidence.observed_at < a.observed_at
                || !(a.decode == DecodeOutcome::Bad
                    || matches!(a.probe, ProbeOutcome::OpenFailed | ProbeOutcome::NoVideo))
            {
                continue;
            }
            let expires_at = match a.access {
                Access::Granted {
                    library,
                    expires_at,
                } if library == stored.spec.library && expires_at > now => expires_at,
                _ => continue,
            };
            let Some(registered) = self
                .engine
                .registered_file(&a.arr_instance_id, &a.resource)
                .await?
            else {
                continue;
            };
            if registered.identity != stored.job.current_original()
                || registered.fingerprint.arr_file_id != a.arr_file_id
                || registered.fingerprint.agent != a.file
            {
                continue;
            }
            match self
                .engine
                .validate_file(
                    &a.arr_instance_id,
                    &a.resource,
                    registered.identity,
                    &registered.fingerprint,
                    now,
                    self.options.evidence_max_age,
                )
                .await
            {
                Ok(()) => {}
                Err(StoreError::IdentityMismatch) => continue,
                Err(error) => return Err(Error::Engine(error)),
            }
            if self
                .engine
                .release_is_excluded(
                    &a.arr_instance_id,
                    candidate.release,
                    &candidate.identity,
                    now,
                )
                .await?
            {
                continue;
            }
            return Ok(Some((
                AdminApproval {
                    job: stored.spec.id,
                    release: candidate.release,
                    broken_file: stored.job.current_original(),
                    expires_at: expires_at
                        .min(a.observed_at.saturating_add(self.options.evidence_max_age)),
                },
                a.access,
            )));
        }
        Ok(None)
    }
}
