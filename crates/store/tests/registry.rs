use pitcrew_domain::FileIdentity;
use pitcrew_store::{Store, policy::IdentityFingerprint, registry::RegisteredFile};

fn fingerprint(arr_file_id: u64) -> IdentityFingerprint {
    IdentityFingerprint {
        arr_file_id,
        size: 1024,
        date_added: "2026-01-01T00:00:00Z".into(),
        quality: r#"{"quality":{"id":4}}"#.into(),
        agent: FileIdentity {
            id: 100,
            revision: 200,
        },
    }
}

#[tokio::test]
async fn current_registry_reader_preserves_normalized_identity_scope_and_refresh_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("journal")).await.unwrap();
    assert_eq!(
        store
            .registered_file("client-a", "resource-a")
            .await
            .unwrap(),
        None
    );
    let file = fingerprint(10);
    let normalized = store
        .register_file("client-a", "resource-a", &file, 7)
        .await
        .unwrap();
    assert_eq!(normalized.id, 10);
    assert_ne!(normalized, file.agent);
    assert_eq!(
        store
            .registered_file("client-a", "resource-a")
            .await
            .unwrap(),
        Some(RegisteredFile {
            identity: normalized,
            fingerprint: file.clone(),
            observed_at: 7,
        })
    );
    assert_eq!(
        store
            .registered_file("client-b", "resource-a")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .registered_file("client-a", "resource-b")
            .await
            .unwrap(),
        None
    );

    let other_client = store
        .register_file("client-b", "resource-a", &file, 8)
        .await
        .unwrap();
    let other_resource_file = fingerprint(11);
    let other_resource = store
        .register_file("client-a", "resource-b", &other_resource_file, 9)
        .await
        .unwrap();
    assert_ne!(normalized, other_client);
    assert_ne!(normalized, other_resource);
    assert_eq!(
        store
            .register_file("client-a", "resource-a", &file, 10)
            .await
            .unwrap(),
        normalized
    );
    for (client, resource, identity, fingerprint, observed_at) in [
        ("client-a", "resource-a", normalized, file.clone(), 10),
        ("client-b", "resource-a", other_client, file, 8),
        (
            "client-a",
            "resource-b",
            other_resource,
            other_resource_file,
            9,
        ),
    ] {
        assert_eq!(
            store.registered_file(client, resource).await.unwrap(),
            Some(RegisteredFile {
                identity,
                fingerprint,
                observed_at
            })
        );
    }
}

#[tokio::test]
async fn changed_current_tuple_and_aba_revision_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal");
    let store = Store::open(&path).await.unwrap();
    let original = fingerprint(10);
    let first = store
        .register_file("client-a", "resource-a", &original, 7)
        .await
        .unwrap();
    let changed = IdentityFingerprint {
        arr_file_id: 20,
        size: 2048,
        agent: FileIdentity {
            id: 101,
            revision: 201,
        },
        ..original.clone()
    };
    let second = store
        .register_file("client-a", "resource-a", &changed, 8)
        .await
        .unwrap();
    let reopened = Store::open(&path).await.unwrap();
    assert_ne!(first, second);
    assert_eq!(
        reopened
            .registered_file("client-a", "resource-a")
            .await
            .unwrap(),
        Some(RegisteredFile {
            identity: second,
            fingerprint: changed,
            observed_at: 8,
        })
    );
    let third = reopened
        .register_file("client-a", "resource-a", &original, 9)
        .await
        .unwrap();
    assert_ne!(first, third);
    assert_eq!(
        store
            .registered_file("client-a", "resource-a")
            .await
            .unwrap(),
        Some(RegisteredFile {
            identity: third,
            fingerprint: original,
            observed_at: 9,
        })
    );
}
