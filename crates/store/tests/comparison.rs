use pitcrew_store::Store;
use serde_json::json;

#[tokio::test]
async fn comparison_reads_include_only_exact_trusted_source_classes_and_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    for source in [
        "engine_dispatch",
        "legacy_arr_action:one",
        "legacy_arr_action:two",
        "diff",
        "history:movies",
        "webhook:series",
        "untrusted",
        "prefixengine_dispatch",
        "legacy_arr_action",
        "diff:untrusted",
        "history",
    ] {
        store
            .ingest_observation(source, "one", &json!({"source":source}), 10)
            .await
            .unwrap();
    }
    store
        .ingest_observation("engine_dispatch", "two", &json!({"different":true}), 11)
        .await
        .unwrap();
    let receipts = store.action_receipts().await.unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|r| r.source.as_str())
            .collect::<Vec<_>>(),
        vec![
            "engine_dispatch",
            "legacy_arr_action:one",
            "legacy_arr_action:two",
            "engine_dispatch"
        ]
    );
    let inputs = store.comparison_inputs().await.unwrap();
    assert_eq!(
        inputs.iter().map(|r| r.source.as_str()).collect::<Vec<_>>(),
        vec!["diff", "history:movies", "webhook:series"]
    );
    assert!(store.observation("missing", "one").await.unwrap().is_none());
    assert!(
        store
            .observation("engine_dispatch", "missing")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .observation("engine_dispatch", "two")
            .await
            .unwrap()
            .unwrap(),
        receipts[3]
    );
    assert_eq!(receipts[3].at, 11);
}
