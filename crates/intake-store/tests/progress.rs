use pitcrew_intake_store::{Store, progress::ReportProgress};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
#[tokio::test]
async fn progress_absent_then_account_scoped_with_all_fields() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(&path).await.unwrap();
    assert!(store.report_progress("a").await.unwrap().is_empty());
    for a in ["a", "b"] {
        store
            .persist_intake("test", a, a, "thread", &serde_json::json!({}), 1)
            .await
            .unwrap();
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    sqlx::query("CREATE TABLE report_progress(report_id INTEGER PRIMARY KEY,job_id INTEGER,state TEXT,updated_at INTEGER,viewer_copy_key TEXT)").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO report_progress VALUES(1,7,'fixed',99,'report.fixed'),(2,NULL,'received',2,'report.received')").execute(&pool).await.unwrap();
    assert_eq!(
        store.report_progress("a").await.unwrap(),
        vec![ReportProgress {
            report_id: 1,
            job_id: Some(7),
            state: "fixed".into(),
            updated_at: 99,
            viewer_copy_key: "report.fixed".into()
        }]
    );
    assert_eq!(
        store.report_progress("b").await.unwrap(),
        vec![ReportProgress {
            report_id: 2,
            job_id: None,
            state: "received".into(),
            updated_at: 2,
            viewer_copy_key: "report.received".into()
        }]
    );
    assert!(store.report_progress("unknown").await.unwrap().is_empty());
    sqlx::query("INSERT INTO report_progress VALUES(3,99,'fixed',100,'report.fixed')")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store.report_progress("a").await.unwrap().len(), 1);
}
