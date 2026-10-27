use super::*;
use crate::{BudgetLimits, NewJob};
use pitcrew_domain::{Constraints, Event, Failure, Policy, StageTimeouts, media::InfoHash};

async fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("policy.db")).await.unwrap();
    (dir, store)
}

#[tokio::test]
async fn effective_pin_preserves_spec_and_requires_current_admin_policy() {
    let (_temp, store) = setup().await;
    let spec = exclusion();
    let saved = store.exclude_release(&spec, "pin-a").await.unwrap();
    let mut viewer = admin();
    viewer.role = PolicyRole::Viewer;
    assert!(matches!(
        store.pin_exclusion(saved.id, &viewer, 100).await,
        Err(StoreError::AdminRequired)
    ));
    assert!(matches!(
        store.pin_exclusion(saved.id, &admin(), 99).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(store.pin_exclusion(saved.id, &admin(), 100).await.unwrap());
    assert!(!store.pin_exclusion(saved.id, &admin(), 101).await.unwrap());
    let current = store
        .exclusions(&spec.client, saved.expires_at)
        .await
        .unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].spec, spec);
    assert!(!current[0].spec.pinned);
    let audit = store.policy_audit().await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "pin_exclusion");
    assert_eq!(audit[0].actor, "operator");
    assert_eq!(audit[0].subject, saved.id.to_string());
    assert_eq!(audit[0].note, "Reviewed evidence");
    assert_eq!(audit[0].at, 100);
    store
        .release_exclusion(saved.id, &admin(), 102)
        .await
        .unwrap();
    assert!(matches!(
        store.pin_exclusion(saved.id, &admin(), 103).await,
        Err(StoreError::InvalidInput)
    ));
    let expired = store.exclude_release(&spec, "pin-b").await.unwrap();
    assert!(matches!(
        store
            .pin_exclusion(expired.id, &admin(), expired.expires_at)
            .await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.pin_exclusion(999, &admin(), 100).await,
        Err(StoreError::Missing)
    ));
}

fn admin() -> AdminAction {
    AdminAction {
        actor: "operator".into(),
        role: PolicyRole::Admin,
        note: "Reviewed evidence".into(),
    }
}

fn exclusion() -> NewExclusion {
    NewExclusion {
        client: "arr-a".into(),
        release: 17,
        identity: ReleaseIdentity {
            infohash: Some(InfoHash("aBcD".into())),
            ..Default::default()
        },
        kind: ExclusionKind::DeadRelease,
        reason: "Source unavailable".into(),
        evidence: serde_json::json!({"attempts": 2}),
        pinned: false,
        created_at: 100,
    }
}

fn fingerprint() -> IdentityFingerprint {
    IdentityFingerprint {
        arr_file_id: 7,
        size: 8192,
        date_added: "2026-01-01T00:00:00Z".into(),
        quality: "HD".into(),
        agent: FileIdentity {
            id: 83,
            revision: 10,
        },
    }
}

fn caps() -> ClientCapabilities {
    ClientCapabilities {
        external_queue_actor: true,
        held_category: Some("verified-hold".into()),
    }
}

fn job(id: u64, at: u64) -> NewJob {
    NewJob {
        id,
        viewer: "viewer".into(),
        resource: "film-7".into(),
        library: 1,
        original: FileIdentity { id: 7, revision: 1 },
        constraints: Constraints::default(),
        policy: Policy {
            max_grabs: 1,
            stage_timeout: 30,
            total_timeout: 300,
        },
        timeouts: StageTimeouts {
            download: 60,
            import: 40,
            decode: 50,
            verify_retry: 20,
        },
        created_at: at,
    }
}

fn limits() -> BudgetLimits {
    BudgetLimits {
        global_hour: 20,
        global_day: 20,
        viewer_hour: 20,
        viewer_day: 20,
    }
}

