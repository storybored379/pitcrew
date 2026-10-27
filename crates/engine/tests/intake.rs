use pitcrew_engine::webhook::{Intake, IntakeError, Kind, MAX_BODY, Secret, parse};
use pitcrew_store::Store;
use serde_json::{Value, json};

const ONE: &str = "first-instance-test-secret-0000001";
const TWO: &str = "second-instance-test-secret-000002";

#[test]
fn secrets_reject_invalid_bounds_and_compare_all_bytes() {
    for value in [
        "".to_owned(),
        "a".repeat(31),
        "a".repeat(4097),
        format!("{} ", "a".repeat(32)),
        "é".repeat(32),
        format!("{}\n", "a".repeat(32)),
    ] {
        assert!(matches!(
            Secret::new(&value),
            Err(IntakeError::Configuration)
        ));
    }
    for value in ["a".repeat(32), "z".repeat(4096), ONE.to_owned()] {
        let secret = Secret::new(&value).unwrap();
        assert!(secret.accepts(&value));
        assert!(!secret.accepts(""));
        assert!(!secret.accepts(&(value.clone() + "a")));
        let mut changed = value.into_bytes();
        changed[0] = b'x';
        assert!(!secret.accepts(std::str::from_utf8(&changed).unwrap()));
    }
}

#[test]
fn all_supported_webhooks_normalize_without_granting_authority() {
    for (name, expected) in [
        ("Grab", Kind::Grab),
        ("Download", Kind::Download),
        ("Import", Kind::Download),
        ("DownloadFailed", Kind::DownloadFailed),
        ("ManualInteractionRequired", Kind::ManualInteractionRequired),
        ("Rename", Kind::Rename),
        ("EpisodeFileDelete", Kind::EpisodeFileDelete),
        ("MovieFileDelete", Kind::MovieFileDelete),
        ("Health", Kind::Health),
        ("HealthRestored", Kind::Health),
        ("Test", Kind::Test),
    ] {
        let observation = parse(&json!({"eventType": name, "downloadId": "opaque-download", "movie": {"id": 3},
            "episodes": [{"id": 2}, {"id": 1}, {"id": 3}, {"id": 1}], "movieFile": {"id": 9}, "episodeFile": {"id": 9},
            "path": "/sensitive/example", "token": "untrusted-token", "access": true})).unwrap();
        assert_eq!(observation.kind, expected);
        assert_eq!(observation.download.as_deref(), Some("opaque-download"));
        assert_eq!(observation.media, vec![1, 2, 3]);
        assert_eq!(observation.files, vec![9]);
        let normalized = serde_json::to_value(observation).unwrap();
        assert_eq!(normalized.as_object().unwrap().len(), 4);
        assert!(normalized.get("path").is_none());
        assert!(normalized.get("token").is_none());
        assert!(normalized.get("access").is_none());
    }
    let test = parse(&json!({"eventType": "Test"})).unwrap();
    assert!(test.download.is_none());
    assert!(test.media.is_empty());
    assert!(test.files.is_empty());
    assert!(
        parse(&json!({"eventType": "Test", "downloadId": null}))
            .unwrap()
            .download
            .is_none()
    );
    assert_eq!(
        parse(&json!({"eventType": "Test", "downloadId": "d".repeat(256)}))
            .unwrap()
            .download
            .unwrap()
            .len(),
        256
    );
    let both = parse(&json!({"eventType": "Rename", "movieFile":{"id":8}, "episodeFile":{"id":7}}))
        .unwrap();
    assert_eq!(both.files, vec![7, 8]);
}

#[test]
fn malformed_schema_rejects_without_exposing_payload() {
    for body in [
        json!(null),
        json!([]),
        json!({}),
        json!({"eventType": "secret-unknown"}),
        json!({"eventType": 1}),
    ] {
        assert!(matches!(parse(&body), Err(IntakeError::Payload)));
    }
    for bad in [
        json!(""),
        json!("d".repeat(257)),
        json!(1),
        json!([]),
        json!({}),
    ] {
        assert!(matches!(
            parse(&json!({"eventType": "Grab", "downloadId": bad})),
            Err(IntakeError::Payload)
        ));
    }
    for key in ["movie", "movieFile", "episodeFile"] {
        for bad in [
            json!(null),
            json!(1),
            json!({}),
            json!({"id": 0}),
            json!({"id": -1}),
            json!({"id":"secret-value"}),
            json!({"id": 1.5}),
        ] {
            let mut value = json!({"eventType":"Download"});
            value[key] = bad;
            let error = parse(&value).unwrap_err();
            assert!(matches!(error, IntakeError::Payload));
            assert!(!error.to_string().contains("secret-value"));
        }
    }
    for bad in [
        json!(null),
        json!({}),
        json!("secret-value"),
        json!([{"id":0}]),
    ] {
        assert!(parse(&json!({"eventType":"Grab", "episodes":bad})).is_err());
    }
    assert_eq!(
        parse(&json!({"eventType":"Grab", "movie":{"id":u64::MAX}}))
            .unwrap()
            .media,
        vec![u64::MAX]
    );
}

async fn setup() -> (tempfile::TempDir, Store, Intake) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("events.sqlite")).await.unwrap();
    let intake = Intake::new(
        store.clone(),
        [
            ("movies_1".into(), Secret::new(ONE).unwrap()),
            ("series-2".into(), Secret::new(TWO).unwrap()),
        ],
    )
    .unwrap();
    (dir, store, intake)
}

