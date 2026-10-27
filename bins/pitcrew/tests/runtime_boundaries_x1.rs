//! Offline composition boundaries: incomplete observations never replace a
//! usable snapshot, and optional integrations cannot invent repair authority.
use pitcrew::{
    runtime::{Runtime, snapshot_path},
    stack,
};
use pitcrew_config::Config;
use pitcrew_intake::pipeline::CatalogResolver;
use pitcrew_sync::{
    CatalogSnapshot,
    reader::{ArrFile, Episode},
};
use pitcrew_upstream::{Error, Method, Request, Response, ResponseFuture, Transport};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const ITEM: &str = "plex:primary:1:11";
type ObservedCall = (String, BTreeMap<String, String>);

#[derive(Clone)]
struct Fake {
    state: Arc<Mutex<Fixture>>,
    calls: Arc<Mutex<Vec<ObservedCall>>>,
    clock: Arc<AtomicU64>,
}
struct Fixture {
    series: u64,
    pages: u64,
    fail: Option<String>,
    section_kind: &'static str,
    plex_root: &'static str,
    arr_root: &'static str,
    title: &'static str,
    expire_on_page: bool,
    episode_counts: Option<[usize; 2]>,
    file_counts: Option<[usize; 2]>,
}
impl Fake {
    fn new(clock: Arc<AtomicU64>) -> Self {
        Self {
            state: Arc::new(Mutex::new(Fixture {
                series: 1,
                pages: 1,
                fail: None,
                section_kind: "show",
                plex_root: "/plex",
                arr_root: "/arr",
                title: "Synthetic Show",
                expire_on_page: false,
                episode_counts: None,
                file_counts: None,
            })),
            calls: Default::default(),
            clock,
        }
    }
}
impl Transport for Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get, "runtime attempted a write");
        assert!(request.body.is_none());
        let path = request.url.path().to_owned();
        let query: BTreeMap<String, String> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        self.calls
            .lock()
            .unwrap()
            .push((path.clone(), query.clone()));
        let fixture = self.state.lock().unwrap();
        if fixture.fail.as_deref() == Some(path.as_str()) {
            return Box::pin(async { Err(Error::Transport) });
        }
        // Typed compact rows keep each large fixture below the transport's
        // byte limit while exercising the independent aggregate inventory cap.
        let counts = match path.as_str() {
            "/api/v3/episode" => fixture.episode_counts,
            "/api/v3/episodefile" => fixture.file_counts,
            _ => None,
        };
        if let Some(counts) = counts {
            let series: u64 = query["seriesId"].parse().unwrap();
            let count = counts[usize::try_from(series - 8).unwrap()];
            let body = if path == "/api/v3/episode" {
                let rows: Vec<_> = (0..count)
                    .map(|index| Episode {
                        id: if index == 0 {
                            12 + series - 8
                        } else {
                            100 + (series - 8) * 50_001 + index as u64
                        },
                        series_id: series,
                        season_number: u32::from(index == 0),
                        episode_number: if index == 0 {
                            (2 + series - 8) as u32
                        } else {
                            index as u32
                        },
                        episode_file_id: if index == 0 { 14 + series - 8 } else { 0 },
                    })
                    .collect();
                serde_json::to_vec(&rows).unwrap()
            } else {
                let rows: Vec<_> = (0..count)
                    .map(|index| {
                        let id = if index == 0 {
                            14 + series - 8
                        } else {
                            100 + (series - 8) * 50_001 + index as u64
                        };
                        ArrFile {
                            id,
                            path: if index == 0 {
                                format!("{}/show/episode-{series}.mkv", fixture.arr_root)
                            } else {
                                format!("/arr/extra-{id}")
                            },
                            series_id: Some(series),
                            movie_id: None,
                        }
                    })
                    .collect();
                serde_json::to_vec(&rows).unwrap()
            };
            assert!(body.len() <= request.limits.max_response_bytes);
            return Box::pin(async move { Ok(Response { status: 200, body }) });
        }
        let body = match path.as_str() {
            "/library/sections" => {
                json!({"MediaContainer":{"size":1,"Directory":[{"key":"1","title":"Shows","type":fixture.section_kind}]}})
            }
            "/api/v3/series" => json!(
                (0..fixture.series)
                    .map(|i| json!({"id":8+i,"title":"Synthetic Show"}))
                    .collect::<Vec<_>>()
            ),
            "/api/v3/episode" => {
                let series: u64 = query["seriesId"].parse().unwrap();
                json!([{"id":12+series-8,"seriesId":series,"seasonNumber":1,"episodeNumber":2+series-8,"episodeFileId":14+series-8}])
            }
            "/api/v3/episodefile" => {
                let series: u64 = query["seriesId"].parse().unwrap();
                json!([{"id":14+series-8,"seriesId":series,"path":format!("{}/show/episode-{series}.mkv",fixture.arr_root)}])
            }
            "/library/sections/1/all" => {
                assert_eq!(query["type"], "4");
                let offset: u64 = query["X-Plex-Container-Start"].parse().unwrap();
                assert!(offset < fixture.pages);
                if fixture.expire_on_page {
                    self.clock.store(130_000, Ordering::SeqCst);
                }
                json!({"MediaContainer":{"size":1,"offset":offset,"totalSize":fixture.pages,"Metadata":[{
                    "ratingKey":(11+offset).to_string(),"type":"episode","title":"Synthetic Episode",
                    "grandparentRatingKey":"10","grandparentTitle":fixture.title,"parentIndex":1,"index":2+offset,
                    "Media":[{"videoResolution":"1080","Part":[{"file":format!("{}/show/episode-{}.mkv",fixture.plex_root,8+offset)}]}]
                }]}})
            }
            "/api/v3/episode/12" => json!({"episodeFileId":14}),
            "/api/v3/episodefile/14" => {
                json!({"id":14,"seriesId":8,"path":format!("{}/show/episode-8.mkv",fixture.arr_root),"size":100000000,"dateAdded":"2026-01-01T00:00:00Z","quality":{"quality":{"id":7}}})
            }
            "/api/v2/user" => json!({"id":99}),
            "/api/servers/server/shared_servers" => {
                return Box::pin(async {
                    Ok(Response {status:200,body:br#"<MediaContainer size="1"><SharedServer userID="7"><Section key="1" shared="1"/></SharedServer></MediaContainer>"#.to_vec()})
                });
            }
            _ => panic!("unexpected fake request: {path}"),
        };
        Box::pin(async move {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}
struct Setup {
    dir: tempfile::TempDir,
    config: Config,
    store: pitcrew_store::Store,
    fake: Fake,
    clock: Arc<AtomicU64>,
}
impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "synthetic-token").unwrap();
        let config = Config::parse(&format!(
            r#"
version=1
database={engine:?}
[stack]
enabled=true
intake_database={intake:?}
plex_auth_url="https://auth.invalid/"
[runtime]
snapshot_ttl_seconds=30
max_pages=2
max_series=2
[[runtime.libraries]]
library_id="1"
plex_root="/plex"
arr_root="/arr"
agent_root="/agent"
[plex]
url="https://plex.invalid/"
owner_token={{file={secret:?}}}
server_id="server"
[[arr]]
id="shows"
kind="sonarr"
url="https://arr.invalid/"
api_key={{file={secret:?}}}
libraries=["1"]
"#,
            engine = dir.path().join("engine.sqlite").to_str().unwrap(),
            intake = dir.path().join("intake.sqlite").to_str().unwrap(),
            secret = secret.to_str().unwrap(),
        ))
        .unwrap();
        let store = pitcrew_store::Store::open(&config.database).await.unwrap();
        let clock = Arc::new(AtomicU64::new(100_000));
        let fake = Fake::new(clock.clone());
        Self {
            dir,
            config,
            store,
            fake,
            clock,
        }
    }
    async fn open(&self) -> Result<Option<Runtime<Fake>>, &'static str> {
        let time = self.clock.clone();
        Runtime::open(
            &self.config,
            self.store.clone(),
            self.fake.clone(),
            Arc::new(move || time.load(Ordering::SeqCst)),
        )
        .await
    }
    fn path(&self) -> PathBuf {
        snapshot_path(&self.config)
    }
    fn snapshot(&self) -> CatalogSnapshot {
        serde_json::from_slice(&std::fs::read(self.path()).unwrap()).unwrap()
    }
}

