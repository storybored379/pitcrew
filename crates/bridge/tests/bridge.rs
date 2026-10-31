use pitcrew_bridge::*;
use pitcrew_diagnosis::{facts::*, rules::*};
use pitcrew_domain::{Access, Event, FileIdentity, Policy, StageTimeouts};
use pitcrew_store::{BudgetLimits, Store, policy::IdentityFingerprint};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

fn options() -> Options {
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
fn diagnosis(report_id: i64) -> Diagnosis {
    let mut facts = EvidenceFacts::default();
    facts.playback.decode = DecodeOutcome::Bad;
    Diagnosis {
        report_id,
        decided_at: 10,
        decision: classify(&facts),
        evidence_summary: facts,
        resource: "library-a:episode-a".into(),
        library_id: 1,
        arr_instance_id: "series-a".into(),
        arr_item_id: 2,
        arr_file_id: 3,
        agent_file: FileIdentity { id: 4, revision: 5 },
    }
}
fn authority(d: &Diagnosis, account: &str) -> Authority {
    Authority {
        account: account.into(),
        resource: d.resource.clone(),
        library: d.library_id,
        arr_instance_id: d.arr_instance_id.clone(),
        arr_item_id: d.arr_item_id,
        arr_file_id: d.arr_file_id,
        file: d.agent_file,
        observed_at: 10,
        access: Access::Granted {
            library: 1,
            expires_at: 1000,
        },
        probe: d.evidence_summary.playback.probe,
        decode: d.evidence_summary.playback.decode,
        route: None,
    }
}
fn fingerprint(d: &Diagnosis) -> IdentityFingerprint {
    IdentityFingerprint {
        arr_file_id: d.arr_file_id,
        size: 1024,
        date_added: "2026-01-01T00:00:00Z".into(),
        quality: "{\"quality\":{\"id\":1}}".into(),
        agent: d.agent_file,
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    bridge: Bridge,
    engine: Store,
    intake: pitcrew_intake_store::Store,
    path: std::path::PathBuf,
}
impl Fixture {
    async fn save_diagnosis(&self, d: &Diagnosis) {
        sqlx::query("INSERT INTO diagnoses(report_id,decided_at,decider_kind,decision,evidence_summary,resource,library_id,arr_instance_id,arr_item_id,arr_file_id,agent_file,agent_observed_at,access_observed_at,access_expires_at) VALUES(?,?,'rules',?,?,?,?,?,?,?,?,10,10,1000)")
            .bind(d.report_id).bind(d.decided_at as i64).bind(serde_json::to_string(&d.decision).unwrap())
            .bind(serde_json::to_string(&d.evidence_summary).unwrap()).bind(&d.resource).bind(d.library_id.to_string())
            .bind(&d.arr_instance_id).bind(d.arr_item_id as i64).bind(d.arr_file_id as i64)
            .bind(serde_json::to_string(&d.agent_file).unwrap()).execute(&self.sql().await).await.unwrap();
    }
    async fn consumer(&self) -> (admin::Consumer, sqlx::SqlitePool) {
        let path = self._temp.path().join("admin.sqlite");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../../../bins/pitcrew-admin/src/schema.sql"))
            .execute(&pool)
            .await
            .unwrap();
        (
            admin::Consumer::open(&path, self.bridge.clone(), vec!["owner-a".into()])
                .await
                .unwrap(),
            pool,
        )
    }
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("intake.sqlite");
        let intake = pitcrew_intake_store::Store::open(&path).await.unwrap();
        let engine = Store::open(temp.path().join("engine.sqlite"))
            .await
            .unwrap();
        let bridge = Bridge::open(&path, engine.clone(), options())
            .await
            .unwrap();
        Self {
            _temp: temp,
            bridge,
            engine,
            intake,
            path,
        }
    }
    async fn report(&self, account: &str, message: &str) -> Diagnosis {
        self.intake
            .persist_intake(
                "test",
                message,
                account,
                "thread-a",
                &serde_json::json!({"outcome":"report"}),
                10,
            )
            .await
            .unwrap();
        let d = diagnosis(
            self.intake
                .stored_intake("test", message, account)
                .await
                .unwrap()
                .unwrap()
                .id,
        );
        self.register(&d, 10).await;
        d
    }
    async fn register(&self, d: &Diagnosis, now: u64) -> FileIdentity {
        self.engine
            .register_file(&d.arr_instance_id, &d.resource, &fingerprint(d), now)
            .await
            .unwrap()
    }
    async fn grant(&self, d: &Diagnosis, account: &str) {
        self.register(d, 10).await;
        self.bridge
            .record_authority(d.report_id, &authority(d, account))
            .await
            .unwrap();
    }
    async fn sql(&self) -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(&self.path))
            .await
            .unwrap()
    }
}

