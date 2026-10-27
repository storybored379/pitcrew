use pitcrew_intake::{
    Resolution,
    pipeline::{AccessProvider, CatalogResolver},
};
use pitcrew_sync::{reader::*, *};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn binding() -> LibraryBinding {
    LibraryBinding {
        server_scope: "primary".into(),
        section_id: 2,
        library_id: "tv".into(),
        library_name: "Shows".into(),
        arr_instance_id: "sonarr-hd".into(),
        plex_root: "/plex".into(),
        arr_root: "/arr".into(),
        agent_root: "/agent".into(),
    }
}
fn episode() -> PlexItem {
    serde_json::from_value(serde_json::json!({"ratingKey":"11","type":"episode","title":"First","grandparentRatingKey":"10","grandparentTitle":"Synthetic Show","parentIndex":1,"index":2,"Media":[{"videoResolution":"1080","Part":[{"file":"/plex/show/episode.mkv"}]}]})).unwrap()
}
fn inventory() -> ArrInventory {
    ArrInventory {
        series: vec![Series {
            id: 8,
            title: "Synthetic Show".into(),
        }],
        episodes: vec![Episode {
            id: 12,
            series_id: 8,
            season_number: 1,
            episode_number: 2,
            episode_file_id: 14,
        }],
        files: vec![ArrFile {
            id: 14,
            path: "/arr/show/episode.mkv".into(),
            series_id: Some(8),
            movie_id: None,
        }],
        movies: vec![],
    }
}
fn page(item: PlexItem) -> PlexPage {
    PlexPage {
        items: vec![item],
        next: None,
    }
}
fn access() -> AccessSnapshot {
    AccessSnapshot::from_grants("viewer".into(), vec!["tv".into()], 100, 200)
}
fn snapshot() -> CatalogSnapshot {
    let mut s = CatalogSnapshot::default();
    s.merge_page(&binding(), &page(episode()), &inventory(), 100, 200)
        .unwrap();
    s
}
const ID: &str = "plex:primary:2:11";

#[test]
fn exact_episode_resolution_and_local_roundtrip() {
    let s = snapshot();
    let t = s.resolve(ID, 100).unwrap();
    assert_eq!(
        (t.arr_instance_id.as_str(), t.arr_item_id, t.arr_file_id),
        ("sonarr-hd", 12, 14)
    );
    assert_eq!(t.agent_path, "/agent/show/episode.mkv");
    assert_eq!(t.agent_file, None);
    assert_eq!(t.resource, "arr:sonarr-hd:episode:12");
    assert_eq!(t.plex_rating_key, "11");
    assert_eq!(t.item.id, ID);
    assert_eq!(t.item.title, "Synthetic Show");
    assert_eq!(t.item.library_id, "tv");
    assert_eq!(t.item.library_name, "Shows");
    assert_eq!(t.item.resolution, Resolution::Hd);
    assert_eq!((t.item.season, t.item.episode), (Some(1), Some(2)));
    assert_eq!((t.observed_at, t.expires_at), (100, 200));
    assert_eq!(s.series_id(ID), Some("plex:primary:2:10"));
    assert_eq!(s.series_id("missing"), None);
    assert!(s.resolve("missing", 100).is_err());
    assert_eq!(s.items(&access(), 100), vec![t.item]);
    assert!(s == serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap());
}