#[tokio::test]
async fn exclusions_persist_expire_at_exact_boundary_and_match_all_release_aliases() {
    let (dir, store) = setup().await;
    let spec = exclusion();
    let row = store.exclude_release(&spec, "failure-1").await.unwrap();
    assert_eq!(row.spec, spec);
    assert_eq!(row.expires_at, 100 + 30 * 86400);
    assert_eq!(row.released_at, None);
    assert_eq!(
        store.exclude_release(&spec, "failure-1").await.unwrap(),
        row
    );
    let mut changed = spec.clone();
    changed.reason = "different reason".into();
    assert!(matches!(
        store.exclude_release(&changed, "failure-1").await,
        Err(StoreError::IdempotencyConflict)
    ));
    store.pool.close().await;
    let store = Store::open(dir.path().join("policy.db")).await.unwrap();
    assert_eq!(
        store.exclusions("arr-a", 100).await.unwrap(),
        vec![row.clone()]
    );
    assert!(store.exclusions("arr-b", 100).await.unwrap().is_empty());
    assert!(
        store
            .release_is_excluded("arr-a", 17, &ReleaseIdentity::default(), row.expires_at - 1)
            .await
            .unwrap()
    );
    assert!(
        store
            .release_is_excluded(
                "arr-a",
                18,
                &ReleaseIdentity {
                    infohash: Some(InfoHash("ABCD".into())),
                    ..Default::default()
                },
                100
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .release_is_excluded("arr-b", 17, &spec.identity, 100)
            .await
            .unwrap()
    );
    assert!(
        !store
            .release_is_excluded("arr-a", 18, &ReleaseIdentity::default(), 100)
            .await
            .unwrap()
    );
    assert!(
        store
            .exclusions("arr-a", row.expires_at)
            .await
            .unwrap()
            .is_empty()
    );
    changed = spec;
    changed.pinned = true;
    changed.kind = ExclusionKind::Quarantine;
    let pinned = store.exclude_release(&changed, "pinned").await.unwrap();
    assert_eq!(
        store
            .exclusions("arr-a", pinned.expires_at + 1)
            .await
            .unwrap(),
        vec![pinned]
    );
}

#[tokio::test]
async fn releasing_requires_a_named_admin_with_note_and_is_audited_once() {
    let (_, store) = setup().await;
    let row = store
        .exclude_release(&exclusion(), "failure")
        .await
        .unwrap();
    for role in [PolicyRole::Viewer, PolicyRole::Unknown] {
        let mut actor = admin();
        actor.role = role;
        assert!(matches!(
            store.release_exclusion(row.id, &actor, 101).await,
            Err(StoreError::AdminRequired)
        ));
    }
    for (actor, note) in [(" ", "reviewed"), ("admin", "\n ")] {
        let action = AdminAction {
            actor: actor.into(),
            role: PolicyRole::Admin,
            note: note.into(),
        };
        assert!(matches!(
            store.release_exclusion(row.id, &action, 101).await,
            Err(StoreError::InvalidInput)
        ));
    }
    assert!(store.policy_audit().await.unwrap().is_empty());
    assert!(
        store
            .release_exclusion(row.id, &admin(), 101)
            .await
            .unwrap()
    );
    assert!(
        !store
            .release_exclusion(row.id, &admin(), 102)
            .await
            .unwrap()
    );
    assert!(store.exclusions("arr-a", 102).await.unwrap().is_empty());
    let audit = store.policy_audit().await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "operator");
    assert_eq!(audit[0].note, "Reviewed evidence");
    assert_eq!(audit[0].action, "release_exclusion");
    assert_eq!(audit[0].subject, row.id.to_string());
    assert_eq!(audit[0].at, 101);
    assert!(matches!(
        store.release_exclusion(999, &admin(), 102).await,
        Err(StoreError::Missing)
    ));
}