#[tokio::test]
async fn sonarr_sweep_resolves_exact_mapped_paths_and_keeps_source_observation_time() {
    let s = Setup::new().await;
    s.fake.state.lock().unwrap().series = 2;
    s.fake.state.lock().unwrap().pages = 2;
    let mut runtime = s.open().await.unwrap().unwrap();
    assert!(
        s.fake.calls.lock().unwrap().is_empty(),
        "construction contacted upstream"
    );
    runtime.sync().await.unwrap();
    let snapshot = s.snapshot();
    for (id, item, file, episode) in [
        ("plex:primary:1:11", 12, 14, 2),
        ("plex:primary:1:12", 13, 15, 3),
    ] {
        let target = snapshot.resolve(id, 100).unwrap();
        assert_eq!(target.arr_instance_id, "shows");
        assert_eq!(target.resource, format!("arr:shows:episode:{item}"));
        assert_eq!(target.arr_file_id, file);
        assert_eq!(target.arr_item_id, item);
        assert_eq!(
            target.agent_path,
            format!("/agent/show/episode-{}.mkv", 8 + episode - 2)
        );
        assert_eq!(target.item.library_id, "1");
        assert_eq!(target.item.library_name, "Shows");
        assert_eq!(target.item.title, "Synthetic Show");
        assert_eq!(target.item.season, Some(1));
        assert_eq!(target.item.episode, Some(episode));
        assert_eq!((target.observed_at, target.expires_at), (100, 130));
        assert!(snapshot.resolve(id, 130).is_err());
    }
    assert!(
        s.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(p, q)| p == "/api/v3/episodefile"
                && q.get("seriesId").map(String::as_str) == Some("9"))
    );
    assert!(
        s.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(p, q)| p == "/library/sections/1/all"
                && q.get("X-Plex-Container-Start").map(String::as_str) == Some("1"))
    );
    assert_eq!(
        stack::web_config(&s.config)
            .unwrap()
            .catalog_snapshot_file
            .as_deref(),
        s.path().to_str()
    );
}