#[test]
fn movie_mapping_rejects_ambiguity_and_preserves_scopes() {
    let mut p = episode();
    p.kind = "movie".into();
    p.media[0].resolution = Some("4k".into());
    let mut arr = inventory();
    arr.movies.push(Movie {
        id: 3,
        title: "Synthetic Film".into(),
        movie_file: Some(ArrFile {
            id: 4,
            path: "/arr/show/episode.mkv".into(),
            series_id: None,
            movie_id: Some(3),
        }),
    });
    let mut s = CatalogSnapshot::default();
    s.merge_page(&binding(), &page(p.clone()), &arr, 100, 200)
        .unwrap();
    let t = s.resolve(ID, 199).unwrap();
    assert_eq!(t.resource, "arr:sonarr-hd:movie:3");
    assert_eq!((t.item.season, t.item.episode), (None, None));
    assert_eq!(t.item.title, "First");
    assert_eq!(t.item.resolution, Resolution::Uhd);
    assert_eq!(s.series_id(ID), None);
    for field in 0..5 {
        let mut bad = arr.clone();
        match field {
            0 => bad.movies[0].id = 0,
            1 => bad.movies[0].movie_file.as_mut().unwrap().id = 0,
            2 => bad.movies[0].movie_file.as_mut().unwrap().movie_id = Some(99),
            3 => bad.movies[0].movie_file.as_mut().unwrap().path = "/elsewhere".into(),
            _ => bad.movies[0].movie_file = None,
        }
        s.merge_page(&binding(), &page(p.clone()), &bad, 100, 200)
            .unwrap();
        assert!(s.resolve(ID, 150).is_err());
    }
    arr.movies[0].movie_file.as_mut().unwrap().movie_id = None;
    s.merge_page(&binding(), &page(p.clone()), &arr, 100, 200)
        .unwrap();
    assert!(s.resolve(ID, 150).is_ok());
    arr.movies.push(arr.movies[0].clone());
    s.merge_page(&binding(), &page(p.clone()), &arr, 100, 200)
        .unwrap();
    assert!(s.resolve(ID, 150).is_err());
    let mut b = binding();
    b.server_scope = "secondary".into();
    s.merge_page(&b, &page(p), &inventory(), 100, 200).unwrap();
    assert_eq!(s.items(&access(), 150).len(), 2);
}

#[test]
fn no_stale_or_cross_library_exposure() {
    let s = snapshot();
    for now in [0, 99, 200, 201, u64::MAX] {
        assert!(s.resolve(ID, now).is_err());
        assert!(s.items(&access(), now).is_empty());
    }
    assert!(s.resolve(ID, 199).is_ok());
    assert!(s.items(&AccessSnapshot::default(), 150).is_empty());
    let denied = AccessSnapshot::from_grants("viewer".into(), vec!["movies".into()], 100, 200);
    assert!(s.items(&denied, 150).is_empty());
}

#[test]
fn missing_changed_and_multi_version_files_never_resolve() {
    for change in 0..16 {
        let mut arr = inventory();
        let mut p = episode();
        match change {
            0 => arr.files.clear(),
            1 => arr.episodes[0].id = 0,
            2 => arr.episodes[0].series_id = 0,
            3 => arr.episodes[0].episode_file_id = 0,
            4 => arr.files[0].id += 1,
            5 => arr.files[0].series_id = None,
            6 => arr.files[0].path = "/other".into(),
            7 => arr.episodes[0].season_number += 1,
            8 => arr.episodes[0].episode_number += 1,
            9 => arr.series.clear(),
            10 => arr.series.push(arr.series[0].clone()),
            11 => arr.files.push(arr.files[0].clone()),
            12 => arr.episodes.push(arr.episodes[0].clone()),
            13 => p.media.push(p.media[0].clone()),
            14 => {
                let part = p.media[0].parts[0].clone();
                p.media[0].parts.push(part);
            }
            _ => p.media[0].parts[0].file = None,
        }
        let mut s = snapshot();
        s.merge_page(&binding(), &page(p), &arr, 100, 200).unwrap();
        assert!(s.resolve(ID, 150).is_err(), "case {change}");
    }
}

