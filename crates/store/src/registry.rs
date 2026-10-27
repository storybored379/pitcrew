//! Read the current store-normalized identity without altering its provenance.
//! Agent file identity and arr file identity belong to different namespaces.
use crate::{Result, Store, policy::IdentityFingerprint, unsigned};
use pitcrew_domain::FileIdentity;
use sqlx::Row;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredFile {
    /// Arr file ID plus the store-minted registry revision.
    pub identity: FileIdentity,
    pub fingerprint: IdentityFingerprint,
    /// Observation time from the trusted collector, never refreshed by a read.
    pub observed_at: u64,
}

impl Store {
    pub async fn registered_file(
        &self,
        client: &str,
        resource: &str,
    ) -> Result<Option<RegisteredFile>> {
        let row = sqlx::query("SELECT f.revision,f.fingerprint,f.observed_at FROM file_revisions f JOIN current_file_revisions c ON c.revision=f.revision WHERE c.client=? AND c.resource=?")
            .bind(client).bind(resource).fetch_optional(&self.pool).await?;
        row.map(|row| {
            let fingerprint: IdentityFingerprint =
                serde_json::from_str(row.try_get("fingerprint")?)?;
            Ok(RegisteredFile {
                identity: FileIdentity {
                    id: fingerprint.arr_file_id,
                    revision: unsigned(row.try_get("revision")?)?,
                },
                fingerprint,
                observed_at: unsigned(row.try_get("observed_at")?)?,
            })
        })
        .transpose()
    }
}