#[tokio::test]
async fn failed_or_capped_sweeps_preserve_the_previous_export_and_expiry() {
    for failure in [
        "series_cap",
        "page_cap",
        "sections",
        "episodes",
        "files",
        "page",
        "type",
        "expired",
    ] {
        let mut s = Setup::new().await;
        s.config.runtime.max_series = 1;
        s.config.runtime.max_pages = 1;
        let mut runtime = s.open().await.unwrap().unwrap();
        runtime.sync().await.unwrap();
        let before = std::fs::read(s.path()).unwrap();
        s.clock.store(110_000, Ordering::SeqCst);
        {
            let f = &mut *s.fake.state.lock().unwrap();
            f.title = "Changed upstream title";
            match failure {
                "series_cap" => f.series = 2,
                "page_cap" => f.pages = 2,
                "sections" => f.fail = Some("/library/sections".into()),
                "episodes" => f.fail = Some("/api/v3/episode".into()),
                "files" => f.fail = Some("/api/v3/episodefile".into()),
                "page" => f.fail = Some("/library/sections/1/all".into()),
                "type" => f.section_kind = "movie",
                "expired" => f.expire_on_page = true,
                _ => unreachable!(),
            }
        }
        // Start at 100 so the final page's 130 clock is exactly the new expiry.
        if failure == "expired" {
            s.clock.store(100_000, Ordering::SeqCst);
        }
        assert!(runtime.sync().await.is_err(), "{failure}");
        assert_eq!(
            std::fs::read(s.path()).unwrap(),
            before,
            "{failure} changed export"
        );
        let prior = s.snapshot().resolve(ITEM, 129).unwrap();
        assert_eq!(prior.item.title, "Synthetic Show");
        assert_eq!(prior.expires_at, 130);
        assert!(s.snapshot().resolve(ITEM, 130).is_err());
    }
}