#[test]
fn malformed_batch_is_atomic_and_bounds_are_inclusive() {
    for change in 0..18 {
        let mut b = binding();
        let mut p = episode();
        let mut expires = 200;
        match change {
            0 => p.rating_key = "0".into(),
            1 => p.rating_key = "bad".into(),
            2 => p.kind = "show".into(),
            3 => p.series_key = None,
            4 => p.series_key = Some("bad".into()),
            5 => p.season = None,
            6 => p.episode = None,
            7 => p.series_title = None,
            8 => p.media[0].parts[0].file = Some("/plex/../escape".into()),
            9 => p.media[0].parts[0].file = Some("relative".into()),
            10 => p.media[0].parts[0].file = Some("/plex".into()),
            11 => p.media[0].parts[0].file = Some("/plexish/file".into()),
            12 => b.server_scope = "bad:scope".into(),
            13 => b.arr_instance_id = String::new(),
            14 => b.section_id = 0,
            15 => b.library_id = String::new(),
            16 => b.agent_root = "relative".into(),
            _ => expires = 100,
        }
        let mut s = snapshot();
        assert!(
            s.merge_page(&b, &page(p), &inventory(), 100, expires)
                .is_err(),
            "case {change}"
        );
        assert!(s == snapshot());
    }
    let mut s = snapshot();
    let mut p = page(episode());
    p.items.push(episode());
    assert_eq!(
        s.merge_page(&binding(), &p, &inventory(), 100, 200),
        Err(Error::Ambiguous)
    );
    p.items = vec![episode(); 1001];
    assert_eq!(
        s.merge_page(&binding(), &p, &inventory(), 100, 200),
        Err(Error::Bound)
    );
    let mut full = page(episode());
    full.items = (1..=1000)
        .map(|i| {
            let mut p = episode();
            p.rating_key = i.to_string();
            p
        })
        .collect();
    s.merge_page(&binding(), &full, &inventory(), 100, 200)
        .unwrap();
    assert_eq!(s.items(&access(), 150).len(), 1000);
}

#[test]
fn inventory_and_total_catalog_caps_preserve_existing_snapshot() {
    let mut s = CatalogSnapshot::default();
    let empty = PlexPage {
        items: vec![],
        next: None,
    };
    for kind in 0..4 {
        let mut arr = ArrInventory::default();
        match kind {
            0 => {
                arr.series = vec![
                    Series {
                        id: 1,
                        title: String::new()
                    };
                    100_000
                ]
            }
            1 => {
                arr.episodes = vec![
                    Episode {
                        id: 1,
                        series_id: 1,
                        season_number: 0,
                        episode_number: 0,
                        episode_file_id: 1
                    };
                    100_000
                ]
            }
            2 => {
                arr.movies = vec![
                    Movie {
                        id: 1,
                        title: String::new(),
                        movie_file: None
                    };
                    100_000
                ]
            }
            _ => {
                arr.files = vec![
                    ArrFile {
                        id: 1,
                        path: String::new(),
                        series_id: None,
                        movie_id: None
                    };
                    100_000
                ]
            }
        }
        assert!(s.merge_page(&binding(), &empty, &arr, 100, 200).is_ok());
        match kind {
            0 => arr.series.push(arr.series[0].clone()),
            1 => arr.episodes.push(arr.episodes[0].clone()),
            2 => arr.movies.push(arr.movies[0].clone()),
            _ => arr.files.push(arr.files[0].clone()),
        }
        assert_eq!(
            s.merge_page(&binding(), &empty, &arr, 100, 200),
            Err(Error::Bound)
        );
    }
}

#[test]
fn history_and_watchlist_filter_sort_dedupe_and_cap() {
    let mut s = snapshot();
    let mut p = episode();
    p.rating_key = "12".into();
    s.merge_page(&binding(), &page(p), &inventory(), 100, 200)
        .unwrap();
    let entry = |account: &str, id: &str, kind, at| PickerEntry {
        account_id: account.into(),
        item_id: id.into(),
        kind,
        at,
    };
    s.set_activity(vec![
        entry("viewer", ID, PickerKind::RecentPlay, 120),
        entry("viewer", ID, PickerKind::RecentPlay, 130),
        entry("viewer", "plex:primary:2:12", PickerKind::RecentPlay, 140),
        entry("other", ID, PickerKind::Watchlist, 120),
        entry("viewer", ID, PickerKind::Watchlist, 121),
        entry("viewer", "missing", PickerKind::RecentPlay, 141),
        entry("viewer", ID, PickerKind::RecentPlay, 151),
    ])
    .unwrap();
    let picked = s.picker("viewer", PickerKind::RecentPlay, &access(), 150, 1000);
    assert_eq!(
        picked.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        vec!["plex:primary:2:12", ID]
    );
    assert_eq!(
        s.picker("viewer", PickerKind::RecentPlay, &access(), 150, 1)
            .len(),
        1
    );
    assert_eq!(
        s.picker("viewer", PickerKind::Watchlist, &access(), 150, 10)[0].id,
        ID
    );
    assert!(
        s.picker("other", PickerKind::RecentPlay, &access(), 150, 10)
            .is_empty()
    );
    assert!(
        s.picker("viewer", PickerKind::RecentPlay, &access(), 200, 10)
            .is_empty()
    );
    assert!(
        s.picker("viewer", PickerKind::RecentPlay, &access(), 150, 0)
            .is_empty()
    );
    s.set_activity(vec![
        entry("viewer", ID, PickerKind::Watchlist, 120);
        10_000
    ])
    .unwrap();
    assert_eq!(
        s.set_activity(vec![
            entry("viewer", ID, PickerKind::Watchlist, 120);
            10_001
        ]),
        Err(Error::Bound)
    );
    let mut seen = BTreeSet::new();
    seen.insert(ID.into());
    s.retain_seen("other", &BTreeSet::new());
    assert_eq!(s.items(&access(), 150).len(), 2);
    s.retain_seen("tv", &seen);
    assert_eq!(s.items(&access(), 150).len(), 1);
    s.retain_seen("tv", &BTreeSet::new());
    assert!(s.items(&access(), 150).is_empty());
}

