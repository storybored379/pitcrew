//! Narrow observer reads avoid loading unrelated diagnostic/usage payloads on
//! every projection tick. These APIs grant no effect authority.
use crate::{ObservationRecord, Result, Store, observation_row};

impl Store {
    pub async fn action_receipts(&self) -> Result<Vec<ObservationRecord>> {
        let rows = sqlx::query("SELECT * FROM observations WHERE source='engine_dispatch' OR source GLOB 'legacy_arr_action:*' ORDER BY id")
            .fetch_all(&self.pool).await?;
        rows.into_iter().map(observation_row).collect()
    }

    pub async fn comparison_inputs(&self) -> Result<Vec<ObservationRecord>> {
        let rows = sqlx::query("SELECT * FROM observations WHERE source='diff' OR source GLOB 'history:*' OR source GLOB 'webhook:*' ORDER BY id")
            .fetch_all(&self.pool).await?;
        rows.into_iter().map(observation_row).collect()
    }

    pub async fn observation(&self, source: &str, key: &str) -> Result<Option<ObservationRecord>> {
        sqlx::query("SELECT * FROM observations WHERE source=? AND observation_key=?")
            .bind(source)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?
            .map(observation_row)
            .transpose()
    }
}
