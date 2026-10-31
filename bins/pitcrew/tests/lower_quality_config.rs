use pitcrew_bridge::{Authority, Bridge, Diagnosis, Options, lower_quality::ReleaseEvidence};
use pitcrew_config::Config;
use pitcrew_diagnosis::{
    facts::{DecodeOutcome, EvidenceFacts},
    rules::classify,
};
use pitcrew_domain::{
    Access, Event, FileIdentity, LowerQualityBound, Policy, QualityRelation, ReleaseCandidate,
    StageTimeouts,
};
use pitcrew_store::{BudgetLimits, Store, policy::IdentityFingerprint};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::test]
async fn configured_consumer_rechecks_floor_opt_in_and_runtime_before_cached_approval() {
    for case in [
        "valid",
        "floor_removed",
        "floor_changed",
        "search_off",
        "runtime_off",
        "missing_plex",
        "no_arr",
        "live",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let engine_path = dir.path().join("engine.sqlite");
        let intake_path = dir.path().join("intake.sqlite");
        let admin_path = dir.path().join("admin.sqlite");
        let store = Store::open(&engine_path).await.unwrap();
        let intake = pitcrew_intake_store::Store::open(&intake_path)
            .await
            .unwrap();
        let bridge = Bridge::open(
            &intake_path,
            store.clone(),
            Options {
                policy: Policy {
                    max_grabs: 2,
                    stage_timeout: 100,
                    total_timeout: 1000,
                },
                timeouts: StageTimeouts {
                    download: 100,
                    import: 100,
                    decode: 100,
                    verify_retry: 100,
                },
                budgets: BudgetLimits {
                    global_hour: 8,
                    global_day: 8,
                    viewer_hour: 4,
                    viewer_day: 4,
                },
                cooldown_seconds: 3600,
                evidence_max_age: 300,
            },
        )
        .await
        .unwrap();
        intake
            .persist_intake(
                "test",
                "report",
                "viewer",
                "thread",
                &serde_json::json!({"outcome":"report"}),
                10,
            )
            .await
            .unwrap();
        let report = intake
            .stored_intake("test", "report", "viewer")
            .await
            .unwrap()
            .unwrap()
            .id;
        let mut facts = EvidenceFacts::default();
        facts.playback.decode = DecodeOutcome::Bad;
        let d = Diagnosis {
            report_id: report,
            decided_at: 10,
            decision: classify(&facts),
            evidence_summary: facts,
            resource: "synthetic-resource".into(),
            library_id: 1,
            arr_instance_id: "films".into(),
            arr_item_id: 2,
            arr_file_id: 3,
            agent_file: FileIdentity { id: 4, revision: 5 },
        };
        store
            .register_file(
                "films",
                &d.resource,
                &IdentityFingerprint {
                    arr_file_id: 3,
                    size: 1000,
                    date_added: "2026-01-01T00:00:00Z".into(),
                    quality: "{\"quality\":{\"resolution\":2160}}".into(),
                    agent: d.agent_file,
                },
                10,
            )
            .await
            .unwrap();
        let access = Access::Granted {
            library: 1,
            expires_at: 1000,
        };
        bridge
            .record_authority(
                report,
                &Authority {
                    account: "viewer".into(),
                    resource: d.resource.clone(),
                    library: 1,
                    arr_instance_id: "films".into(),
                    arr_item_id: 2,
                    arr_file_id: 3,
                    file: d.agent_file,
                    observed_at: 10,
                    access,
                    probe: d.evidence_summary.playback.probe,
                    decode: DecodeOutcome::Bad,
                    route: None,
                },
            )
            .await
            .unwrap();
        let job = bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
        let before = store.load_job(job).await.unwrap();
        let candidate = ReleaseCandidate {
            release: 7,
            identity: Default::default(),
            quality: QualityRelation::Lower,
        };
        let bound = LowerQualityBound {
            minimum_resolution: 720,
            candidate_resolution: 1080,
            original_resolution: 2160,
        };
        let lease = store.acquire_lease(job, "test", 11, 30).await.unwrap();
        let pending = store
            .apply_event(
                &lease,
                before.revision,
                "lower",
                Event::RequestBoundedLowerQuality {
                    candidate: candidate.clone(),
                    bound,
                    access,
                },
                11,
            )
            .await
            .unwrap()
            .stored;
        store.release_lease(&lease, 11).await.unwrap();
        bridge
            .record_lower_quality(
                job,
                &ReleaseEvidence {
                    revision: pending.revision,
                    arr_instance_id: "films".into(),
                    arr_item_id: 2,
                    original: pending.job.current_original(),
                    candidate,
                    bound,
                    observed_at: 11,
                },
            )
            .await
            .unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&admin_path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../pitcrew-admin/src/schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO admin_requests VALUES ('lower','owner',?,'Reviewed the bounded proposal','pending_controller',12,12)")
            .bind(serde_json::json!({"action":"lower_quality","job":job,"revision":pending.revision}).to_string()).execute(&pool).await.unwrap();
        let mut config = Config::parse(&format!(
            r#"version=1
database={engine_path:?}
[stack]
enabled=true
intake_database={intake_path:?}
[runtime]
search_enabled=true
[lower_quality]
minimum_resolution=720
[plex]
url="https://plex.invalid/"
owner_token={{env="SYNTHETIC_UNUSED_SECRET"}}
server_id="synthetic"
[[arr]]
id="films"
kind="radarr"
url="https://arr.invalid/"
api_key={{env="SYNTHETIC_UNUSED_SECRET"}}
libraries=["1"]
[admin]
database={admin_path:?}
engine_database={engine_path:?}
intake_database={intake_path:?}
bind="127.0.0.1:8792"
public_origin="http://localhost:8792"
plex_auth_url="https://auth.invalid/"
client_id="synthetic"
admin_account_ids=["owner"]
"#
        ))
        .unwrap();
        match case {
            "floor_removed" => config.lower_quality.minimum_resolution = None,
            "floor_changed" => {
                config.lower_quality.minimum_resolution = std::num::NonZeroU32::new(1080)
            }
            "search_off" => config.runtime.search_enabled = false,
            "runtime_off" => config.stack.enabled = false,
            "missing_plex" => config.plex = None,
            "no_arr" => config.arr.clear(),
            "live" => config.engine.mode = pitcrew_config::EngineMode::Live,
            _ => {}
        }
        let consumer = pitcrew::stack::admin_consumer(&config, Some(&bridge))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(consumer.tick(12).await.unwrap(), 1);
        let state: String =
            sqlx::query_scalar("SELECT state FROM admin_requests WHERE request_key='lower'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            state,
            if case == "valid" {
                "controller_applied"
            } else {
                "controller_deferred"
            },
            "{case}"
        );
        if case != "valid" {
            let audit: String =
                sqlx::query_scalar("SELECT event FROM admin_audit ORDER BY id DESC LIMIT 1")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(
                audit.contains(if case == "live" {
                    "certified upgrade-guard"
                } else if case == "floor_changed" {
                    "bounded release"
                } else {
                    "shadow approval is disabled"
                }),
                "{case}: {audit}"
            );
            assert_eq!(store.load_job(job).await.unwrap(), pending, "{case}");
            assert!(store.outbox(job).await.unwrap().is_empty(), "{case}");
        }
    }
}