#[test]
fn complete_shared_access_xml_and_identity() {
    let xml=br#"<MediaContainer size="2"><SharedServer userID="viewer"><Section key="2" shared="1"/><Section key="3" shared="0"/><Section key="9" shared="1"/></SharedServer><SharedServer userID="other"><Section key="3" shared="1"/></SharedServer></MediaContainer>"#;
    let map = vec![("2".into(), "tv".into()), ("3".into(), "movies".into())];
    let access = AccessSnapshot::from_shared_servers_xml(xml, "viewer", &map, 100, 200).unwrap();
    let grant = access.access("viewer", 100).unwrap();
    assert_eq!(
        (
            grant.account_id.as_str(),
            grant.observed_at,
            grant.expires_at
        ),
        ("viewer", 100, 200)
    );
    assert_eq!(grant.library_ids, vec!["tv"]);
    assert!(access.access("other", 150).is_none());
    assert!(access.access("viewer", 200).is_none());
    let unknown = AccessSnapshot::from_shared_servers_xml(xml, "absent", &map, 100, 200).unwrap();
    assert!(unknown.authorize(100).unwrap().library_ids.is_empty());
    for bad in [
        "<MediaContainer size=\"1\"/>",
        "<bad size=\"0\"/>",
        "<MediaContainer/>",
        "<MediaContainer size=\"no\"/>",
        "<MediaContainer size=\"1\"><Wrong/></MediaContainer>",
        "<MediaContainer size=\"1\"><SharedServer userID=\"viewer\"><Wrong/></SharedServer></MediaContainer>",
        "<MediaContainer size=\"1\"><SharedServer userID=\"viewer\"><Section key=\"2\"/></SharedServer></MediaContainer>",
        "<MediaContainer size=\"1\"><SharedServer userID=\"viewer\"><Section key=\"2\" shared=\"true\"/></SharedServer></MediaContainer>",
        "<MediaContainer size=\"1\"><SharedServer userID=\"viewer\"><Section shared=\"1\"/></SharedServer></MediaContainer>",
        "<MediaContainer size=\"2\"><SharedServer userID=\"viewer\"/><SharedServer userID=\"viewer\"/></MediaContainer>",
        "broken",
    ] {
        assert!(
            AccessSnapshot::from_shared_servers_xml(bad.as_bytes(), "viewer", &map, 100, 200)
                .is_err(),
            "{bad}"
        );
    }
    assert!(AccessSnapshot::from_shared_servers_xml(&[255], "viewer", &map, 100, 200).is_err());
    assert!(
        AccessSnapshot::from_shared_servers_xml(
            &vec![b' '; 1024 * 1024 + 1],
            "viewer",
            &map,
            100,
            200
        )
        .is_err()
    );
    assert!(
        AccessSnapshot::from_grants("".into(), vec![], 100, 200)
            .authorize(100)
            .is_none()
    );
    assert!(
        AccessSnapshot::from_grants("viewer".into(), vec!["".into()], 100, 200)
            .authorize(100)
            .is_none()
    );
}