async fn admin_request(
    pool: &sqlx::SqlitePool,
    key: &str,
    actor: &str,
    command: serde_json::Value,
) {
    sqlx::query("INSERT INTO admin_requests VALUES (?,?,?,'Reviewed saved evidence','pending_controller',10,10)")
        .bind(key).bind(actor).bind(command.to_string()).execute(pool).await.unwrap();
}
async fn request_state(pool: &sqlx::SqlitePool, key: &str) -> String {
    sqlx::query_scalar("SELECT state FROM admin_requests WHERE request_key=?")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn owner_approval_revalidates_all_current_authority_and_audits_a_single_execution() {
    for case in 0..8 {
        let f = Fixture::new().await;
        let d = f.report("viewer-a", "approve").await;
        f.save_diagnosis(&d).await;
        f.grant(&d, "viewer-a").await;
        let (consumer, pool) = f.consumer().await;
        match case {
            1 => f.bridge.clear_authority(d.report_id).await.unwrap(),
            2 => {
                let mut a = authority(&d, "viewer-a");
                a.access = Access::Denied;
                f.bridge.record_authority(d.report_id, &a).await.unwrap();
            }
            3 => {
                let mut fp = fingerprint(&d);
                fp.arr_file_id += 1;
                f.engine
                    .register_file(&d.arr_instance_id, &d.resource, &fp, 10)
                    .await
                    .unwrap();
            }
            4 => {
                sqlx::query("UPDATE diagnoses SET decided_at=11")
                    .execute(&f.sql().await)
                    .await
                    .unwrap();
            }
            5 => {
                let mut options = options();
                options.budgets.global_hour = 0;
                let bridge = Bridge::open(&f.path, f.engine.clone(), options)
                    .await
                    .unwrap();
                let consumer = admin::Consumer::open(
                    f._temp.path().join("admin.sqlite"),
                    bridge,
                    vec!["owner-a".into()],
                )
                .await
                .unwrap();
                admin_request(
                    &pool,
                    "a",
                    "owner-a",
                    serde_json::json!({"action":"approve","report":d.report_id,"decided_at":10}),
                )
                .await;
                assert_eq!(consumer.tick(10).await.unwrap(), 1);
                assert_eq!(request_state(&pool, "a").await, "controller_deferred");
                assert!(f.engine.jobs().await.unwrap().is_empty());
                continue;
            }
            _ => {}
        }
        admin_request(
            &pool,
            "a",
            if case == 6 {
                "revoked-owner"
            } else {
                "owner-a"
            },
            serde_json::json!({"action":"approve","report":d.report_id,"decided_at":10}),
        )
        .await;
        assert_eq!(
            consumer
                .tick(if case == 7 { 311 } else { 10 })
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            request_state(&pool, "a").await,
            if case == 0 {
                "controller_applied"
            } else if case == 4 || case == 6 {
                "controller_rejected"
            } else {
                "controller_deferred"
            }
        );
        assert_eq!(f.engine.jobs().await.unwrap().len(), usize::from(case == 0));
        assert_eq!(consumer.tick(12).await.unwrap(), 0);
        let audit: Vec<String> = sqlx::query_scalar("SELECT event FROM admin_audit ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[0], "controller_claimed");
        assert!(audit[1].starts_with(&request_state(&pool, "a").await));
        assert!(
            sqlx::query("DELETE FROM admin_audit")
                .execute(&pool)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn denial_and_closure_stop_automatic_jobs_but_never_cancel_frozen_repairs() {
    for action in ["deny", "close"] {
        for attached in [false, true] {
            let f = Fixture::new().await;
            let d = f.report("viewer-a", "control").await;
            f.save_diagnosis(&d).await;
            f.grant(&d, "viewer-a").await;
            if attached {
                f.bridge.consume(&d, 10).await.unwrap();
            }
            let (consumer, pool) = f.consumer().await;
            let mut command = serde_json::json!({"action":action,"report":d.report_id});
            if action == "deny" {
                command["decided_at"] = 10.into();
            }
            admin_request(&pool, "a", "owner-a", command).await;
            assert_eq!(consumer.tick(10).await.unwrap(), 1);
            assert_eq!(
                request_state(&pool, "a").await,
                if attached {
                    "controller_rejected"
                } else {
                    "controller_applied"
                }
            );
            f.bridge.tick(11).await.unwrap();
            assert_eq!(f.engine.jobs().await.unwrap().len(), usize::from(attached));
            let p = f.bridge.consume(&d, 12).await.unwrap();
            assert_eq!(
                p.state,
                if attached {
                    ProgressState::Working
                } else {
                    ProgressState::NeedsReview
                }
            );
        }
    }
}

#[tokio::test]
async fn owner_closure_preserves_review_status_over_an_advisory_diagnosis() {
    let f = Fixture::new().await;
    let mut d = f.report("viewer-a", "advisory").await;
    d.decision.action = RecommendedAction::ClientAdvice;
    f.save_diagnosis(&d).await;
    assert_eq!(
        f.bridge.consume(&d, 10).await.unwrap().state,
        ProgressState::Advice
    );
    let (consumer, pool) = f.consumer().await;
    admin_request(
        &pool,
        "close-advice",
        "owner-a",
        serde_json::json!({"action":"close","report":d.report_id}),
    )
    .await;
    assert_eq!(consumer.tick(11).await.unwrap(), 1);
    assert_eq!(
        request_state(&pool, "close-advice").await,
        "controller_applied"
    );

    // Restart and a still-readable advisory diagnosis must not overwrite the
    // owner's explicit closure with the automatic advice projection.
    let resumed = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    resumed.tick(12).await.unwrap();
    let progress = resumed
        .progress(d.report_id, "viewer-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(progress.state, ProgressState::NeedsReview);
    assert_eq!(progress.viewer_copy_key, "report.needs_review");
    assert_eq!(progress.job_id, None);
    assert!(f.engine.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn claimed_owner_commands_are_never_reexecuted_after_restart_or_by_second_consumer() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "interrupted").await;
    let (consumer, pool) = f.consumer().await;
    admin_request(
        &pool,
        "interrupted",
        "owner-a",
        serde_json::json!({"action":"close","report":d.report_id}),
    )
    .await;
    sqlx::query("UPDATE admin_requests SET state='controller_claimed'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(consumer.tick(309).await.unwrap(), 0);
    assert_eq!(
        request_state(&pool, "interrupted").await,
        "controller_claimed"
    );
    let restarted = admin::Consumer::open(
        f._temp.path().join("admin.sqlite"),
        f.bridge.clone(),
        vec!["owner-a".into()],
    )
    .await
    .unwrap();
    assert_eq!(restarted.tick(310).await.unwrap(), 0);
    assert_eq!(
        request_state(&pool, "interrupted").await,
        "controller_uncertain"
    );
    let controls: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_report_controls")
        .fetch_one(&f.sql().await)
        .await
        .unwrap();
    assert_eq!(controls, 0);
    admin_request(
        &pool,
        "valid",
        "owner-a",
        serde_json::json!({"action":"close","report":d.report_id}),
    )
    .await;
    let (a, b) = tokio::join!(consumer.tick(311), restarted.tick(311));
    assert_eq!(a.unwrap() + b.unwrap(), 1);
    assert_eq!(request_state(&pool, "valid").await, "controller_applied");
    let audit: Vec<String> =
        sqlx::query_scalar("SELECT event FROM admin_audit WHERE request_key='interrupted'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].contains("Interrupted action"));
}

#[tokio::test]
async fn template_commands_use_only_authenticated_routes_and_fixed_copy() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "message").await;
    let (consumer, pool) = f.consumer().await;
    admin_request(
        &pool,
        "missing",
        "owner-a",
        serde_json::json!({"action":"message","report":d.report_id,"template":"received"}),
    )
    .await;
    consumer.tick(10).await.unwrap();
    assert_eq!(request_state(&pool, "missing").await, "controller_deferred");
    let mut a = authority(&d, "viewer-a");
    a.route = Some(ViewerRoute {
        recipient: pitcrew_domain::channels::ChannelIdentity {
            channel_id: "chat".into(),
            platform_user_id: "verified-user".into(),
        },
        conversation: "verified-thread".into(),
        last_inbound: Some(10),
    });
    f.bridge.record_authority(d.report_id, &a).await.unwrap();
    for template in ["received", "need_details", "under_review"] {
        admin_request(
            &pool,
            template,
            "owner-a",
            serde_json::json!({"action":"message","report":d.report_id,"template":template}),
        )
        .await;
    }
    admin_request(&pool,"forged","owner-a",serde_json::json!({"action":"message","report":d.report_id,"template":"received","recipient":"other"})).await;
    assert_eq!(consumer.tick(11).await.unwrap(), 4);
    let notifications = f.bridge.lifecycle_notifications().await.unwrap();
    assert_eq!(notifications.len(), 3);
    let mut expected_texts = std::collections::BTreeSet::from([
        "We received your report and will review it.".to_string(),
        "Please reply with the episode or movie, your device, and what happened when you tried to play it.".to_string(),
        "Your report is under review. We will update you when we have a verified result.".to_string(),
    ]);
    for notification in notifications {
        assert_eq!(notification.report_id, d.report_id);
        assert_eq!(notification.job_id, None);
        assert_eq!(
            notification.purpose,
            pitcrew_domain::delivery::DeliveryPurpose::TemplateMessage
        );
        assert_eq!(
            notification.payload.recipient.platform_user_id,
            "verified-user"
        );
        assert_eq!(notification.payload.conversation, "verified-thread");
        assert_eq!(notification.state, "pending");
        assert_eq!(notification.payload.last_inbound, Some(10));
        assert!(expected_texts.remove(&notification.payload.text));
    }
    assert!(expected_texts.is_empty());
    assert_eq!(request_state(&pool, "forged").await, "controller_rejected");
    assert_eq!(consumer.tick(12).await.unwrap(), 0);
    assert_eq!(f.bridge.lifecycle_notifications().await.unwrap().len(), 3);
}

#[tokio::test]
async fn controller_rejects_empty_or_partly_blank_allowlists_against_a_valid_journal() {
    let f = Fixture::new().await;
    let (_consumer, _pool) = f.consumer().await;
    for admins in [
        vec![],
        vec![String::new()],
        vec![" \t".into()],
        vec!["owner-a".into(), " ".into()],
    ] {
        assert!(matches!(
            admin::Consumer::open(
                f._temp.path().join("admin.sqlite"),
                f.bridge.clone(),
                admins
            )
            .await,
            Err(Error::Invalid)
        ));
    }
}

#[tokio::test]
async fn controller_request_bounds_reject_invalid_rows_and_accept_exact_limits() {
    for (key, note, allowed) in [
        ("empty-note".to_string(), String::new(), false),
        ("blank-note".into(), " \t ".into(), false),
        ("maximum-note".into(), "x".repeat(1000), true),
        ("oversized-note".into(), "x".repeat(1001), false),
        (String::new(), "Reviewed".into(), false),
        ("k".repeat(128), "Reviewed".into(), true),
        ("k".repeat(129), "Reviewed".into(), false),
    ] {
        let f = Fixture::new().await;
        let d = f.report("viewer-a", "close").await;
        let (consumer, pool) = f.consumer().await;
        sqlx::query(
            "INSERT INTO admin_requests VALUES (?,'owner-a',?,?,'pending_controller',10,10)",
        )
        .bind(&key)
        .bind(serde_json::json!({"action":"close","report":d.report_id}).to_string())
        .bind(&note)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(consumer.tick(10).await.unwrap(), 1);
        assert_eq!(
            request_state(&pool, &key).await,
            if allowed {
                "controller_applied"
            } else {
                "controller_rejected"
            }
        );
        let controls: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_report_controls")
            .fetch_one(&f.sql().await)
            .await
            .unwrap();
        assert_eq!(controls, i64::from(allowed));
        assert!(f.engine.jobs().await.unwrap().is_empty());
        assert_eq!(consumer.tick(11).await.unwrap(), 0);
    }
}

#[tokio::test]
async fn lower_quality_requires_the_pending_state_even_at_the_current_revision() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "not-pending").await;
    f.grant(&d, "viewer-a").await;
    let job = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let before = f.engine.load_job(job).await.unwrap();
    let (consumer, pool) = f.consumer().await;
    admin_request(
        &pool,
        "lower",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":before.revision}),
    )
    .await;
    assert_eq!(consumer.tick(11).await.unwrap(), 1);
    assert_eq!(request_state(&pool, "lower").await, "controller_rejected");
    assert_eq!(f.engine.load_job(job).await.unwrap(), before);
    assert!(f.engine.outbox(job).await.unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_lower_quality_approval_is_explicitly_deferred_without_effects() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "lower").await;
    f.grant(&d, "viewer-a").await;
    let job = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let stored = f.engine.load_job(job).await.unwrap();
    let lease = f.engine.acquire_lease(job, "test", 11, 30).await.unwrap();
    f.engine
        .apply_event(
            &lease,
            stored.revision,
            "request-lower",
            Event::RequestLowerQuality {
                candidate: pitcrew_domain::ReleaseCandidate {
                    release: 7,
                    identity: Default::default(),
                    quality: pitcrew_domain::QualityRelation::Lower,
                },
                access: Access::Granted {
                    library: 1,
                    expires_at: 1000,
                },
            },
            11,
        )
        .await
        .unwrap();
    f.engine.release_lease(&lease, 11).await.unwrap();
    let pending = f.engine.load_job(job).await.unwrap();
    let (consumer, pool) = f.consumer().await;
    admin_request(
        &pool,
        "exact",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":pending.revision}),
    )
    .await;
    admin_request(
        &pool,
        "stale",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":pending.revision-1}),
    )
    .await;
    assert_eq!(consumer.tick(12).await.unwrap(), 2);
    assert_eq!(request_state(&pool, "exact").await, "controller_deferred");
    assert_eq!(request_state(&pool, "stale").await, "controller_rejected");
    assert_eq!(f.engine.load_job(job).await.unwrap(), pending);
    assert!(f.engine.outbox(job).await.unwrap().is_empty());
    let consumer = consumer.with_lower_quality_shadow(Some(720));
    admin_request(
        &pool,
        "legacy-shadow",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":pending.revision}),
    )
    .await;
    assert_eq!(consumer.tick(13).await.unwrap(), 1);
    assert_eq!(
        request_state(&pool, "legacy-shadow").await,
        "controller_deferred"
    );
    assert_eq!(f.engine.load_job(job).await.unwrap(), pending);
}

#[tokio::test]
async fn owner_pinning_uses_current_exclusion_and_keeps_its_evidence() {
    use pitcrew_store::policy::{ExclusionKind, NewExclusion};
    let f = Fixture::new().await;
    let spec = NewExclusion {
        client: "arr-a".into(),
        release: 7,
        identity: Default::default(),
        kind: ExclusionKind::Quarantine,
        reason: "Bad decode".into(),
        evidence: serde_json::json!({"decode":"bad"}),
        pinned: false,
        created_at: 10,
    };
    let saved = f
        .engine
        .exclude_release(&spec, "test-exclusion")
        .await
        .unwrap();
    let (consumer, pool) = f.consumer().await;
    admin_request(
        &pool,
        "pin",
        "owner-a",
        serde_json::json!({"action":"pin","id":saved.id}),
    )
    .await;
    admin_request(
        &pool,
        "missing",
        "owner-a",
        serde_json::json!({"action":"pin","id":999}),
    )
    .await;
    assert_eq!(consumer.tick(10).await.unwrap(), 2);
    assert_eq!(request_state(&pool, "pin").await, "controller_applied");
    assert_eq!(request_state(&pool, "missing").await, "controller_rejected");
    assert_eq!(
        f.engine
            .exclusions("arr-a", saved.expires_at)
            .await
            .unwrap()[0]
            .spec,
        spec
    );
    let audit = f.engine.policy_audit().await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "owner-a");
    assert_eq!(audit[0].note, "Reviewed saved evidence");
}

#[tokio::test]
async fn unavailable_prerequisite_holds_unlinked_report_without_overwriting_a_job_or_newer_time() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "hold").await;
    f.bridge.hold_report(d.report_id, 10).await.unwrap();
    let held = f
        .bridge
        .progress(d.report_id, "viewer-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held.state, ProgressState::NeedsReview);
    assert_eq!(held.job_id, None);
    f.bridge.hold_report(d.report_id, 9).await.unwrap();
    assert_eq!(
        f.bridge.progress(d.report_id, "viewer-a").await.unwrap(),
        Some(held)
    );
    f.grant(&d, "viewer-a").await;
    let working = f.bridge.consume(&d, 10).await.unwrap();
    f.bridge.hold_report(d.report_id, 11).await.unwrap();
    assert_eq!(
        f.bridge.progress(d.report_id, "viewer-a").await.unwrap(),
        Some(working)
    );
}
#[tokio::test]
async fn exact_evidence_creates_once_and_repeat_viewers_share_one_job_across_restart() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let p = f.bridge.consume(&d, 10).await.unwrap();
    assert_eq!(p.state, ProgressState::Working);
    assert_eq!(p.viewer_copy_key, "report.working");
    let id = p.job_id.unwrap();
    // Persisted creation keys/IDs are a restart protocol, stable across upgrades.
    assert_eq!(id, 6_054_730_433_464_821_081);
    let job = f.engine.load_job(id).await.unwrap();
    assert_eq!(job.spec.original, FileIdentity { id: 3, revision: 1 });
    assert_eq!(job.spec.resource, d.resource);
    assert_eq!(job.spec.library, 1);
    assert_eq!(job.spec.viewer, "viewer-a");
    assert_eq!(job.spec.policy, options().policy);
    assert_eq!(job.spec.timeouts, options().timeouts);
    assert_eq!(job.spec.created_at, 10);
    assert_eq!(job.revision, 1);
    assert!(f.engine.outbox(id).await.unwrap().is_empty());
    let d2 = f.report("viewer-b", "b").await;
    f.grant(&d2, "viewer-b").await;
    let reopened = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    assert_eq!(reopened.consume(&d2, 11).await.unwrap().job_id, Some(id));
    assert_eq!(reopened.consume(&d, 12).await.unwrap().job_id, Some(id));
    assert_eq!(f.engine.jobs().await.unwrap(), vec![id]);
    assert_eq!(f.engine.load_job(id).await.unwrap().revision, 1);
    assert!(
        reopened
            .progress(d.report_id, "viewer-b")
            .await
            .unwrap()
            .is_none()
    );
}
#[tokio::test]
async fn absent_or_mismatched_authority_and_nonbroken_evidence_never_create_a_job() {
    let f = Fixture::new().await;
    for case in 0..26 {
        // Reports remain independent while sharing the same empty engine;
        // none of these inputs may reach job creation or budget reservation.
        let mut d = f.report("viewer-a", &format!("case-{case}")).await;
        let mut a = authority(&d, "viewer-a");
        match case {
            0 => {}
            1 => a.access = Access::Unknown,
            2 => a.access = Access::Denied,
            3 => {
                a.access = Access::Granted {
                    library: 2,
                    expires_at: 1000,
                }
            }
            4 => {
                a.access = Access::Granted {
                    library: 1,
                    expires_at: 10,
                }
            }
            5 => a.observed_at = 11,
            6 => d.decided_at = 11,
            7 => a.resource = "other".into(),
            8 => a.library = 2,
            9 => a.arr_instance_id = "other".into(),
            10 => a.arr_item_id = 9,
            11 => a.arr_file_id = 9,
            12 => a.file.revision = 9,
            13 => a.decode = DecodeOutcome::Good,
            14 => {
                d.evidence_summary.playback.decode = DecodeOutcome::Unknown;
                a.decode = DecodeOutcome::Unknown;
            }
            15 => {
                d.evidence_summary.scope = Scope::Show;
            }
            16 => d.evidence_summary.injection_suspected = true,
            17 => {
                d.agent_file.id = 0;
                a.file.id = 0;
            }
            18 => {
                d.agent_file.revision = 0;
                a.file.revision = 0;
            }
            19 => {
                d.arr_item_id = 0;
                a.arr_item_id = 0;
            }
            20 => {
                d.arr_file_id = 0;
                a.arr_file_id = 0;
            }
            21 => d.decision.constraint = None,
            22 => {
                a.access = Access::Granted {
                    library: 1,
                    expires_at: 9,
                }
            }
            23 => a.probe = ProbeOutcome::NoVideo,
            24 => d.resource.clear(),
            25 => d.arr_instance_id.clear(),
            _ => unreachable!(),
        }
        if case != 0 {
            f.bridge.record_authority(d.report_id, &a).await.unwrap();
        }
        let p = f.bridge.consume(&d, 10).await.unwrap();
        assert_eq!(p.state, ProgressState::NeedsReview, "case {case}");
        assert_eq!(p.viewer_copy_key, "report.needs_review");
        assert!(p.job_id.is_none());
        assert!(f.engine.jobs().await.unwrap().is_empty());
    }
}
#[tokio::test]
async fn hdr_without_profile5_preserves_the_exact_replacement_invariant() {
    let f = Fixture::new().await;
    let mut d = f.report("viewer-a", "hdr").await;
    d.decision.constraint = Some(ReplacementConstraint::HdrOkNoProfile5);
    f.grant(&d, "viewer-a").await;
    let p = f.bridge.consume(&d, 10).await.unwrap();
    let job = f.engine.load_job(p.job_id.unwrap()).await.unwrap();
    assert!(job.spec.constraints.forbid_dolby_vision_profile5);
    assert!(!job.spec.constraints.require_sdr);
    assert_eq!(job.spec.constraints.language, None);
}
#[tokio::test]
async fn authority_account_binding_stale_boundary_and_probe_proof() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    assert!(
        f.bridge
            .record_authority(d.report_id, &authority(&d, "viewer-b"))
            .await
            .is_err()
    );
    for mutate in [0, 1] {
        let mut a = authority(&d, "viewer-a");
        if mutate == 0 {
            a.resource.clear();
        } else {
            a.arr_instance_id.clear();
        }
        assert!(f.bridge.record_authority(d.report_id, &a).await.is_err());
    }
    f.grant(&d, "viewer-a").await;
    f.register(&d, 311).await; // Isolate stale proof from registry freshness.
    assert_eq!(
        f.bridge.consume(&d, 311).await.unwrap().state,
        ProgressState::NeedsReview
    );
    let boundary = Fixture::new().await;
    let exact = boundary.report("viewer-a", "a").await;
    boundary.grant(&exact, "viewer-a").await;
    assert_eq!(
        boundary.bridge.consume(&exact, 310).await.unwrap().state,
        ProgressState::Working
    );
    for probe in [ProbeOutcome::OpenFailed, ProbeOutcome::NoVideo] {
        let f = Fixture::new().await;
        let mut d = f.report("viewer-a", "a").await;
        d.evidence_summary.playback.decode = DecodeOutcome::Unknown;
        d.evidence_summary.playback.probe = probe;
        f.grant(&d, "viewer-a").await;
        assert!(f.bridge.consume(&d, 10).await.unwrap().job_id.is_some());
    }
}
#[tokio::test]
async fn advice_never_opens_a_job_even_with_proof() {
    for action in [
        RecommendedAction::Analyze,
        RecommendedAction::AdviseHd,
        RecommendedAction::ClientAdvice,
        RecommendedAction::SelectAudioTrack,
        RecommendedAction::NotifyAdmin,
        RecommendedAction::NotifyUrgent,
        RecommendedAction::ExplainPlayOrigin,
        RecommendedAction::Close,
        RecommendedAction::CollectEvidence,
    ] {
        let f = Fixture::new().await;
        let mut d = f.report("viewer-a", "a").await;
        d.decision.action = action;
        f.grant(&d, "viewer-a").await;
        let p = f.bridge.consume(&d, 10).await.unwrap();
        assert_eq!(p.state, ProgressState::Advice);
        assert_eq!(p.viewer_copy_key, "report.advice");
        assert!(f.engine.jobs().await.unwrap().is_empty());
    }
}
#[tokio::test]
async fn constraints_are_carried_without_weakening_and_budgets_hold() {
    for (constraint, language, sdr) in [
        (ReplacementConstraint::AnyWorking, None, false),
        (ReplacementConstraint::Sdr, None, true),
        (ReplacementConstraint::EnglishAudio, Some("eng"), false),
        (ReplacementConstraint::SpanishAudio, Some("spa"), false),
    ] {
        let f = Fixture::new().await;
        let mut d = f.report("viewer-a", "a").await;
        d.decision.constraint = Some(constraint);
        f.grant(&d, "viewer-a").await;
        let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
        let spec = f.engine.load_job(id).await.unwrap().spec;
        assert_eq!(spec.constraints.language.as_deref(), language);
        assert_eq!(spec.constraints.require_sdr, sdr);
    }
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let mut opts = options();
    opts.budgets.global_hour = 1;
    let b = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
    assert_eq!(
        b.consume(&d, 10).await.unwrap().state,
        ProgressState::NeedsReview
    );
    assert!(f.engine.jobs().await.unwrap().is_empty());
}
#[tokio::test]
async fn failed_state_projects_honestly_and_replay_cannot_claim_fixed() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    assert!(
        !f.bridge
            .reply_for_job(id)
            .await
            .unwrap()
            .contains("Ready to watch")
    );
    let lease = f.engine.acquire_lease(id, "test", 20, 300).await.unwrap();
    let current = f.engine.load_job(id).await.unwrap();
    f.engine
        .apply_event(&lease, current.revision, "timeout", Event::Tick, 110)
        .await
        .unwrap();
    f.engine.release_lease(&lease, 110).await.unwrap();
    assert_eq!(f.bridge.project(111).await.unwrap(), 1);
    let p = f
        .bridge
        .progress(d.report_id, "viewer-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.state, ProgressState::CouldNotFix);
    assert_eq!(p.viewer_copy_key, "report.could_not_fix");
    assert_eq!(p.updated_at, 111);
    assert_eq!(
        f.bridge.consume(&d, 112).await.unwrap().state,
        ProgressState::CouldNotFix
    );
    assert!(
        f.bridge
            .reply_for_job(id)
            .await
            .unwrap()
            .contains("needs the server owner")
    );
    assert_eq!(f.engine.outbox(id).await.unwrap().len(), 2);
}
#[tokio::test]
async fn sqlite_seam_accepts_decision_or_report_and_bad_rows_do_not_block() {
    let f = Fixture::new().await;
    assert_eq!(f.bridge.tick(10).await.unwrap(), Tick::default());
    let pool = f.sql().await;
    // Rows go straight into the intake store's real `diagnoses` table so some
    // can be malformed on purpose.
    for i in 0..4 {
        let d = f.report("viewer-a", &format!("message-{i}")).await;
        // A valid diagnosis without independent authority must be held and
        // counted alongside malformed diagnoses, without blocking ready work.
        if i != 3 {
            f.grant(&d, "viewer-a").await;
        }
        let decision = match i {
            0 | 3 => serde_json::to_string(&d.decision).unwrap(),
            1 => serde_json::to_string(&pitcrew_deciders::DecisionReport {
                decision: d.decision,
                usage: Default::default(),
            })
            .unwrap(),
            _ => "invalid".into(),
        };
        sqlx::query("INSERT INTO diagnoses(report_id,decided_at,decider_kind,decision,evidence_summary,resource,library_id,arr_instance_id,arr_item_id,arr_file_id,agent_file,agent_observed_at,access_observed_at,access_expires_at) VALUES(?,10,'rules',?,?,'library-a:episode-a','1','series-a',2,3,?,10,10,100)")
            .bind(d.report_id).bind(decision).bind(serde_json::to_string(&d.evidence_summary).unwrap()).bind(serde_json::to_string(&d.agent_file).unwrap()).execute(&pool).await.unwrap();
    }
    let result = f.bridge.tick(10).await.unwrap();
    assert_eq!(
        result,
        Tick {
            examined: 4,
            linked: 2,
            held: 2,
            projected: 2
        }
    );
    assert_eq!(f.engine.jobs().await.unwrap().len(), 1);
    let untrusted = f
        .intake
        .stored_intake("test", "message-3", "viewer-a")
        .await
        .unwrap()
        .unwrap();
    let progress = f
        .bridge
        .progress(untrusted.id, "viewer-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(progress.state, ProgressState::NeedsReview);
    assert_eq!(progress.job_id, None);
    assert_eq!(
        f.bridge.tick(11).await.unwrap(),
        Tick {
            examined: 2,
            linked: 0,
            held: 2,
            projected: 2
        }
    );
}
#[tokio::test]
async fn durable_request_recovers_engine_commit_and_never_retargets_changed_input() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    sqlx::query("DELETE FROM report_progress")
        .execute(&f.sql().await)
        .await
        .unwrap();
    let b = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    assert_eq!(b.consume(&d, 11).await.unwrap().job_id, Some(id));
    assert_eq!(f.engine.jobs().await.unwrap(), vec![id]);
    sqlx::query("DELETE FROM report_progress")
        .execute(&f.sql().await)
        .await
        .unwrap();
    let mut changed = d.clone();
    changed.arr_item_id = 99;
    f.grant(&changed, "viewer-a").await;
    assert_eq!(b.consume(&changed, 11).await.unwrap().job_id, Some(id));
    assert_eq!(
        f.engine.load_job(id).await.unwrap().spec.original,
        FileIdentity { id: 3, revision: 1 }
    );
}