#[tokio::test]
async fn capabilities_are_durable_version_bound_and_reset_by_admin() {
    let (dir, store) = setup().await;
    assert_eq!(store.capabilities("arr-a", "4.0.19").await.unwrap(), None);
    let initial = store
        .observe_client_version("arr-a", "4.0.19", 10)
        .await
        .unwrap();
    assert_eq!(initial.client, "arr-a");
    assert_eq!(initial.version, "4.0.19");
    assert_eq!(initial.capabilities, None);
    assert_eq!(initial.updated_at, 10);
    let evidence = serde_json::json!({"contract":"queue-actor"});
    let recorded = store
        .record_capabilities("arr-a", "4.0.19", &caps(), &evidence, 11)
        .await
        .unwrap();
    assert_eq!(recorded.capabilities, Some(caps()));
    assert_eq!(recorded.evidence, evidence);
    assert_eq!(recorded.updated_at, 11);
    assert_eq!(
        store
            .observe_client_version("arr-a", "4.0.19", 12)
            .await
            .unwrap(),
        recorded
    );
    store.pool.close().await;
    let store = Store::open(dir.path().join("policy.db")).await.unwrap();
    assert_eq!(
        store.capabilities("arr-a", "4.0.19").await.unwrap(),
        Some(recorded)
    );
    assert_eq!(store.capabilities("arr-a", "4.1.0").await.unwrap(), None);
    let reset = store
        .observe_client_version("arr-a", "4.1.0", 12)
        .await
        .unwrap();
    assert_eq!(reset.capabilities, None);
    assert_eq!(reset.evidence, serde_json::Value::Null);
    assert_eq!(store.capabilities("arr-a", "4.0.19").await.unwrap(), None);
    assert!(matches!(
        store
            .record_capabilities("arr-a", "4.0.19", &caps(), &evidence, 13)
            .await,
        Err(StoreError::RevisionConflict)
    ));
    store
        .record_capabilities("arr-a", "4.1.0", &caps(), &evidence, 13)
        .await
        .unwrap();
    let mut viewer = admin();
    viewer.role = PolicyRole::Viewer;
    assert!(matches!(
        store.reset_capabilities("arr-a", &viewer, 14).await,
        Err(StoreError::AdminRequired)
    ));
    store
        .reset_capabilities("arr-a", &admin(), 14)
        .await
        .unwrap();
    let reset = store.capabilities("arr-a", "4.1.0").await.unwrap().unwrap();
    assert_eq!(reset.capabilities, None);
    assert_eq!(reset.updated_at, 14);
    assert_eq!(
        store.policy_audit().await.unwrap()[0].action,
        "reset_capabilities"
    );
    assert_eq!(store.policy_audit().await.unwrap()[0].subject, "arr-a");
    assert!(matches!(
        store.reset_capabilities("missing", &admin(), 15).await,
        Err(StoreError::Missing)
    ));
}

#[tokio::test]
async fn file_registry_mints_opaque_revisions_and_rejects_every_stale_or_changed_component() {
    let (dir, store) = setup().await;
    let original = fingerprint();
    let file = store
        .register_file("arr-a", "film-7", &original, 10)
        .await
        .unwrap();
    assert_eq!(file.id, original.arr_file_id);
    assert!(file.revision > 0);
    assert_eq!(
        store
            .register_file("arr-a", "film-7", &original, 11)
            .await
            .unwrap(),
        file
    );
    store.pool.close().await;
    let store = Store::open(dir.path().join("policy.db")).await.unwrap();
    store
        .validate_file("arr-a", "film-7", file, &original, 12, 1)
        .await
        .unwrap();
    for (client, resource, expected, now, age) in [
        ("arr-b", "film-7", file, 12, 1),
        ("arr-a", "film-8", file, 12, 1),
        ("arr-a", "film-7", FileIdentity { id: 8, ..file }, 12, 1),
        (
            "arr-a",
            "film-7",
            FileIdentity {
                revision: file.revision + 1,
                ..file
            },
            12,
            1,
        ),
        ("arr-a", "film-7", file, 13, 1),
        ("arr-a", "film-7", file, 10, 1),
    ] {
        assert!(matches!(
            store
                .validate_file(client, resource, expected, &original, now, age)
                .await,
            Err(StoreError::IdentityMismatch)
        ));
    }
    let mut changes = vec![original.clone(); 6];
    changes[0].arr_file_id += 1;
    changes[1].size += 1;
    changes[2].date_added.push('X');
    changes[3].quality.push('X');
    changes[4].agent.id += 1;
    changes[5].agent.revision += 1;
    for (i, changed) in changes.iter().enumerate() {
        assert!(matches!(
            store
                .validate_file("arr-a", "film-7", file, changed, 12, 1)
                .await,
            Err(StoreError::IdentityMismatch)
        ));
        let next = store
            .register_file("arr-a", "film-7", changed, 20 + i as u64)
            .await
            .unwrap();
        assert_ne!(next, file);
        store
            .validate_file("arr-a", "film-7", next, changed, 20 + i as u64, 0)
            .await
            .unwrap();
        assert!(matches!(
            store
                .validate_file("arr-a", "film-7", file, &original, 20 + i as u64, 100)
                .await,
            Err(StoreError::IdentityMismatch)
        ));
    }
}