proptest! {
    #[test]
    fn access_requires_exact_fresh_interval(observed in any::<u64>(), expires in any::<u64>(), now in any::<u64>()) {
        let access=AccessSnapshot::from_grants("viewer".into(),vec!["tv".into()],observed,expires);
        prop_assert_eq!(access.authorize(now).is_some(), observed <= now && now < expires);
    }
}

#[tokio::test]
async fn synced_exact_item_flows_through_intake_to_durable_diagnosis() {
    use pitcrew_diagnosis::{
        collectors::{AgentEvidence, Observation},
        rules::RecommendedAction,
    };
    use pitcrew_domain::{
        FileIdentity,
        channels::{ChannelIdentity, IncomingMessage},
        media::{MediaEvidence, MediaProbe, ProbeOutcome},
    };
    use pitcrew_intake::{IntakeService, IssueKind, pipeline::*};
    struct Evidence;
    impl EvidenceSource for Evidence {
        fn collect<'a>(
            &'a self,
            target: &'a ResolvedTarget,
            account: &'a str,
            _: &'a Clock,
        ) -> EvidenceFuture<'a> {
            assert_eq!(target.agent_path, "/agent/show/episode.mkv");
            assert_eq!(account, "viewer");
            Box::pin(async {
                CollectedEvidence {
                    agent: AgentEvidence {
                        media: Observation::Available(MediaEvidence {
                            file: FileIdentity {
                                id: 77,
                                revision: 4,
                            },
                            observed_at_unix_ms: 150_000,
                            symlinks: None,
                            mount: None,
                            reads: vec![],
                            header: None,
                            decode: Some(MediaProbe {
                                outcome: ProbeOutcome::Failed,
                                duration_ms: Some(90_000),
                                elapsed_ms: 1,
                                timeout_ms: 1000,
                                faults: vec![],
                                streams: vec![],
                                tested_ranges: vec![],
                            }),
                            plex_failures: vec![],
                        }),
                        plex: Observation::default(),
                    },
                    sessions: Observation::default(),
                }
            })
        }
    }
    let catalog = snapshot();
    let grants = access();
    let message = IncomingMessage {
        sender: ChannelIdentity {
            channel_id: "test".into(),
            platform_user_id: "viewer".into(),
        },
        message_id: "m1".into(),
        conversation_id: "c1".into(),
        text: "this episode fails".into(),
        received_at: 150,
    };
    let response = IntakeService.handle_selected(
        &message,
        grants.authorize(150).as_ref(),
        &catalog.items(&grants, 150),
        ID,
        &[IssueKind::Playback],
    );
    let dir = tempfile::tempdir().unwrap();
    let store = pitcrew_intake_store::Store::open(dir.path().join("intake.sqlite"))
        .await
        .unwrap();
    store
        .persist_intake(
            "test",
            "m1",
            "viewer",
            "c1",
            &serde_json::to_value(response).unwrap(),
            150,
        )
        .await
        .unwrap();
    let id = store.reports("viewer").await.unwrap()[0].id;
    let pipeline = Pipeline::new(&store, &catalog, &grants, &Evidence, &|| 150_000);
    assert_eq!(pipeline.diagnose(id).await.unwrap(), Outcome::Persisted);
    let diagnosis = store.diagnosis(id).await.unwrap().unwrap();
    assert_eq!(diagnosis.resource, "arr:sonarr-hd:episode:12");
    assert_eq!((diagnosis.arr_item_id, diagnosis.arr_file_id), (12, 14));
    assert_eq!(
        diagnosis.agent_file,
        Some(FileIdentity {
            id: 77,
            revision: 4
        })
    );
    assert_eq!(diagnosis.library_id, "tv");
    assert_eq!(
        diagnosis.decision.decision.action,
        RecommendedAction::Replace
    );
    assert_eq!(
        pipeline.diagnose(id).await.unwrap(),
        Outcome::AlreadyDecided
    );
}