#[tokio::test]
async fn identical_paths_need_no_mapping_but_wrong_roots_never_resolve_a_target() {
    for matching in [true, false] {
        let mut s = Setup::new().await;
        s.config.runtime.libraries.clear();
        {
            let f = &mut *s.fake.state.lock().unwrap();
            f.plex_root = "/media";
            f.arr_root = if matching { "/media" } else { "/other" };
        }
        let mut runtime = s.open().await.unwrap().unwrap();
        runtime.sync().await.unwrap();
        let resolved = s.snapshot().resolve(ITEM, 100);
        if matching {
            assert_eq!(resolved.unwrap().agent_path, "/media/show/episode-8.mkv");
        } else {
            assert!(resolved.is_err());
        }
    }
}

#[tokio::test]
async fn absent_agent_keeps_catalog_available_and_report_unverified() {
    let s = Setup::new().await;
    assert!(s.config.agent.is_none());
    let mut runtime = s.open().await.unwrap().unwrap();
    runtime.sync().await.unwrap();
    let intake = pitcrew_intake_store::Store::open(&s.config.stack.intake_database)
        .await
        .unwrap();
    intake.persist_intake("web","report","7","browser",&json!({
        "outcome":"report","issues":[{"item_id":ITEM,"kinds":["playback"]}],
        "clarification":null,"report_ids":[],"reply":"Report received","suspicious_input":false,
    }),100).await.unwrap();
    let summary = runtime.tick().await.unwrap();
    assert_eq!(summary.reports_examined, 1);
    assert_eq!(summary.authority_recorded, 0);
    assert_eq!(summary.reports_held, 1);
    assert!(s.store.jobs().await.unwrap().is_empty());
    assert_eq!(
        intake.report_progress("7").await.unwrap()[0].state,
        "needs_review"
    );
    assert!(s.snapshot().resolve(ITEM, 100).is_ok());
    assert!(
        !s.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p.starts_with("/v1/"))
    );
}

#[tokio::test]
async fn inactive_runtime_never_resolves_credentials_or_contacts_upstreams() {
    for missing in ["disabled", "plex", "arr"] {
        let mut s = Setup::new().await;
        std::fs::remove_file(s.dir.path().join("secret")).unwrap();
        match missing {
            "disabled" => s.config.stack.enabled = false,
            "plex" => s.config.plex = None,
            "arr" => s.config.arr.clear(),
            _ => unreachable!(),
        }
        assert!(s.open().await.unwrap().is_none());
        assert!(s.fake.calls.lock().unwrap().is_empty());
        assert!(!s.path().exists());
    }
}

#[tokio::test]
async fn runtime_refuses_noncanonical_duplicate_or_unbound_library_mappings_before_any_read() {
    const CANONICAL_ERROR: &str =
        "arr.libraries require unique canonical positive Plex section keys";
    for (libraries, keep_mapping, expected_error) in [
        (vec!["0"], false, Some(CANONICAL_ERROR)),
        (vec!["01"], false, Some(CANONICAL_ERROR)),
        (vec!["+1"], false, Some(CANONICAL_ERROR)),
        (
            vec!["not-numeric"],
            false,
            Some("arr.libraries must be numeric Plex section keys"),
        ),
        (vec!["1", "1"], false, Some(CANONICAL_ERROR)),
        (
            vec!["2"],
            true,
            Some("runtime.libraries contains a library absent from arr.libraries"),
        ),
        (vec!["1"], false, None),
        (vec!["1", "2"], false, None),
    ] {
        let mut s = Setup::new().await;
        s.config.arr[0].libraries = libraries.into_iter().map(str::to_owned).collect();
        // An unrelated root mapping must not reject an invalid section key on
        // behalf of the canonical-key guard being tested.
        if !keep_mapping {
            s.config.runtime.libraries.clear();
        }
        match s.open().await {
            Ok(runtime) => {
                assert_eq!(expected_error, None, "{:?}", s.config.arr[0].libraries);
                assert!(runtime.is_some());
            }
            Err(error) => assert_eq!(Some(error), expected_error),
        }
        assert!(s.fake.calls.lock().unwrap().is_empty());
        assert!(!s.path().exists());
    }
}