#[tokio::test]
async fn item_cooldown_is_atomic_with_creation_and_budget_and_cannot_be_bypassed() {
    let (dir, store) = setup().await;
    let first = job(1, 100);
    let created = store
        .create_job_with_cooldown(&first, "first", limits(), 10)
        .await
        .unwrap();
    assert_eq!(
        store
            .create_job_with_cooldown(&first, "first", limits(), 10)
            .await
            .unwrap(),
        created
    );
    let lease = store.acquire_lease(1, "worker", 100, 30).await.unwrap();
    store
        .apply_event(&lease, 0, "done", Event::Fail(Failure::Cancelled), 101)
        .await
        .unwrap();
    assert!(matches!(
        store.create_job(&job(2, 109), "second", limits()).await,
        Err(StoreError::ItemCooldown { until: 110 })
    ));
    assert_eq!(store.jobs().await.unwrap(), vec![1]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM budget_reservations")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        1
    );
    store.pool.close().await;
    let store = Store::open(dir.path().join("policy.db")).await.unwrap();
    assert!(matches!(
        store.create_job(&job(2, 109), "second", limits()).await,
        Err(StoreError::ItemCooldown { until: 110 })
    ));
    store
        .create_job_with_cooldown(&job(2, 110), "second", limits(), 10)
        .await
        .unwrap();
    let lease = store.acquire_lease(2, "worker", 110, 30).await.unwrap();
    store
        .apply_event(&lease, 0, "done", Event::Fail(Failure::Cancelled), 111)
        .await
        .unwrap();
    assert!(matches!(
        store.create_job(&job(3, 119), "third", limits()).await,
        Err(StoreError::ItemCooldown { until: 120 })
    ));
    store
        .create_job(&job(3, 120), "third", limits())
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_exclusions_and_backwards_release_leave_no_state_or_audit() {
    let (_, store) = setup().await;
    let mut bad = vec![exclusion(); 6];
    bad[0].client = " ".into();
    bad[1].reason = "\t".into();
    bad[2].release = 0;
    bad[3].evidence = Value::Null;
    bad[4].created_at = u64::MAX;
    bad[5].created_at = i64::MAX as u64;
    for input in bad {
        assert!(matches!(
            store.exclude_release(&input, "invalid").await,
            Err(StoreError::InvalidInput)
        ));
    }
    assert!(matches!(
        store.exclude_release(&exclusion(), " ").await,
        Err(StoreError::InvalidInput)
    ));
    assert!(store.exclusions("arr-a", 100).await.unwrap().is_empty());
    assert!(matches!(
        store.exclusions(" ", 100).await,
        Err(StoreError::InvalidInput)
    ));
    let row = store.exclude_release(&exclusion(), "valid").await.unwrap();
    assert!(matches!(
        store.release_exclusion(row.id, &admin(), 99).await,
        Err(StoreError::InvalidInput)
    ));
    assert_eq!(
        store.exclusions("arr-a", 100).await.unwrap(),
        vec![row.clone()]
    );
    assert!(store.policy_audit().await.unwrap().is_empty());
    assert!(
        store
            .release_exclusion(row.id, &admin(), 100)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .exclude_release(&exclusion(), "valid")
            .await
            .unwrap()
            .released_at,
        Some(100)
    );
    for statement in [
        "DELETE FROM policy_audit",
        "UPDATE policy_audit SET note='changed'",
    ] {
        assert!(sqlx::query(statement).execute(&store.pool).await.is_err());
    }
}

#[tokio::test]
async fn capability_validation_and_clock_regression_do_not_erase_the_last_known_ledger() {
    let (_, store) = setup().await;
    let evidence = serde_json::json!({"contract":"checked"});
    for (client, version) in [(" ", "v1"), ("a", " ")] {
        assert!(matches!(
            store.observe_client_version(client, version, 10).await,
            Err(StoreError::InvalidInput)
        ));
        assert!(matches!(
            store.capabilities(client, version).await,
            Err(StoreError::InvalidInput)
        ));
        assert!(matches!(
            store
                .record_capabilities(client, version, &caps(), &evidence, 10)
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
    assert!(matches!(
        store.observe_client_version("a", "v1", u64::MAX).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store
            .record_capabilities("missing", "v1", &caps(), &evidence, 10)
            .await,
        Err(StoreError::Missing)
    ));
    let observed = store.observe_client_version("a", "v1", 10).await.unwrap();
    assert_eq!(
        store.observe_client_version("a", "v1", 10).await.unwrap(),
        observed
    );
    assert!(matches!(
        store.observe_client_version("a", "v1", 9).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.observe_client_version("a", "v2", 9).await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store
            .record_capabilities("a", "v1", &caps(), &Value::Null, 10)
            .await,
        Err(StoreError::InvalidInput)
    ));
    let mut bad = caps();
    bad.held_category = Some(" ".into());
    assert!(matches!(
        store
            .record_capabilities("a", "v1", &bad, &evidence, 10)
            .await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store
            .record_capabilities("a", "v1", &caps(), &evidence, 9)
            .await,
        Err(StoreError::InvalidInput)
    ));
    assert!(matches!(
        store.reset_capabilities("a", &admin(), 9).await,
        Err(StoreError::InvalidInput)
    ));
    assert_eq!(store.capabilities("a", "v1").await.unwrap(), Some(observed));
    let plain = ClientCapabilities {
        external_queue_actor: false,
        held_category: None,
    };
    let recorded = store
        .record_capabilities("a", "v1", &plain, &evidence, 10)
        .await
        .unwrap();
    assert_eq!(recorded.capabilities, Some(plain));
    store.reset_capabilities("a", &admin(), 10).await.unwrap();
    assert_eq!(
        store
            .capabilities("a", "v1")
            .await
            .unwrap()
            .unwrap()
            .capabilities,
        None
    );
    assert_eq!(store.policy_audit().await.unwrap()[0].id, 1);
}

#[tokio::test]
async fn identity_validation_rejects_incomplete_tuples_and_backwards_refreshes() {
    let (_, store) = setup().await;
    let mut bad = vec![fingerprint(); 6];
    bad[0].arr_file_id = 0;
    bad[1].size = 0;
    bad[2].agent.id = 0;
    bad[3].agent.revision = 0;
    bad[4].date_added = " ".into();
    bad[5].quality = "\n".into();
    for input in bad {
        assert!(matches!(
            store.register_file("a", "item", &input, 10).await,
            Err(StoreError::InvalidInput)
        ));
    }
    for (client, resource) in [(" ", "item"), ("a", " ")] {
        assert!(matches!(
            store
                .register_file(client, resource, &fingerprint(), 10)
                .await,
            Err(StoreError::InvalidInput)
        ));
    }
    let original = fingerprint();
    let file = store
        .register_file("a", "item", &original, 10)
        .await
        .unwrap();
    assert_eq!(
        store
            .register_file("a", "item", &original, 10)
            .await
            .unwrap(),
        file
    );
    assert!(matches!(
        store.register_file("a", "item", &original, 9).await,
        Err(StoreError::InvalidInput)
    ));
    let mut changed = original.clone();
    changed.size += 1;
    assert!(matches!(
        store.register_file("a", "item", &changed, 9).await,
        Err(StoreError::InvalidInput)
    ));
    store
        .validate_file("a", "item", file, &original, 10, 0)
        .await
        .unwrap();
    let second = store
        .register_file("a", "item", &changed, 11)
        .await
        .unwrap();
    let third = store
        .register_file("a", "item", &original, 12)
        .await
        .unwrap();
    assert_ne!(third, second);
    assert_ne!(third, file);
    let other = store
        .register_file("b", "item", &original, 12)
        .await
        .unwrap();
    assert_ne!(other, third);
    assert!(matches!(
        store
            .validate_file("a", "item", other, &original, 12, 0)
            .await,
        Err(StoreError::IdentityMismatch)
    ));
}

#[tokio::test]
async fn failed_budget_and_overflow_do_not_reserve_cooldown() {
    let (_, store) = setup().await;
    let cap = BudgetLimits {
        global_hour: 0,
        ..limits()
    };
    assert!(matches!(
        store
            .create_job_with_cooldown(&job(1, 10), "first", cap, 10)
            .await,
        Err(StoreError::BudgetExhausted)
    ));
    assert!(matches!(
        store
            .create_job_with_cooldown(&job(1, 10), "first", limits(), u64::MAX)
            .await,
        Err(StoreError::InvalidInput)
    ));
    assert!(store.jobs().await.unwrap().is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM item_cooldowns")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0
    );
    store
        .create_job_with_cooldown(&job(1, 10), "first", limits(), 10)
        .await
        .unwrap();
}

#[tokio::test]
async fn competing_connections_cannot_restore_stale_capabilities_or_duplicate_release_audit() {
    let (dir, first) = setup().await;
    let second = Store::open(dir.path().join("policy.db")).await.unwrap();
    first
        .observe_client_version("arr-a", "v1", 10)
        .await
        .unwrap();
    let capabilities = caps();
    let evidence = serde_json::json!({"contract":"v1-observation"});
    let (version, observation) = tokio::join!(
        first.observe_client_version("arr-a", "v2", 11),
        second.record_capabilities("arr-a", "v1", &capabilities, &evidence, 11),
    );
    assert_eq!(version.unwrap().capabilities, None);
    assert!(observation.is_ok() || matches!(observation, Err(StoreError::RevisionConflict)));
    assert_eq!(first.capabilities("arr-a", "v1").await.unwrap(), None);
    let current = first.capabilities("arr-a", "v2").await.unwrap().unwrap();
    assert_eq!(current.capabilities, None);
    assert_eq!(current.evidence, Value::Null);

    let row = first
        .exclude_release(&exclusion(), "failure")
        .await
        .unwrap();
    let operator = admin();
    let (a, b) = tokio::join!(
        first.release_exclusion(row.id, &operator, 101),
        second.release_exclusion(row.id, &operator, 101),
    );
    assert_ne!(a.unwrap(), b.unwrap());
    assert_eq!(first.policy_audit().await.unwrap().len(), 1);
    assert!(first.exclusions("arr-a", 101).await.unwrap().is_empty());
}

#[tokio::test]
async fn zero_cooldown_keeps_ordinary_recorded_job_creation_compatible() {
    let (_, store) = setup().await;
    store
        .create_job(&job(1, 100), "first", limits())
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "worker", 100, 30).await.unwrap();
    store
        .apply_event(&lease, 0, "done", Event::Fail(Failure::Cancelled), 101)
        .await
        .unwrap();
    // Read-only replays can be imported out of original creation order.
    store
        .create_job(&job(2, 99), "second", limits())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM item_cooldowns")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn capability_downgrades_survive_clean_observations_until_admin_or_version_reset() {
    let (dir, store) = setup().await;
    store
        .observe_client_version("arr-a", "v1", 10)
        .await
        .unwrap();
    let clean = ClientCapabilities {
        external_queue_actor: false,
        held_category: Some("certified-hold".into()),
    };
    let removed = ClientCapabilities {
        external_queue_actor: true,
        held_category: None,
    };
    let evidence = serde_json::json!({"observed":"queue-actor"});
    store
        .record_capabilities("arr-a", "v1", &clean, &evidence, 11)
        .await
        .unwrap();
    store
        .record_capabilities("arr-a", "v1", &removed, &evidence, 12)
        .await
        .unwrap();
    let retained = store
        .record_capabilities("arr-a", "v1", &clean, &evidence, 13)
        .await
        .unwrap();
    assert_eq!(retained.capabilities, Some(removed));
    store.pool.close().await;
    let store = Store::open(dir.path().join("policy.db")).await.unwrap();
    assert_eq!(store.client_ledger("arr-a").await.unwrap(), Some(retained));
    assert_eq!(store.client_ledger("missing").await.unwrap(), None);
    assert!(matches!(
        store.client_ledger(" ").await,
        Err(StoreError::InvalidInput)
    ));
    store
        .reset_capabilities("arr-a", &admin(), 14)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_capabilities("arr-a", "v1", &clean, &evidence, 15)
            .await
            .unwrap()
            .capabilities,
        Some(clean.clone())
    );
    let no_hold = ClientCapabilities {
        external_queue_actor: false,
        held_category: None,
    };
    store
        .record_capabilities("arr-a", "v1", &no_hold, &evidence, 16)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_capabilities("arr-a", "v1", &clean, &evidence, 17)
            .await
            .unwrap()
            .capabilities,
        Some(no_hold)
    );
    store
        .observe_client_version("arr-a", "v2", 18)
        .await
        .unwrap();
    assert_eq!(
        store
            .record_capabilities("arr-a", "v2", &clean, &evidence, 19)
            .await
            .unwrap()
            .capabilities,
        Some(clean)
    );
}

#[tokio::test]
async fn capability_observations_combine_independent_risks_and_only_preserve_identical_holds() {
    let (_, store) = setup().await;
    let evidence = serde_json::json!({"contract":"synthetic"});
    for old_actor in [false, true] {
        for new_actor in [false, true] {
            for old_hold in [None, Some("hold-a"), Some("hold-b")] {
                for new_hold in [None, Some("hold-a"), Some("hold-b")] {
                    let client = format!("arr-{old_actor}-{new_actor}-{old_hold:?}-{new_hold:?}");
                    store
                        .observe_client_version(&client, "v1", 10)
                        .await
                        .unwrap();
                    let old = ClientCapabilities {
                        external_queue_actor: old_actor,
                        held_category: old_hold.map(str::to_owned),
                    };
                    let new = ClientCapabilities {
                        external_queue_actor: new_actor,
                        held_category: new_hold.map(str::to_owned),
                    };
                    store
                        .record_capabilities(&client, "v1", &old, &evidence, 11)
                        .await
                        .unwrap();
                    let observed = store
                        .record_capabilities(&client, "v1", &new, &evidence, 12)
                        .await
                        .unwrap();
                    assert_eq!(
                        observed.capabilities,
                        Some(ClientCapabilities {
                            external_queue_actor: old_actor || new_actor,
                            held_category: if old_hold == new_hold {
                                old.held_category
                            } else {
                                None
                            },
                        })
                    );
                }
            }
        }
    }
}