#[tokio::test]
async fn failed_creation_never_backdates_a_later_budget_reservation() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let mut opts = options();
    opts.budgets.global_hour = 1;
    let b = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
    assert_eq!(
        b.consume(&d, 10).await.unwrap().state,
        ProgressState::NeedsReview
    );
    // Reopening with a corrected budget must not let the frozen command spend
    // against its historical timestamp, even though the current budget fits.
    let corrected = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    assert_eq!(
        corrected.consume(&d, 11).await.unwrap().state,
        ProgressState::NeedsReview
    );
    assert!(f.engine.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_uncommitted_command_cannot_preserve_a_superseded_grab_allowance() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let mut denied = options();
    denied.budgets.global_hour = 1;
    let first = Bridge::open(&f.path, f.engine.clone(), denied)
        .await
        .unwrap();
    assert_eq!(
        first.consume(&d, 10).await.unwrap().state,
        ProgressState::NeedsReview
    );

    // Retry within the same clock tick, isolating the current policy check
    // from the stale-command guard and the now-sufficient current budgets.
    let mut stricter = options();
    stricter.policy.max_grabs = 1;
    let reopened = Bridge::open(&f.path, f.engine.clone(), stricter)
        .await
        .unwrap();
    assert_eq!(
        reopened.consume(&d, 10).await.unwrap().state,
        ProgressState::NeedsReview
    );
    assert!(f.engine.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn received_reports_have_a_projection_without_a_diagnosis() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.bridge.tick(10).await.unwrap();
    let p = f
        .bridge
        .progress(d.report_id, "viewer-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.state, ProgressState::Received);
    assert_eq!(p.viewer_copy_key, "report.received");
    assert_eq!(p.updated_at, 10);
    assert_eq!(p.job_id, None);
}

#[tokio::test]
async fn cooldown_and_resource_ownership_cannot_be_bypassed_by_changed_file_or_constraint() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let mut d2 = f.report("viewer-a", "b").await;
    d2.agent_file.revision += 1;
    f.grant(&d2, "viewer-a").await;
    assert_eq!(
        f.bridge.consume(&d2, 10).await.unwrap().state,
        ProgressState::NeedsReview
    );
    let lease = f.engine.acquire_lease(id, "test", 20, 1000).await.unwrap();
    let job = f.engine.load_job(id).await.unwrap();
    f.engine
        .apply_event(&lease, job.revision, "timeout", Event::Tick, 110)
        .await
        .unwrap();
    f.engine.release_lease(&lease, 110).await.unwrap();
    let mut d3 = f.report("viewer-a", "c").await;
    d3.agent_file.revision += 2;
    f.grant(&d3, "viewer-a").await;
    assert_eq!(
        f.bridge.consume(&d3, 110).await.unwrap().state,
        ProgressState::NeedsReview
    );
    assert_eq!(f.engine.jobs().await.unwrap(), vec![id]);
}

#[tokio::test]
async fn late_repeat_report_inherits_terminal_status_immediately() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let lease = f.engine.acquire_lease(id, "test", 20, 300).await.unwrap();
    let job = f.engine.load_job(id).await.unwrap();
    f.engine
        .apply_event(&lease, job.revision, "timeout", Event::Tick, 110)
        .await
        .unwrap();
    f.engine.release_lease(&lease, 110).await.unwrap();
    let d2 = f.report("viewer-b", "b").await;
    f.grant(&d2, "viewer-b").await;
    assert_eq!(
        f.bridge.consume(&d2, 111).await.unwrap().state,
        ProgressState::CouldNotFix
    );
    assert_eq!(f.engine.jobs().await.unwrap(), vec![id]);
}