#[test]
fn boundary_regressions_do_not_depend_on_redundant_failures() {
    for zero_series in [true, false] {
        let mut arr = inventory();
        if zero_series {
            arr.episodes[0].series_id = 0;
            arr.series[0].id = 0;
            arr.files[0].series_id = Some(0);
        } else {
            arr.episodes[0].episode_file_id = 0;
            arr.files[0].id = 0;
        }
        let mut s = snapshot();
        s.merge_page(&binding(), &page(episode()), &arr, 100, 200)
            .unwrap();
        assert!(s.resolve(ID, 150).is_err());
    }
    let mut b = binding();
    b.agent_root = "/agent/../escape".into();
    assert!(
        CatalogSnapshot::default()
            .merge_page(&b, &page(episode()), &inventory(), 100, 200)
            .is_err()
    );
    let prefix = "<MediaContainer size=\"0\"><!--";
    let suffix = "--></MediaContainer>";
    for len in [1024 * 1024, 1024 * 1024 + 1] {
        let xml = format!(
            "{prefix}{}{suffix}",
            " ".repeat(len - prefix.len() - suffix.len())
        );
        assert_eq!(
            AccessSnapshot::from_shared_servers_xml(xml.as_bytes(), "viewer", &[], 100, 200)
                .is_ok(),
            len == 1024 * 1024
        );
    }
}

#[test]
fn picker_has_no_access_or_activity_fallback() {
    let mut s = snapshot();
    for (account, kind, at) in [
        ("other", PickerKind::RecentPlay, 150),
        ("viewer", PickerKind::Watchlist, 150),
        ("viewer", PickerKind::RecentPlay, 151),
    ] {
        s.set_activity(vec![PickerEntry {
            account_id: account.into(),
            item_id: ID.into(),
            kind,
            at,
        }])
        .unwrap();
        assert!(
            s.picker("viewer", PickerKind::RecentPlay, &access(), 150, 10)
                .is_empty()
        );
    }
    s.set_activity(vec![PickerEntry {
        account_id: "viewer".into(),
        item_id: ID.into(),
        kind: PickerKind::RecentPlay,
        at: 150,
    }])
    .unwrap();
    let denied = AccessSnapshot::from_grants("viewer".into(), vec!["other".into()], 100, 200);
    assert!(
        s.picker("viewer", PickerKind::RecentPlay, &denied, 150, 10)
            .is_empty()
    );
    let newer = AccessSnapshot::from_grants("viewer".into(), vec!["tv".into()], 200, 300);
    assert!(
        s.picker("viewer", PickerKind::RecentPlay, &newer, 200, 10)
            .is_empty()
    );
    assert!(s.items(&newer, 200).is_empty());
}

#[test]
fn canonical_numeric_ids_cannot_create_alias_targets() {
    for key in ["01", "+1", " 1", "1 "] {
        let mut p = episode();
        p.rating_key = key.into();
        assert_eq!(
            CatalogSnapshot::default().merge_page(&binding(), &page(p), &inventory(), 100, 200),
            Err(Error::Malformed)
        );
        let mut p = episode();
        p.series_key = Some(key.into());
        assert_eq!(
            CatalogSnapshot::default().merge_page(&binding(), &page(p), &inventory(), 100, 200),
            Err(Error::Malformed)
        );
    }
}

#[test]
fn conflicting_shared_sections_or_mappings_do_not_authorize() {
    let xml=br#"<MediaContainer size="1"><SharedServer userID="viewer"><Section key="2" shared="0"/><Section key="2" shared="1"/></SharedServer></MediaContainer>"#;
    let map = vec![("2".into(), "tv".into())];
    assert_eq!(
        AccessSnapshot::from_shared_servers_xml(xml, "viewer", &map, 100, 200),
        Err(Error::Ambiguous)
    );
    let xml=br#"<MediaContainer size="1"><SharedServer userID="viewer"><Section key="2" shared="1"/></SharedServer></MediaContainer>"#;
    let map = vec![("2".into(), "tv".into()), ("2".into(), "movies".into())];
    assert_eq!(
        AccessSnapshot::from_shared_servers_xml(xml, "viewer", &map, 100, 200),
        Err(Error::Ambiguous)
    );
}

