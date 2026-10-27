//! Bounded recovery must eventually visit committed jobs even when older
//! requests never reached the engine and no current diagnosis can revisit them.
use pitcrew_bridge::{Bridge, Options, ProgressState};
use pitcrew_domain::{Constraints, FileIdentity, Policy, StageTimeouts};
use pitcrew_store::{BudgetLimits, NewJob, Store};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

fn options() -> Options {
    Options {
        policy: Policy {
            max_grabs: 1,
            stage_timeout: 100,
            total_timeout: 1000,
        },
        timeouts: StageTimeouts {
            download: 100,
            import: 100,
            decode: 100,
            verify_retry: 180,
        },
        budgets: BudgetLimits {
            global_hour: 8,
            global_day: 8,
            viewer_hour: 4,
            viewer_day: 4,
        },
        cooldown_seconds: 3600,
        evidence_max_age: 300,
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    bridge: Bridge,
    engine: Store,
    pool: SqlitePool,
}
impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("intake.sqlite");
        let engine = Store::open(temp.path().join("engine.sqlite"))
            .await
            .unwrap();
        let bridge = Bridge::open(&path, engine.clone(), options())
            .await
            .unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .foreign_keys(true),
            )
            .await
            .unwrap();
        Self {
            _temp: temp,
            bridge,
            engine,
            pool,
        }
    }
    async fn reports(&self, count: i64) {
        sqlx::query("WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<?) INSERT INTO web_intakes(id,channel,message,account,conversation,response,created) SELECT id,'test','message-'||id,'account-a','thread-a','{\"outcome\":\"report\"}',10 FROM ids")
            .bind(count).execute(&self.pool).await.unwrap();
    }
}

#[tokio::test]
async fn pending_recovery_rotates_past_uncommitted_requests_without_diagnoses() {
    let f = Fixture::new().await;
    f.reports(129).await;
    let mut tx = f.pool.begin().await.unwrap();
    let mut committed = None;
    for id in 1..=129_u64 {
        let spec = NewJob {
            id,
            viewer: "account-a".into(),
            resource: format!("resource-{id}"),
            library: 1,
            original: FileIdentity { id: 2, revision: 3 },
            constraints: Constraints::default(),
            policy: options().policy,
            timeouts: options().timeouts,
            created_at: 10,
        };
        sqlx::query("INSERT INTO bridge_requests(key,spec,limits,cooldown) VALUES (?,?,?,3600)")
            .bind(format!("frozen-{id}"))
            .bind(serde_json::to_string(&spec).unwrap())
            .bind("[8,8,4,4]")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bridge_reports(report_id,request_key) VALUES (?,?)")
            .bind(id as i64)
            .bind(format!("frozen-{id}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        if id == 129 {
            committed = Some(spec);
        }
    }
    // Some old requests were held; others subsequently became advice-only.
    // Neither group has a diagnosis that can advance its recovery timestamp.
    sqlx::query("INSERT INTO report_progress(report_id,state,updated_at,viewer_copy_key) SELECT id,CASE WHEN id%2=0 THEN 'advice' ELSE 'needs_review' END,10,CASE WHEN id%2=0 THEN 'report.advice' ELSE 'report.needs_review' END FROM web_intakes WHERE id<=128")
        .execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO report_progress(report_id,state,updated_at,viewer_copy_key) VALUES (129,'received',11,'report.received')")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    f.engine
        .create_job(&committed.unwrap(), "frozen-129", options().budgets)
        .await
        .unwrap();

    // Authority is absent/expired. Recovery observes an existing reservation;
    // it must never create jobs for any of the 128 old commands.
    f.bridge.tick(2000).await.unwrap();
    assert!(
        f.bridge
            .progress(129, "account-a")
            .await
            .unwrap()
            .unwrap()
            .job_id
            .is_none()
    );
    f.bridge.tick(2001).await.unwrap();
    assert_eq!(
        f.bridge
            .progress(129, "account-a")
            .await
            .unwrap()
            .unwrap()
            .job_id,
        Some(129)
    );
    assert_eq!(f.engine.jobs().await.unwrap(), vec![129]);
    for (id, expected) in [(1, ProgressState::NeedsReview), (2, ProgressState::Advice)] {
        assert_eq!(
            f.bridge
                .progress(id, "account-a")
                .await
                .unwrap()
                .unwrap()
                .state,
            expected
        );
    }
}

#[tokio::test]
async fn received_initialization_is_bounded_and_eventually_covers_all_reports() {
    let f = Fixture::new().await;
    f.reports(257).await;
    for (now, expected) in [(20, 128_i64), (21, 256), (22, 257)] {
        f.bridge.tick(now).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM report_progress")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(
            count, expected,
            "one tick must initialize at most 128 new reports"
        );
    }
    assert_eq!(
        f.bridge
            .progress(257, "account-a")
            .await
            .unwrap()
            .unwrap()
            .state,
        ProgressState::Received
    );
    assert!(f.engine.jobs().await.unwrap().is_empty());
}