#[tokio::test]
async fn concurrent_reports_reserve_one_job_and_progress_clock_never_rewinds() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let d2 = f.report("viewer-b", "b").await;
    f.grant(&d2, "viewer-b").await;
    let b = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    let (left, right) = tokio::join!(f.bridge.consume(&d, 10), b.consume(&d2, 10));
    assert_eq!(left.unwrap().job_id, right.unwrap().job_id);
    assert_eq!(f.engine.jobs().await.unwrap().len(), 1);
    f.bridge.project(20).await.unwrap();
    b.project(19).await.unwrap();
    assert_eq!(
        b.progress(d.report_id, "viewer-a")
            .await
            .unwrap()
            .unwrap()
            .updated_at,
        20
    );
}

#[tokio::test]
async fn committed_job_recovers_after_authority_expires_without_a_new_mutation() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    sqlx::query("DELETE FROM report_progress")
        .execute(&f.sql().await)
        .await
        .unwrap();
    let b = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    assert_eq!(b.consume(&d, 2000).await.unwrap().job_id, Some(id));
    assert_eq!(f.engine.load_job(id).await.unwrap().revision, 1);
    assert_eq!(f.engine.jobs().await.unwrap(), vec![id]);
}

#[tokio::test]
async fn changed_diagnosis_after_denied_creation_holds_instead_of_poisoning_the_batch() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let mut opts = options();
    opts.budgets.global_hour = 1;
    let b = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
    assert_eq!(
        b.consume(&d, 10).await.unwrap().state,
        ProgressState::NeedsReview
    );
    let mut changed = d.clone();
    changed.agent_file.revision = 9;
    f.grant(&changed, "viewer-a").await;
    assert_eq!(
        b.consume(&changed, 11).await.unwrap().state,
        ProgressState::NeedsReview
    );
    assert!(f.engine.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn initial_projection_schema_upgrades_and_zero_safety_bounds_are_rejected() {
    let f = Fixture::new().await;
    let pool = f.sql().await;
    sqlx::query("DROP TABLE report_progress")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE report_progress(report_id INTEGER PRIMARY KEY,job_id INTEGER,state TEXT,updated_at INTEGER,viewer_copy_key TEXT)").execute(&pool).await.unwrap();
    let b = Bridge::open(&f.path, f.engine.clone(), options())
        .await
        .unwrap();
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    assert!(b.consume(&d, 10).await.unwrap().job_id.is_some());
    for which in 0..2 {
        let mut options = options();
        if which == 0 {
            options.cooldown_seconds = 0;
        } else {
            options.evidence_max_age = 0;
        }
        assert!(matches!(
            Bridge::open(&f.path, f.engine.clone(), options).await,
            Err(Error::Invalid)
        ));
    }
}

#[tokio::test]
async fn tick_recovers_committed_mapping_without_a_current_diagnosis_or_grant() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let pool = f.sql().await;
    sqlx::query("DELETE FROM report_progress")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM bridge_authority")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        f.bridge.tick(2000).await.unwrap(),
        Tick {
            examined: 0,
            linked: 0,
            held: 0,
            projected: 1
        }
    );
    assert_eq!(
        f.bridge
            .progress(d.report_id, "viewer-a")
            .await
            .unwrap()
            .unwrap()
            .job_id,
        Some(id)
    );
    assert_eq!(f.engine.load_job(id).await.unwrap().revision, 1);
}