#[test]
fn duplicate_primary_ids_with_conflicting_metadata_cannot_resolve() {
    for kind in 0..3 {
        let mut arr = inventory();
        let mut p = episode();
        match kind {
            0 => {
                let mut row = arr.files[0].clone();
                row.path = "/arr/other.mkv".into();
                arr.files.push(row);
            }
            1 => {
                let mut row = arr.episodes[0].clone();
                row.episode_number = 3;
                arr.episodes.push(row);
            }
            _ => {
                p.kind = "movie".into();
                let file = ArrFile {
                    id: 4,
                    path: "/arr/show/episode.mkv".into(),
                    series_id: None,
                    movie_id: Some(3),
                };
                arr.movies = vec![
                    Movie {
                        id: 3,
                        title: "Synthetic".into(),
                        movie_file: Some(file.clone()),
                    },
                    Movie {
                        id: 3,
                        title: "Other".into(),
                        movie_file: Some(ArrFile {
                            path: "/arr/other.mkv".into(),
                            ..file
                        }),
                    },
                ];
            }
        }
        let mut s = snapshot();
        s.merge_page(&binding(), &page(p), &arr, 100, 200).unwrap();
        assert!(s.resolve(ID, 150).is_err());
    }
}

#[test]
fn older_pages_never_overwrite_or_resurrect_newer_observations() {
    let mut s = snapshot();
    let mut arr = inventory();
    arr.files[0].id = 15;
    arr.episodes[0].episode_file_id = 15;
    s.merge_page(&binding(), &page(episode()), &arr, 150, 250)
        .unwrap();
    assert!(
        s.merge_page(&binding(), &page(episode()), &inventory(), 100, 200)
            .is_err()
    );
    assert_eq!(s.resolve(ID, 160).unwrap().arr_file_id, 15);
    s.retain_seen("tv", &BTreeSet::new());
    assert!(
        s.merge_page(&binding(), &page(episode()), &inventory(), 100, 200)
            .is_err()
    );
    assert!(s.resolve(ID, 160).is_err());
}

#[test]
fn empty_plex_root_cannot_discard_agent_root() {
    let mut b = binding();
    b.plex_root = String::new();
    let mut arr = inventory();
    arr.files[0].path = "/plex/show/episode.mkv".into();
    assert!(
        CatalogSnapshot::default()
            .merge_page(&b, &page(episode()), &arr, 100, 200)
            .is_err()
    );
}

#[test]
fn shared_episode_files_and_conflicting_movie_file_owners_withhold_target() {
    let mut arr = inventory();
    let mut second = arr.episodes[0].clone();
    second.id = 13;
    second.episode_number = 3;
    arr.episodes.push(second);
    let mut s = snapshot();
    s.merge_page(&binding(), &page(episode()), &arr, 100, 200)
        .unwrap();
    assert!(s.resolve(ID, 150).is_err());
    let mut p = episode();
    p.kind = "movie".into();
    let arr = ArrInventory {
        movies: vec![
            Movie {
                id: 3,
                title: "One".into(),
                movie_file: Some(ArrFile {
                    id: 4,
                    path: "/arr/show/episode.mkv".into(),
                    series_id: None,
                    movie_id: Some(3),
                }),
            },
            Movie {
                id: 5,
                title: "Two".into(),
                movie_file: Some(ArrFile {
                    id: 4,
                    path: "/arr/other.mkv".into(),
                    series_id: None,
                    movie_id: Some(5),
                }),
            },
        ],
        ..ArrInventory::default()
    };
    s.merge_page(&binding(), &page(p), &arr, 100, 200).unwrap();
    assert!(s.resolve(ID, 150).is_err());
}

#[test]
fn zero_movie_id_cannot_resolve_even_when_file_owner_matches() {
    let mut p = episode();
    p.kind = "movie".into();
    let arr = ArrInventory {
        movies: vec![Movie {
            id: 0,
            title: "Synthetic Film".into(),
            movie_file: Some(ArrFile {
                id: 4,
                path: "/arr/show/episode.mkv".into(),
                series_id: None,
                movie_id: Some(0),
            }),
        }],
        ..ArrInventory::default()
    };
    let mut s = snapshot();
    s.merge_page(&binding(), &page(p), &arr, 100, 200).unwrap();
    assert!(s.resolve(ID, 150).is_err());
}