#[tokio::test]
async fn export_failure_does_not_replace_destination_or_destroy_last_successful_snapshot() {
    let mut s = Setup::new().await;
    s.config.stack.catalog_snapshot_file = Some(s.dir.path().join("catalog.json"));
    let mut runtime = s.open().await.unwrap().unwrap();
    runtime.sync().await.unwrap();
    let before = std::fs::read(s.path()).unwrap();
    let saved = s.path().with_extension("saved");
    std::fs::rename(s.path(), &saved).unwrap();
    std::fs::create_dir(s.path()).unwrap();
    std::fs::write(s.path().join("keep"), "existing destination").unwrap();
    s.fake.state.lock().unwrap().title = "Changed upstream title";
    assert!(runtime.sync().await.is_err());
    assert_eq!(
        std::fs::read(s.path().join("keep")).unwrap(),
        b"existing destination"
    );
    assert_eq!(std::fs::read(&saved).unwrap(), before);
    std::fs::remove_file(s.path().join("keep")).unwrap();
    std::fs::remove_dir(s.path()).unwrap();
    std::fs::rename(saved, s.path()).unwrap();
    assert_eq!(
        s.snapshot().resolve(ITEM, 100).unwrap().item.title,
        "Synthetic Show"
    );
    runtime.sync().await.unwrap();
    assert_eq!(
        s.snapshot().resolve(ITEM, 100).unwrap().item.title,
        "Changed upstream title"
    );
    // A nonexistent output parent must fail without creating a partial export.
    s.config.stack.catalog_snapshot_file = Some(s.dir.path().join("absent/catalog.json"));
    let mut missing = s.open().await.unwrap().unwrap();
    assert!(missing.sync().await.is_err());
    assert!(!s.path().exists());
}

#[tokio::test]
async fn aggregate_episode_and_file_caps_preserve_the_last_complete_snapshot() {
    for episodes in [true, false] {
        let s = Setup::new().await;
        let mut runtime = s.open().await.unwrap().unwrap();
        runtime.sync().await.unwrap();
        {
            let fixture = &mut *s.fake.state.lock().unwrap();
            fixture.series = 2;
            fixture.title = "Complete large catalog";
            if episodes {
                fixture.episode_counts = Some([50_000, 50_000]);
            } else {
                fixture.file_counts = Some([50_000, 50_000]);
            }
        }
        s.clock.store(110_000, Ordering::SeqCst);
        runtime.sync().await.unwrap();
        let before = std::fs::read(s.path()).unwrap();
        let prior = s.snapshot().resolve(ITEM, 110).unwrap();
        assert_eq!(prior.item.title, "Complete large catalog");
        assert_eq!((prior.observed_at, prior.expires_at), (110, 140));
        {
            let fixture = &mut *s.fake.state.lock().unwrap();
            fixture.title = "Incomplete large catalog";
            if episodes {
                fixture.episode_counts = Some([50_000, 50_001]);
            } else {
                fixture.file_counts = Some([50_000, 50_001]);
            }
        }
        s.fake.calls.lock().unwrap().clear();
        s.clock.store(120_000, Ordering::SeqCst);
        assert_eq!(
            runtime.sync().await,
            Err("series inventory exceeds row bound"),
            "episodes={episodes}"
        );
        assert_eq!(std::fs::read(s.path()).unwrap(), before);
        let prior = s.snapshot().resolve(ITEM, 139).unwrap();
        assert_eq!(prior.item.title, "Complete large catalog");
        assert_eq!((prior.observed_at, prior.expires_at), (110, 140));
        assert!(s.snapshot().resolve(ITEM, 140).is_err());
        // The runtime must stop at its inventory bound, before a later catalog
        // mapping guard can reject the same oversized observations for it.
        let calls = s.fake.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|(path, query)| path == "/api/v3/episodefile"
                    && query.get("seriesId").map(String::as_str) == Some("9"))
        );
        assert!(
            !calls
                .iter()
                .any(|(path, _)| path == "/library/sections/1/all")
        );
    }
}