#[tokio::test]
async fn an_uncommitted_command_always_honors_current_budget_caps() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let mut opts = options();
    opts.budgets.global_hour = 1;
    let b = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
    assert!(b.consume(&d, 10).await.unwrap().job_id.is_none());
    sqlx::query("UPDATE bridge_requests SET limits='[8,8,4,4]'")
        .execute(&f.sql().await)
        .await
        .unwrap();
    assert!(b.consume(&d, 10).await.unwrap().job_id.is_none());
    let mut tighter = options();
    tighter.policy.max_grabs = 1;
    let b = Bridge::open(&f.path, f.engine.clone(), tighter)
        .await
        .unwrap();
    assert!(b.consume(&d, 10).await.unwrap().job_id.is_none());
    assert!(f.engine.jobs().await.unwrap().is_empty());
}

#[tokio::test]
async fn job_uses_registered_arr_identity_not_the_agent_identity_namespace() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let fingerprint = pitcrew_store::policy::IdentityFingerprint {
        arr_file_id: d.arr_file_id,
        size: 1024,
        date_added: "2026-01-01T00:00:00Z".into(),
        quality: "{\"quality\":{\"id\":1}}".into(),
        agent: d.agent_file,
    };
    let expected = f
        .engine
        .register_file(&d.arr_instance_id, &d.resource, &fingerprint, 10)
        .await
        .unwrap();
    assert_ne!(expected, d.agent_file);
    let id = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let job = f.engine.load_job(id).await.unwrap();
    assert_eq!(job.spec.original, expected);
}

