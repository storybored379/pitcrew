mod common;
use common::*;
use pitcrew_admin::{Error, actions::Command, read::Sources};
use pitcrew_domain::*;
use serde_json::json;

async fn sources(s: &Setup) -> Sources {
    Sources::open(
        &s.dir.path().join("engine.sqlite"),
        &s.dir.path().join("intake.sqlite"),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn optional_seams_and_exact_report_detail_are_read_only() {
    let s = Setup::new(false).await;
    s.report().await;
    let read = sources(&s).await;
    let initial = read.queue().await.unwrap();
    assert_eq!(initial.reports.len(), 1);
    assert_eq!(initial.reports[0]["id"], 1);
    assert_eq!(initial.reports[0]["created"], 90);
    assert_eq!(initial.reports[0]["channel"], "web");
    assert!(initial.reports[0].get("account").is_none());
    // The intake store always creates `diagnoses`; the bridge creates progress.
    assert_eq!(initial.diagnoses, Some(vec![]));
    assert!(initial.progress.is_none());
    s.seams().await;
    let exact = read.report(1).await.unwrap();
    assert_eq!(exact.reports, initial.reports);
    let diagnoses = exact.diagnoses.unwrap();
    assert_eq!(diagnoses[0]["resource"], "series-a:episode-a");
    // Decider comparisons live in their own table and are joined in.
    assert_eq!(diagnoses[0]["comparison"]["primary"], "rules");
    assert_eq!(
        read.queue().await.unwrap().diagnoses.unwrap()[0]["comparison"]["primary"],
        "rules"
    );
    assert_eq!(exact.progress, Some(vec![]));
    assert!(matches!(read.report(2).await, Err(Error::Missing)));
    assert!(read.report(u64::MAX).await.is_err());
    assert_eq!(
        scalar(
            &s.engine_sql,
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'admin_%'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &s.intake_sql,
            "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'admin_%'"
        )
        .await,
        0
    );
    assert!(
        Sources::open(
            &s.dir.path().join("missing"),
            &s.dir.path().join("intake.sqlite")
        )
        .await
        .is_err()
    );
    assert!(!s.dir.path().join("missing").exists());
}

#[tokio::test]
async fn lifecycle_delivery_projection_is_optional_exact_and_read_only() {
    let s = Setup::new(false).await;
    s.report().await;
    let read = sources(&s).await;
    assert_eq!(read.lifecycle_deliveries().await.unwrap(), None);
    sqlx::raw_sql(include_str!("../../../crates/bridge/src/schema.sql"))
        .execute(&s.intake_sql)
        .await
        .unwrap();
    let payload = json!({"recipient":{"channel_id":"chat","platform_user_id":"viewer-a"},"conversation":"thread-a","text":"<working>","last_inbound":90});
    sqlx::query("INSERT INTO bridge_lifecycle(key,report_id,job_id,purpose,payload,state,created_at,updated_at) VALUES ('command-a',1,7,'Working',?,'uncertain',90,95)")
        .bind(payload.to_string()).execute(&s.intake_sql).await.unwrap();
    let expected = Some(vec![
        json!({"key":"command-a","report_id":1,"job_id":7,"purpose":"Working","payload":payload,"state":"uncertain","created_at":90,"updated_at":95}),
    ]);
    assert_eq!(read.lifecycle_deliveries().await.unwrap(), expected);
    assert_eq!(read.lifecycle_deliveries().await.unwrap(), expected);
    let (session, _) = s.session().await;
    let (status, _, html) = call(&s.app, "GET", "/deliveries", &session, "").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(html.contains("report follow-ups"));
    assert!(html.contains("&lt;working&gt;"));
    assert!(html.contains("never automatically resent"));
    assert!(!html.contains("Resend job 7"));
}
#[tokio::test]
async fn job_projection_replays_events_in_order_and_reports_effects_budget_and_deadline() {
    let s = Setup::new(false).await;
    s.job(1).await;
    let read = sources(&s).await;
    let before = read.job(1).await.unwrap();
    assert_eq!(before.state, "Picked");
    assert_eq!(before.revision, 0);
    assert_eq!(before.summary["deadline"], 30);
    assert!(before.events.is_empty());
    assert!(before.intents.is_empty());
    let lease = s.engine.acquire_lease(1, "worker", 0, 100).await.unwrap();
    s.engine
        .apply_event(
            &lease,
            0,
            "selected",
            Event::SelectRelease {
                release: 88,
                access: Access::Granted {
                    library: 7,
                    expires_at: 200,
                },
            },
            1,
        )
        .await
        .unwrap();
    let after = read.job(1).await.unwrap();
    assert_eq!(after.state, "GrabIntent");
    assert_eq!(after.revision, 1);
    assert_eq!(after.id, 1);
    assert_eq!(after.summary["deadline"], 31);
    assert_eq!(after.summary["reserved_grabs"], 1);
    assert_eq!(after.summary["original"], json!({"id":4,"revision":1}));
    assert_eq!(after.summary["resource"], "media-1");
    assert_eq!(
        after.events,
        vec![
            json!({"revision":1,"at":1,"event":{"SelectRelease":{"release":88,"access":{"Granted":{"library":7,"expires_at":200}}}}})
        ]
    );
    assert_eq!(
        after.intents,
        vec![
            json!({"sequence":1,"status":"pending","intent":{"job":1,"sequence":1,"effect":{"Grab":{"release":88,"reservation":1}}}})
        ]
    );
    assert_eq!(read.jobs().await.unwrap(), vec![after]);
    assert!(matches!(read.job(2).await, Err(Error::Missing)));
    let budget = read.budgets(100_000).await.unwrap();
    assert_eq!(
        budget,
        vec![json!({"job":1,"viewer":"viewer-a","at":1,"units":2,"active":1})]
    );
    sqlx::query("DELETE FROM ownership")
        .execute(&s.engine_sql)
        .await
        .unwrap();
    assert_eq!(read.budgets(86_400).await.unwrap().len(), 1);
    assert!(read.budgets(86_401).await.unwrap().is_empty());
    assert_eq!(read.budgets(0).await.unwrap()[0]["active"], 0);
}
#[tokio::test]
async fn corrupt_event_journals_fail_closed_instead_of_showing_a_state() {
    for mutation in [
        "UPDATE jobs SET revision=1",
        "UPDATE jobs SET spec=json_set(spec,'$.id',2)",
        "UPDATE jobs SET spec='bad'",
        "UPDATE jobs SET spec=json_set(spec,'$.policy.total_timeout',0)",
    ] {
        let s = Setup::new(false).await;
        s.job(1).await;
        sqlx::query(mutation).execute(&s.engine_sql).await.unwrap();
        assert!(
            matches!(sources(&s).await.job(1).await, Err(Error::Storage)),
            "{mutation}"
        );
    }
    for (revision, event, at) in [
        (2, json!("Tick"), 1),
        (
            1,
            json!({"Observed":{"intent":{"job":9,"sequence":1},"observation":"ReadyNotified"}}),
            1,
        ),
        (1, json!("Tick"), -1),
    ] {
        let s = Setup::new(false).await;
        s.job(1).await;
        sqlx::query("UPDATE jobs SET revision=1")
            .execute(&s.engine_sql)
            .await
            .unwrap();
        sqlx::query("INSERT INTO events VALUES(1,?,'corrupt',?,?)")
            .bind(revision)
            .bind(at)
            .bind(event.to_string())
            .execute(&s.engine_sql)
            .await
            .unwrap();
        assert!(sources(&s).await.job(1).await.is_err());
    }
}
#[tokio::test]
async fn queue_does_not_turn_clarifications_or_followups_into_reports_regression() {
    let s = Setup::new(false).await;
    s.report().await;
    for outcome in ["clarify", "follow_up", "unauthorized"] {
        sqlx::query("INSERT INTO web_intakes(channel,message,account,conversation,response,created) VALUES('test',?,'1001','a',?,100)").bind(outcome).bind(json!({"outcome":outcome,"issues":[]}).to_string()).execute(&s.intake_sql).await.unwrap();
    }
    let read = sources(&s).await;
    assert_eq!(read.queue().await.unwrap().reports.len(), 1);
    for report in 2..=4 {
        assert!(matches!(read.report(report).await, Err(Error::Missing)));
        assert!(
            read.validate_command(&Command::Close { report }, 100)
                .await
                .is_err()
        );
    }
}
#[tokio::test]
async fn unrelated_observations_do_not_hide_recorded_health_regression() {
    let s = Setup::new(false).await;
    s.engine
        .ingest_observation(
            "admin:health",
            "health-a",
            &json!({"component":"agent","status":"up"}),
            90,
        )
        .await
        .unwrap();
    for i in 0..100 {
        s.engine
            .ingest_observation(
                "diff",
                &format!("diff-{i}"),
                &json!({"correlated":false}),
                100,
            )
            .await
            .unwrap();
    }
    assert!(
        sources(&s)
            .await
            .observations()
            .await
            .unwrap()
            .iter()
            .any(|r| r["source"] == "admin:health")
    );
}
#[tokio::test]
async fn approvals_require_current_diagnosis_and_no_attached_job() {
    let s = Setup::new(false).await;
    s.report().await;
    let read = sources(&s).await;
    assert!(
        read.validate_command(
            &Command::Approve {
                report: 1,
                decided_at: 95
            },
            100
        )
        .await
        .is_err()
    );
    s.seams().await;
    for command in [
        Command::Approve {
            report: 1,
            decided_at: 95,
        },
        Command::Deny {
            report: 1,
            decided_at: 95,
        },
    ] {
        read.validate_command(&command, 100).await.unwrap();
    }
    for decided_at in [94, 96] {
        assert!(matches!(
            read.validate_command(
                &Command::Approve {
                    report: 1,
                    decided_at
                },
                100
            )
            .await,
            Err(Error::Conflict)
        ));
    }
    sqlx::query("INSERT INTO report_progress(report_id,job_id,state,updated_at,viewer_copy_key) VALUES(1,2,'working',100,'working')")
        .execute(&s.intake_sql)
        .await
        .unwrap();
    assert!(matches!(
        read.validate_command(
            &Command::Approve {
                report: 1,
                decided_at: 95
            },
            100
        )
        .await,
        Err(Error::Conflict)
    ));
    let (cookie, _) = s.session().await;
    assert!(
        !call(&s.app, "GET", "/reports/1", &cookie, "")
            .await
            .2
            .contains("Request fix approval")
    );
}
#[tokio::test]
async fn advice_only_diagnosis_is_not_a_pending_fix_approval() {
    let s = Setup::new(false).await;
    s.report().await;
    s.seams().await;
    sqlx::query("UPDATE diagnoses SET decision=?")
        .bind(json!({"action":"client_advice"}).to_string())
        .execute(&s.intake_sql)
        .await
        .unwrap();
    let read = sources(&s).await;
    assert!(matches!(
        read.validate_command(
            &Command::Approve {
                report: 1,
                decided_at: 95
            },
            100
        )
        .await,
        Err(Error::Conflict)
    ));
    let (cookie, _) = s.session().await;
    let body = call(&s.app, "GET", "/reports/1", &cookie, "").await.2;
    assert!(!body.contains("Request fix approval"));
}
#[tokio::test]
async fn exclusion_and_capability_projections_and_command_guards_follow_saved_state() {
    let s = Setup::new(false).await;
    s.exclusion().await;
    s.capabilities().await;
    let read = sources(&s).await;
    let rows = read.exclusions().await.unwrap();
    assert_eq!(rows[0]["id"], 1);
    assert_eq!(rows[0]["spec"]["reason"], "Decode failed");
    assert_eq!(rows[0]["pinned"], 0);
    assert_eq!(rows[0]["released_at"], serde_json::Value::Null);
    assert_eq!(
        read.capabilities().await.unwrap()[0]["ledger"]["version"],
        "1.0"
    );
    read.validate_command(
        &Command::Reset {
            client: "arr-a".into(),
        },
        100,
    )
    .await
    .unwrap();
    assert!(matches!(
        read.validate_command(
            &Command::Reset {
                client: "unknown".into()
            },
            100
        )
        .await,
        Err(Error::Missing)
    ));
    let expiry = rows[0]["expires_at"].as_u64().unwrap();
    for command in [Command::Release { id: 1 }, Command::Pin { id: 1 }] {
        read.validate_command(&command, expiry - 1).await.unwrap();
        assert!(read.validate_command(&command, expiry).await.is_err());
    }
    sqlx::query("UPDATE release_exclusions SET pinned=1")
        .execute(&s.engine_sql)
        .await
        .unwrap();
    read.validate_command(&Command::Release { id: 1 }, expiry + 100)
        .await
        .unwrap();
    sqlx::query("UPDATE release_exclusions SET released_at=100")
        .execute(&s.engine_sql)
        .await
        .unwrap();
    assert!(
        read.validate_command(&Command::Release { id: 1 }, 100)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn lower_quality_request_is_bound_to_the_pending_job_revision() {
    let s = Setup::new(false).await;
    s.job(1).await;
    let lease = s.engine.acquire_lease(1, "worker", 0, 100).await.unwrap();
    s.engine
        .apply_event(
            &lease,
            0,
            "broken",
            Event::BrokenFileConfirmed {
                file: spec(1).original,
            },
            1,
        )
        .await
        .unwrap();
    s.engine
        .apply_event(
            &lease,
            1,
            "request-quality",
            Event::RequestLowerQuality {
                candidate: ReleaseCandidate {
                    release: 41,
                    identity: Default::default(),
                    quality: QualityRelation::Lower,
                },
                access: Access::Granted {
                    library: 7,
                    expires_at: 200,
                },
            },
            2,
        )
        .await
        .unwrap();
    let read = sources(&s).await;
    read.validate_command(
        &Command::LowerQuality {
            job: 1,
            revision: 2,
        },
        3,
    )
    .await
    .unwrap();
    for revision in [1, 3] {
        assert!(matches!(
            read.validate_command(&Command::LowerQuality { job: 1, revision }, 3)
                .await,
            Err(Error::Conflict)
        ));
    }
    let (cookie, csrf) = s.session().await;
    let page = call(&s.app, "GET", "/jobs/1", &cookie, "").await.2;
    assert!(page.contains("Request lower-quality approval"));
    assert_eq!(
        call(
            &s.app,
            "POST",
            "/actions",
            &cookie,
            &action(
                &csrf,
                "quality-a",
                &Command::LowerQuality {
                    job: 1,
                    revision: 2
                },
                "Reviewed exact broken copy"
            )
        )
        .await
        .0,
        303
    );
    assert_eq!(
        s.app.journal.requests().await.unwrap()[0]["state"],
        "pending_controller"
    );
    assert_eq!(
        s.engine.load_job(1).await.unwrap().job.state(),
        State::AwaitingQualityApproval
    );
}