#[tokio::test]
async fn configuration_is_fail_closed_for_invalid_or_duplicate_instances() {
    let (_dir, store, intake) = setup().await;
    for id in ["", "../escape", "contains space", "invalid/instance", "é"] {
        assert!(matches!(
            Intake::new(store.clone(), [(id.into(), Secret::new(ONE).unwrap())]),
            Err(IntakeError::Configuration)
        ));
    }
    assert!(matches!(
        Intake::new(
            store,
            [
                ("same".into(), Secret::new(ONE).unwrap()),
                ("same".into(), Secret::new(TWO).unwrap())
            ]
        ),
        Err(IntakeError::Configuration)
    ));
    assert!(intake.authorized("movies_1", ONE));
    assert!(intake.authorized("series-2", TWO));
    assert!(!intake.authorized("movies_1", TWO));
    assert!(!intake.authorized("missing", ONE));
}

#[tokio::test]
async fn auth_precedes_parsing_and_size_limits_and_rejected_input_never_persists() {
    let (_dir, store, intake) = setup().await;
    let oversized = vec![b' '; MAX_BODY + 1];
    assert!(matches!(
        intake.ingest("movies_1", TWO, &oversized, 1).await,
        Err(IntakeError::Unauthorized)
    ));
    assert!(matches!(
        intake.ingest("missing", ONE, b"invalid", 1).await,
        Err(IntakeError::Unauthorized)
    ));
    assert!(matches!(
        intake.ingest("movies_1", ONE, &oversized, 1).await,
        Err(IntakeError::TooLarge)
    ));
    assert!(matches!(
        intake
            .ingest("movies_1", ONE, b"secret-invalid-json", 1)
            .await,
        Err(IntakeError::Payload)
    ));
    assert!(matches!(
        intake
            .ingest("movies_1", ONE, br#"{"eventType":"Unsupported"}"#, 1)
            .await,
        Err(IntakeError::Payload)
    ));
    assert!(store.observations().await.unwrap().is_empty());
    let mut exact = br#"{"eventType":"Test"}"#.to_vec();
    exact.resize(MAX_BODY, b' ');
    assert!(intake.ingest("movies_1", ONE, &exact, 1).await.unwrap());
    assert_eq!(store.observations().await.unwrap().len(), 1);
}

#[tokio::test]
async fn replay_is_canonical_instance_scoped_and_durable_with_out_of_order_events() {
    let (dir, store, intake) = setup().await;
    let import = br#"{"eventType":"Download","movie":{"id":2},"downloadId":"opaque"}"#;
    assert!(intake.ingest("movies_1", ONE, import, 20).await.unwrap());
    let reordered = br#"{ "downloadId": "opaque", "movie": {"id": 2}, "eventType": "Download" }"#;
    assert!(!intake.ingest("movies_1", ONE, reordered, 21).await.unwrap());
    assert!(intake.ingest("series-2", TWO, import, 22).await.unwrap());
    let grab = br#"{"eventType":"Grab","downloadId":"opaque"}"#;
    let (first, second) = tokio::join!(
        intake.ingest("movies_1", ONE, grab, 10),
        intake.ingest("movies_1", ONE, grab, 10)
    );
    assert_ne!(first.unwrap(), second.unwrap());
    let records = store.observations().await.unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].source, "webhook:movies_1");
    assert_eq!(records[0].at, 20);
    assert_eq!(records[1].source, "webhook:series-2");
    assert_eq!(records[2].at, 10);
    assert_eq!(records[2].payload["kind"], "Grab");
    assert_eq!(records[0].key, records[1].key);
    assert_eq!(records[0].key.len(), 64);
    assert!(records[0].key.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(store.jobs().await.unwrap().is_empty());
    let reopened = Store::open(dir.path().join("events.sqlite")).await.unwrap();
    let restarted = Intake::new(
        reopened.clone(),
        [("movies_1".into(), Secret::new(ONE).unwrap())],
    )
    .unwrap();
    assert!(!restarted.ingest("movies_1", ONE, import, 30).await.unwrap());
    assert_eq!(reopened.observations().await.unwrap(), records);
}

proptest::proptest! {
    #[test]
    fn episode_ids_are_sorted_deduplicated_and_exact(ids in proptest::collection::vec(1u64..u64::MAX, 0..100)) {
        let values: Vec<Value> = ids.iter().map(|id| json!({"id":id})).collect();
        let actual = parse(&json!({"eventType":"Grab", "episodes": values})).unwrap();
        let expected: Vec<_> = ids.into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        proptest::prop_assert_eq!(actual.media, expected);
    }
}

#[test]
fn sonarr_import_complete_episode_files_array_is_preserved_and_validated() {
    let event = json!({"eventType":"Download", "series":{"id":1}, "episodes":[{"id":2}],
        "episodeFiles":[{"id":5}, {"id":3}, {"id":5}], "downloadId":"opaque"});
    let parsed = parse(&event).unwrap();
    assert_eq!(parsed.files, vec![3, 5]);
    assert_eq!(parsed.media, vec![2]);
    for files in [
        json!(null),
        json!({"id":3}),
        json!([{"id":0}]),
        json!([{}]),
        json!([{"id":"invalid"}]),
    ] {
        let mut bad = event.clone();
        bad["episodeFiles"] = files;
        assert!(matches!(parse(&bad), Err(IntakeError::Payload)));
    }
    let mut mixed = event;
    mixed["episodeFile"] = json!({"id":7});
    assert_eq!(parse(&mixed).unwrap().files, vec![3, 5, 7]);
}