#[tokio::test]
async fn absent_mismatched_future_or_stale_registry_cannot_authorize_creation() {
    for case in 0..5 {
        let f = Fixture::new().await;
        let mut d = f.report("viewer-a", "a").await;
        f.grant(&d, "viewer-a").await;
        let mut current = fingerprint(&d);
        let now = match case {
            0 => {
                let pool = SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_with(
                        SqliteConnectOptions::new().filename(f._temp.path().join("engine.sqlite")),
                    )
                    .await
                    .unwrap();
                sqlx::query("DELETE FROM current_file_revisions")
                    .execute(&pool)
                    .await
                    .unwrap();
                10
            }
            1 => {
                current.arr_file_id += 1;
                f.engine
                    .register_file(&d.arr_instance_id, &d.resource, &current, 10)
                    .await
                    .unwrap();
                10
            }
            2 => {
                current.agent.revision += 1;
                f.engine
                    .register_file(&d.arr_instance_id, &d.resource, &current, 10)
                    .await
                    .unwrap();
                10
            }
            3 => {
                f.register(&d, 11).await;
                10
            }
            4 => {
                d.decided_at = 311;
                let mut a = authority(&d, "viewer-a");
                a.observed_at = 311;
                f.bridge.record_authority(d.report_id, &a).await.unwrap();
                311
            }
            _ => unreachable!(),
        };
        assert_eq!(
            f.bridge.consume(&d, now).await.unwrap().state,
            ProgressState::NeedsReview,
            "case {case}"
        );
        assert!(f.engine.jobs().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn registry_aba_revision_never_dedupes_to_an_old_terminal_repair() {
    let f = Fixture::new().await;
    let d = f.report("viewer-a", "a").await;
    f.grant(&d, "viewer-a").await;
    let mut repeat = f.report("viewer-a", "b").await;
    let first = f.bridge.consume(&d, 10).await.unwrap().job_id.unwrap();
    let lease = f
        .engine
        .acquire_lease(first, "test", 20, 300)
        .await
        .unwrap();
    let job = f.engine.load_job(first).await.unwrap();
    f.engine
        .apply_event(&lease, job.revision, "timeout", Event::Tick, 110)
        .await
        .unwrap();
    f.engine.release_lease(&lease, 110).await.unwrap();
    let mut different = fingerprint(&d);
    different.size += 1;
    f.engine
        .register_file(&d.arr_instance_id, &d.resource, &different, 3600)
        .await
        .unwrap();
    let current = f.register(&d, 3610).await;
    assert_eq!(current, FileIdentity { id: 3, revision: 3 });
    repeat.decided_at = 3610;
    let mut a = authority(&repeat, "viewer-a");
    a.observed_at = 3610;
    a.access = Access::Granted {
        library: 1,
        expires_at: 10_000,
    };
    f.bridge
        .record_authority(repeat.report_id, &a)
        .await
        .unwrap();
    let second = f
        .bridge
        .consume(&repeat, 3610)
        .await
        .unwrap()
        .job_id
        .unwrap();
    assert_ne!(second, first);
    assert_eq!(
        f.engine.load_job(second).await.unwrap().spec.original,
        current
    );
    f.engine
        .validate_file(
            &d.arr_instance_id,
            &d.resource,
            current,
            &fingerprint(&d),
            3610,
            300,
        )
        .await
        .unwrap();
}

async fn bounded_pending(f: &Fixture) -> (Diagnosis, u64, lower_quality::ReleaseEvidence) {
    bounded_pending_at(f, 10).await
}

async fn bounded_pending_at(
    f: &Fixture,
    created_at: u64,
) -> (Diagnosis, u64, lower_quality::ReleaseEvidence) {
    use pitcrew_domain::{LowerQualityBound, QualityRelation, ReleaseCandidate};
    let mut d = f.report("viewer-a", "bounded").await;
    d.decided_at = created_at;
    f.register(&d, created_at).await;
    let mut auth = authority(&d, "viewer-a");
    auth.observed_at = created_at;
    auth.access = Access::Granted {
        library: 1,
        expires_at: created_at + 990,
    };
    f.bridge.record_authority(d.report_id, &auth).await.unwrap();
    let job = f
        .bridge
        .consume(&d, created_at)
        .await
        .unwrap()
        .job_id
        .unwrap();
    let stored = f.engine.load_job(job).await.unwrap();
    let lease = f
        .engine
        .acquire_lease(job, "test", created_at + 1, 30)
        .await
        .unwrap();
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
    let pending = f
        .engine
        .apply_event(
            &lease,
            stored.revision,
            "bounded",
            Event::RequestBoundedLowerQuality {
                candidate: candidate.clone(),
                bound,
                access: auth.access,
            },
            created_at + 1,
        )
        .await
        .unwrap()
        .stored;
    f.engine
        .release_lease(&lease, created_at + 1)
        .await
        .unwrap();
    let evidence = lower_quality::ReleaseEvidence {
        revision: pending.revision,
        arr_instance_id: d.arr_instance_id.clone(),
        arr_item_id: d.arr_item_id,
        original: pending.job.current_original(),
        candidate,
        bound,
        observed_at: created_at + 1,
    };
    f.bridge.record_lower_quality(job, &evidence).await.unwrap();
    (d, job, evidence)
}

#[tokio::test]
async fn bounded_shadow_approval_records_guard_once_and_shadow_dispatch_calls_no_transport() {
    struct NoTransport;
    impl pitcrew_engine::Upstream for NoTransport {
        fn read<'a>(&'a self, _: &'a pitcrew_domain::Intent) -> pitcrew_engine::CallFuture<'a> {
            panic!("shadow read")
        }
        fn write<'a>(&'a self, _: &'a pitcrew_domain::Intent) -> pitcrew_engine::CallFuture<'a> {
            panic!("shadow write")
        }
    }
    let f = Fixture::new().await;
    let (_, job, evidence) = bounded_pending(&f).await;
    let (consumer, pool) = f.consumer().await;
    let consumer = consumer.with_lower_quality_shadow(Some(720));
    admin_request(
        &pool,
        "bounded",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
    )
    .await;
    assert_eq!(consumer.tick(12).await.unwrap(), 1);
    assert_eq!(request_state(&pool, "bounded").await, "controller_applied");
    let stored = f.engine.load_job(job).await.unwrap();
    assert_eq!(
        stored.job.state(),
        pitcrew_domain::State::UpgradeGuardIntent
    );
    assert_eq!(stored.job.reserved_budget(), 0);
    assert_eq!(
        stored.job.bounded_lower_quality(),
        Some((&evidence.candidate, evidence.bound))
    );
    let restarted = Store::open(f._temp.path().join("engine.sqlite"))
        .await
        .unwrap();
    assert_eq!(restarted.load_job(job).await.unwrap(), stored);
    let outbox = f.engine.outbox(job).await.unwrap();
    assert_eq!(restarted.outbox(job).await.unwrap(), outbox);
    assert_eq!(outbox.len(), 1);
    assert!(matches!(
        outbox[0].intent.effect,
        pitcrew_domain::Effect::InstallUpgradeGuard { .. }
    ));
    assert_eq!(consumer.tick(13).await.unwrap(), 0);
    let worker =
        pitcrew_engine::Worker::new(Some(restarted), NoTransport, Default::default()).unwrap();
    let lease = f.engine.acquire_lease(job, "shadow", 13, 60).await.unwrap();
    assert_eq!(worker.drive(&lease, 13).await.unwrap(), 1);
    assert_eq!(
        f.engine.outbox(job).await.unwrap()[0].status,
        pitcrew_store::OutboxStatus::Shadowed
    );
    assert_eq!(
        f.engine.load_job(job).await.unwrap().job.state(),
        pitcrew_domain::State::UpgradeGuardIntent
    );
}

#[tokio::test]
async fn bounded_approval_revalidates_every_authority_and_release_component() {
    for case in [
        "missing",
        "revision",
        "candidate",
        "bound",
        "original",
        "instance",
        "item",
        "future",
        "stale",
        "older_than_authority",
        "account",
        "resource",
        "library",
        "authority_future",
        "authority_stale",
        "access_unknown",
        "access_denied",
        "access_expired",
        "access_library",
        "not_broken",
        "file",
        "arr_file",
        "excluded",
        "registry_changed",
        "registry_stale",
        "busy",
        "live",
    ] {
        let f = Fixture::new().await;
        let (d, job, mut evidence) = bounded_pending(&f).await;
        let before = f.engine.load_job(job).await.unwrap();
        let mut auth = authority(&d, "viewer-a");
        let mut now = 12;
        match case {
            "revision" => evidence.revision += 1,
            "candidate" => evidence.candidate.release += 1,
            "bound" => evidence.bound.minimum_resolution += 1,
            "original" => evidence.original.revision += 1,
            "instance" => evidence.arr_instance_id = "other".into(),
            "item" => evidence.arr_item_id += 1,
            "future" => evidence.observed_at = 13,
            "stale" => {
                now = 312;
                auth.observed_at = 312;
            }
            "older_than_authority" => auth.observed_at = 12,
            "account" => auth.account = "other".into(),
            "resource" => auth.resource = "other".into(),
            "library" => auth.library += 1,
            "authority_future" => auth.observed_at = 13,
            "authority_stale" => {
                now = 311;
                evidence.observed_at = 311;
            }
            "access_unknown" => auth.access = Access::Unknown,
            "access_denied" => auth.access = Access::Denied,
            "access_expired" => {
                auth.access = Access::Granted {
                    library: 1,
                    expires_at: 12,
                }
            }
            "access_library" => {
                auth.access = Access::Granted {
                    library: 2,
                    expires_at: 1000,
                }
            }
            "not_broken" => {
                auth.decode = DecodeOutcome::Good;
                auth.probe = ProbeOutcome::Video;
            }
            "file" => auth.file.revision += 1,
            "arr_file" => auth.arr_file_id += 1,
            "excluded" => {
                f.engine
                    .exclude_release(
                        &pitcrew_store::policy::NewExclusion {
                            client: d.arr_instance_id.clone(),
                            release: 7,
                            identity: Default::default(),
                            kind: pitcrew_store::policy::ExclusionKind::DeadRelease,
                            reason: "Synthetic failure".into(),
                            evidence: serde_json::json!({}),
                            pinned: false,
                            created_at: 11,
                        },
                        "excluded",
                    )
                    .await
                    .unwrap();
            }
            "registry_changed" => {
                let mut changed = fingerprint(&d);
                changed.size += 1;
                f.engine
                    .register_file(&d.arr_instance_id, &d.resource, &changed, 11)
                    .await
                    .unwrap();
            }
            "registry_stale" => {
                now = 311;
                auth.observed_at = 311;
                evidence.observed_at = 311;
            }
            "busy" => {
                f.engine.acquire_lease(job, "other", 12, 60).await.unwrap();
            }
            _ => {}
        }
        // Deliberately tamper only in this fixture to test that a durable row is
        // evidence to revalidate, never an authorization bypass.
        sqlx::query("UPDATE bridge_authority SET authority=? WHERE report_id=?")
            .bind(serde_json::to_string(&auth).unwrap())
            .bind(d.report_id)
            .execute(&f.sql().await)
            .await
            .unwrap();
        f.bridge.record_lower_quality(job, &evidence).await.unwrap();
        if case == "missing" {
            f.bridge.clear_lower_quality(job).await.unwrap();
        }
        let (consumer, pool) = f.consumer().await;
        let consumer = if case == "live" {
            consumer
        } else {
            consumer.with_lower_quality_shadow(Some(720))
        };
        admin_request(
            &pool,
            case,
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":before.revision}),
        )
        .await;
        assert_eq!(consumer.tick(now).await.unwrap(), 1, "{case}");
        assert_eq!(
            request_state(&pool, case).await,
            "controller_deferred",
            "{case}"
        );
        assert_eq!(f.engine.load_job(job).await.unwrap(), before, "{case}");
        assert!(f.engine.outbox(job).await.unwrap().is_empty(), "{case}");
    }
}

#[tokio::test]
async fn bounded_approval_honors_reduced_budget_caps_without_spending_again() {
    for case in [
        "job",
        "global_hour",
        "global_day",
        "viewer_hour",
        "viewer_day",
    ] {
        let mut f = Fixture::new().await;
        let (_, job, evidence) = bounded_pending(&f).await;
        let mut opts = options();
        match case {
            "job" => opts.policy.max_grabs = 0,
            "global_hour" => opts.budgets.global_hour = 1,
            "global_day" => opts.budgets.global_day = 1,
            "viewer_hour" => opts.budgets.viewer_hour = 1,
            "viewer_day" => opts.budgets.viewer_day = 1,
            _ => unreachable!(),
        }
        f.bridge = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(Some(720));
        admin_request(
            &pool,
            "budget",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        assert_eq!(consumer.tick(12).await.unwrap(), 1);
        assert_eq!(
            request_state(&pool, "budget").await,
            "controller_deferred",
            "{case}"
        );
        assert!(f.engine.outbox(job).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn lower_quality_approval_expiry_does_not_renew_saved_evidence_or_access() {
    use pitcrew_domain::{Observation, TransitionError};
    for access_expiry in [13, 1000] {
        let mut f = Fixture::new().await;
        let (d, job, evidence) = bounded_pending(&f).await;
        let mut auth = authority(&d, "viewer-a");
        auth.access = Access::Granted {
            library: 1,
            expires_at: access_expiry,
        };
        f.bridge.record_authority(d.report_id, &auth).await.unwrap();
        let mut opts = options();
        opts.evidence_max_age = 5;
        f.bridge = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(Some(720));
        admin_request(
            &pool,
            "expiry",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        consumer.tick(12).await.unwrap();
        assert_eq!(request_state(&pool, "expiry").await, "controller_applied");
        let stored = f.engine.load_job(job).await.unwrap();
        let guarded = stored
            .job
            .apply(
                Event::Observed {
                    intent: stored.job.expected_observation().unwrap(),
                    observation: Observation::UpgradeGuardInstalled,
                },
                12,
            )
            .unwrap()
            .job;
        let expiry = access_expiry.min(15);
        let fresh_access = Access::Granted {
            library: 1,
            expires_at: 1000,
        };
        assert!(
            guarded
                .apply(
                    Event::GrabApproved {
                        access: fresh_access
                    },
                    expiry - 1
                )
                .is_ok()
        );
        assert_eq!(
            guarded.apply(
                Event::GrabApproved {
                    access: fresh_access
                },
                expiry
            ),
            Err(TransitionError::ApprovalRequired)
        );
    }
}

#[tokio::test]
async fn lower_quality_evidence_at_its_expiry_is_deferred_without_effects() {
    for expire_release in [true, false] {
        let mut f = Fixture::new().await;
        let (d, job, mut evidence) = bounded_pending(&f).await;
        let mut opts = options();
        opts.evidence_max_age = 2;
        f.bridge = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
        if expire_release {
            let mut auth = authority(&d, "viewer-a");
            auth.observed_at = 11;
            f.bridge.record_authority(d.report_id, &auth).await.unwrap();
            evidence.observed_at = 10;
            f.bridge.record_lower_quality(job, &evidence).await.unwrap();
        }
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(Some(720));
        admin_request(
            &pool,
            "expired",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        consumer.tick(12).await.unwrap();
        assert_eq!(request_state(&pool, "expired").await, "controller_deferred");
        assert!(f.engine.outbox(job).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn lower_quality_accepts_current_exact_open_failure_or_missing_video_evidence() {
    for probe in [ProbeOutcome::OpenFailed, ProbeOutcome::NoVideo] {
        let f = Fixture::new().await;
        let (d, job, evidence) = bounded_pending(&f).await;
        let mut auth = authority(&d, "viewer-a");
        auth.decode = DecodeOutcome::Unknown;
        auth.probe = probe;
        f.bridge.record_authority(d.report_id, &auth).await.unwrap();
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(Some(720));
        admin_request(
            &pool,
            "probe",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        consumer.tick(12).await.unwrap();
        assert_eq!(request_state(&pool, "probe").await, "controller_applied");
    }
}

#[tokio::test]
async fn lower_quality_consumer_requires_current_positive_matching_floor() {
    for minimum in [None, Some(0), Some(1080), Some(720)] {
        let f = Fixture::new().await;
        let (_, job, evidence) = bounded_pending(&f).await;
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(minimum);
        admin_request(
            &pool,
            "floor",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        consumer.tick(12).await.unwrap();
        assert_eq!(
            request_state(&pool, "floor").await,
            if minimum == Some(720) {
                "controller_applied"
            } else {
                "controller_deferred"
            },
            "{minimum:?}"
        );
        let audit: String = sqlx::query_scalar(
            "SELECT event FROM admin_audit WHERE request_key='floor' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            audit.contains("Lower-quality shadow approval is disabled."),
            minimum.is_none() || minimum == Some(0),
            "The audit must distinguish a removed opt-in from stale evidence."
        );
        if minimum != Some(720) {
            assert!(f.engine.outbox(job).await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn lower_quality_batch_revocation_discards_every_cached_proof_without_changing_job() {
    let f = Fixture::new().await;
    let (_, job, evidence) = bounded_pending(&f).await;
    let before = f.engine.load_job(job).await.unwrap();
    // A proof for a job absent from this batch must be invalidated as well.
    f.bridge
        .record_lower_quality(job + 1, &evidence)
        .await
        .unwrap();
    let pool = f.sql().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    f.bridge.clear_lower_quality_proofs().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let (consumer, admin) = f.consumer().await;
    let consumer = consumer.with_lower_quality_shadow(Some(720));
    admin_request(
        &admin,
        "batch-revoked",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
    )
    .await;
    consumer.tick(12).await.unwrap();
    assert_eq!(
        request_state(&admin, "batch-revoked").await,
        "controller_deferred"
    );
    assert_eq!(f.engine.load_job(job).await.unwrap(), before);
    assert!(f.engine.outbox(job).await.unwrap().is_empty());
}

#[tokio::test]
async fn lower_quality_accepts_fresh_equal_observation_and_approval_timestamps() {
    for (authority_at, release_at) in [(10, 10), (11, 11), (12, 12), (11, 12)] {
        let f = Fixture::new().await;
        let (d, job, mut evidence) = bounded_pending(&f).await;
        let mut auth = authority(&d, "viewer-a");
        auth.observed_at = authority_at;
        evidence.observed_at = release_at;
        f.bridge.record_authority(d.report_id, &auth).await.unwrap();
        f.bridge.record_lower_quality(job, &evidence).await.unwrap();
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(Some(720));
        admin_request(
            &pool,
            "fresh",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        consumer.tick(12).await.unwrap();
        assert_eq!(
            request_state(&pool, "fresh").await,
            "controller_applied",
            "{authority_at} {release_at}"
        );
    }
}

#[tokio::test]
async fn lower_quality_approval_freshness_uses_elapsed_time_at_realistic_epoch() {
    let created_at = 1_700_000_000;
    let f = Fixture::new().await;
    let (_, job, evidence) = bounded_pending_at(&f, created_at).await;
    let (consumer, pool) = f.consumer().await;
    let consumer = consumer.with_lower_quality_shadow(Some(720));
    admin_request(
        &pool,
        "epoch",
        "owner-a",
        serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
    )
    .await;
    consumer.tick(created_at + 2).await.unwrap();
    assert_eq!(request_state(&pool, "epoch").await, "controller_applied");
}

#[tokio::test]
async fn fresh_file_and_release_reads_do_not_renew_expired_broken_file_authority() {
    for now in [14, 15, 16] {
        let mut f = Fixture::new().await;
        let (d, job, mut evidence) = bounded_pending(&f).await;
        // Refresh bytes and release only. The independent decode/access
        // authority was collected at 10 and expires at 15.
        f.register(&d, now).await;
        evidence.observed_at = now;
        f.bridge.record_lower_quality(job, &evidence).await.unwrap();
        let mut opts = options();
        opts.evidence_max_age = 5;
        f.bridge = Bridge::open(&f.path, f.engine.clone(), opts).await.unwrap();
        let (consumer, pool) = f.consumer().await;
        let consumer = consumer.with_lower_quality_shadow(Some(720));
        admin_request(
            &pool,
            "fresh-bytes",
            "owner-a",
            serde_json::json!({"action":"lower_quality","job":job,"revision":evidence.revision}),
        )
        .await;
        consumer.tick(now).await.unwrap();
        assert_eq!(
            request_state(&pool, "fresh-bytes").await,
            if now < 15 {
                "controller_applied"
            } else {
                "controller_deferred"
            },
            "approval at {now}"
        );
        assert_eq!(
            f.engine.outbox(job).await.unwrap().len(),
            usize::from(now < 15)
        );
    }
}
