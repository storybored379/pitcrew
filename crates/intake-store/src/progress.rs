//! Account-scoped, read-only W2 progress projection for viewer and owner hosts.
//! The bridge owns the schema and all writes. Absence means no bridge has run.
use crate::{Result, Store};
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportProgress {
    pub report_id: i64,
    pub job_id: Option<i64>,
    pub state: String,
    pub updated_at: i64,
    pub viewer_copy_key: String,
}
impl Store {
    pub async fn report_progress(&self, account: &str) -> Result<Vec<ReportProgress>> {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='report_progress')")
            .fetch_one(&self.pool).await?;
        if !exists {
            return Ok(Vec::new());
        }
        Ok(sqlx::query("SELECT p.* FROM report_progress p JOIN web_intakes i ON i.id=p.report_id WHERE i.account=? ORDER BY p.report_id DESC")
            .bind(account).fetch_all(&self.pool).await?.into_iter().map(|row| ReportProgress {
                report_id: row.get("report_id"), job_id: row.get("job_id"), state: row.get("state"),
                updated_at: row.get("updated_at"), viewer_copy_key: row.get("viewer_copy_key")
            }).collect())
    }
}
