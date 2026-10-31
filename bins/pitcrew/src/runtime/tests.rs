use super::*;
use pitcrew_domain::{
    Access, Constraints, Effect, Event, FileIdentity, Policy, QualityRelation, ReleaseCandidate,
    StageTimeouts, State,
    media::{ReleaseGuid, ReleaseIdentity},
};
use pitcrew_intake::{CatalogItem, Resolution, pipeline::ResolvedTarget};
use pitcrew_store::{BudgetLimits, Lease, NewJob, policy::IdentityFingerprint};
use pitcrew_upstream::{Method, Response};
use serde_json::{Value, json};
use std::sync::Mutex;

// These fakes are deliberately below the readers: every request still passes
// through the real arr client and JSON decoder, without opening any socket.
#[derive(Clone, Default)]
struct Fake {
    replies: Arc<Mutex<BTreeMap<String, Value>>>,
    calls: Arc<Mutex<Vec<String>>>,
    failure: Arc<Mutex<Option<pitcrew_upstream::Error>>>,
}
impl Fake {
    fn reply(&self, path: &str, value: Value) {
        self.replies.lock().unwrap().insert(path.into(), value);
    }
}
impl Transport for Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get);
        assert!(request.body.is_none());
        let path = request.url.path().to_owned();
        self.calls.lock().unwrap().push(path.clone());
        if let Some(error) = *self.failure.lock().unwrap() {
            return Box::pin(async move { Err(error) });
        }
        let body = serde_json::to_vec(
            self.replies
                .lock()
                .unwrap()
                .get(&path)
                .unwrap_or_else(|| panic!("unexpected fake request {path}")),
        )
        .unwrap();
        Box::pin(async move { Ok(Response { status: 200, body }) })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    runtime: Runtime<Fake>,
    fake: Fake,
    target: ResolvedTarget,
    file_path: &'static str,
    item_path: &'static str,
}

fn quality() -> Value {
    json!({"quality":{"id":7},"revision":{"version":2,"real":1}})
}
fn file() -> Value {
    json!({
        "id":12,"path":"/arr/film.mkv","movieId":9,"size":100_000_001,
        "dateAdded":"2026-01-01T00:00:00Z","quality":quality()
    })
}
fn agent() -> FileIdentity {
    FileIdentity {
        id: 77,
        revision: 4,
    }
}
fn identity(guid: &str) -> ReleaseIdentity {
    ReleaseIdentity {
        guid: Some(ReleaseGuid {
            source: "films:1".into(),
            value: guid.into(),
        }),
        ..ReleaseIdentity::default()
    }
}
fn candidate(guid: &str) -> Value {
    json!({
        "guid":guid,"indexerId":1,"approved":true,"rejected":false,
        "downloadAllowed":true,"rejections":[],"size":100_000_000,"quality":quality()
    })
}
fn access() -> Access {
    Access::Granted {
        library: 1,
        expires_at: 1000,
    }
}

impl Fixture {
    async fn new(kind: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "synthetic-test-token").unwrap();
        let config = Config::parse(&format!(
            r#"version=1
database={engine:?}
[stack]
enabled=true
intake_database={intake:?}
plex_auth_url="https://auth.invalid/"
[runtime]
search_enabled=true
[[runtime.libraries]]
library_id="1"
plex_root="/plex"
arr_root="/arr"
agent_root="/agent"
[[runtime.libraries]]
library_id="2"
plex_root="/plex"
arr_root="/arr"
agent_root="/agent"
[plex]
url="https://plex.invalid/"
owner_token={{file={secret:?}}}
server_id="synthetic"
[[arr]]
id="films"
kind={kind:?}
url="https://arr.invalid/"
api_key={{file={secret:?}}}
libraries=["1"]
[[arr]]
id="other"
kind={kind:?}
url="https://other.invalid/"
api_key={{file={secret:?}}}
libraries=["2"]
"#,
            engine = dir.path().join("engine.sqlite").to_str().unwrap(),
            intake = dir.path().join("intake.sqlite").to_str().unwrap(),
            secret = secret.to_str().unwrap(),
        ))
        .unwrap();
        let store = Store::open(&config.database).await.unwrap();
        let fake = Fake::default();
        let (item_path, file_path, current) = if kind == "radarr" {
            (
                "/api/v3/movie/9",
                "/api/v3/moviefile/12",
                json!({"movieFileId":12}),
            )
        } else {
            (
                "/api/v3/episode/9",
                "/api/v3/episodefile/12",
                json!({"episodeFileId":12}),
            )
        };
        fake.reply(item_path, current);
        fake.reply(file_path, file());
        fake.reply(
            "/api/v3/release",
            json!([candidate("blocked"), candidate("working")]),
        );
        let runtime = Runtime::open(&config, store, fake.clone(), Arc::new(|| 100_000))
            .await
            .unwrap()
            .unwrap();
        Self {
            _dir: dir,
            runtime,
            fake,
            target: ResolvedTarget {
                item: CatalogItem {
                    id: "plex:primary:1:5".into(),
                    title: "Synthetic Film".into(),
                    aliases: vec![],
                    library_id: "1".into(),
                    library_name: "Films".into(),
                    resolution: Resolution::Hd,
                    season: None,
                    episode: None,
                },
                resource: "arr:films:movie:9".into(),
                arr_instance_id: "films".into(),
                arr_item_id: 9,
                arr_file_id: 12,
                agent_path: "/agent/film.mkv".into(),
                agent_file: Some(agent()),
                plex_rating_key: "5".into(),
                observed_at: 100,
                expires_at: 1000,
            },
            file_path,
            item_path,
        }
    }

    async fn job(&mut self) -> Lease {
        let fingerprint = self
            .runtime
            .fingerprint(&self.target, Some(agent()))
            .await
            .unwrap();
        let original = self
            .runtime
            .store
            .register_file("films", &self.target.resource, &fingerprint, 100)
            .await
            .unwrap();
        self.runtime
            .store
            .create_job(
                &NewJob {
                    id: 1,
                    viewer: "viewer".into(),
                    resource: self.target.resource.clone(),
                    library: 1,
                    original,
                    constraints: Constraints::default(),
                    policy: Policy {
                        max_grabs: 3,
                        stage_timeout: 30,
                        total_timeout: 3600,
                    },
                    timeouts: StageTimeouts {
                        download: 60,
                        import: 40,
                        decode: 50,
                        verify_retry: 20,
                    },
                    created_at: 100,
                },
                "create",
                BudgetLimits {
                    global_hour: 10,
                    global_day: 10,
                    viewer_hour: 10,
                    viewer_day: 10,
                },
            )
            .await
            .unwrap();
        self.runtime.authorities.insert(
            ("viewer".into(), self.target.resource.clone()),
            Prepared {
                target: self.target.clone(),
                access: access(),
            },
        );
        self.fake.calls.lock().unwrap().clear();
        self.runtime
            .store
            .acquire_lease(1, "test-selection", 100, 600)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn fingerprints_preserve_every_observed_component_and_unknown_agent() {
    for kind in ["radarr", "sonarr"] {
        let s = Fixture::new(kind).await;
        let mut observed = file();
        if kind == "sonarr" {
            observed.as_object_mut().unwrap().remove("movieId");
        }
        observed["size"] = json!(1);
        s.fake.reply(s.file_path, observed);
        let expected = IdentityFingerprint {
            arr_file_id: 12,
            size: 1,
            date_added: "2026-01-01T00:00:00Z".into(),
            quality: quality().to_string(),
            agent: agent(),
        };
        assert_eq!(
            s.runtime
                .fingerprint(&s.target, Some(agent()))
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            s.runtime.fingerprint(&s.target, None).await.unwrap(),
            IdentityFingerprint {
                agent: FileIdentity { id: 0, revision: 0 },
                ..expected
            }
        );
        assert_eq!(
            *s.fake.calls.lock().unwrap(),
            vec![s.item_path, s.file_path, s.item_path, s.file_path]
        );
    }
}

#[tokio::test]
async fn fingerprints_reject_missing_and_malformed_metadata_independently() {
    let s = Fixture::new("radarr").await;
    for (field, bad, expected) in [
        ("path", Value::Null, "arr file path unavailable"),
        ("id", json!(13), "arr file mapping changed"),
        ("movieId", Value::Null, "arr file mapping changed"),
        ("movieId", json!(10), "arr file mapping changed"),
        ("size", Value::Null, "arr file size unavailable"),
        ("size", json!(0), "arr file size unavailable"),
        ("dateAdded", Value::Null, "arr file date unavailable"),
        ("dateAdded", json!(""), "arr file date unavailable"),
        ("dateAdded", json!(" \n\t"), "arr file date unavailable"),
        ("quality", Value::Null, "arr quality unavailable"),
        ("quality", json!("HD"), "arr quality unavailable"),
        ("quality", json!([]), "arr quality unavailable"),
        ("quality", json!({}), "arr quality unavailable"),
        ("quality", json!({"quality":{}}), "arr quality unavailable"),
        (
            "quality",
            json!({"quality":{"id":0}}),
            "arr quality unavailable",
        ),
        (
            "quality",
            json!({"quality":{"id":-1}}),
            "arr quality unavailable",
        ),
        (
            "quality",
            json!({"quality":{"id":"7"}}),
            "arr quality unavailable",
        ),
    ] {
        let mut observed = file();
        observed[field] = bad.clone();
        s.fake.reply(s.file_path, observed);
        assert_eq!(
            s.runtime.fingerprint(&s.target, Some(agent())).await,
            Err(expected),
            "{field}={bad}"
        );
    }
}

#[tokio::test]
async fn fingerprints_require_current_identity_and_both_library_binding_keys() {
    let s = Fixture::new("radarr").await;
    s.fake.reply(s.item_path, json!({"movieFileId":13}));
    assert_eq!(
        s.runtime.fingerprint(&s.target, Some(agent())).await,
        Err("installed arr identity changed")
    );
    assert_eq!(*s.fake.calls.lock().unwrap(), vec![s.item_path]);
    s.fake.reply(s.item_path, json!({"movieFileId":12}));
    for target in [
        ResolvedTarget {
            item: CatalogItem {
                library_id: "2".into(),
                ..s.target.item.clone()
            },
            ..s.target.clone()
        },
        ResolvedTarget {
            arr_instance_id: "other".into(),
            ..s.target.clone()
        },
    ] {
        assert_eq!(
            s.runtime.fingerprint(&target, Some(agent())).await,
            Err("library binding unavailable")
        );
    }
}

#[tokio::test]
async fn fingerprints_reject_empty_traversing_outside_and_differently_mapped_paths() {
    let s = Fixture::new("radarr").await;
    for (path, agent_path, expected) in [
        ("/arr", "/agent", "arr file mapping changed"),
        (
            "/arr/../escape.mkv",
            "/agent/../escape.mkv",
            "arr file mapping changed",
        ),
        (
            "/arr/deep/../../escape.mkv",
            "/agent/deep/../../escape.mkv",
            "arr file mapping changed",
        ),
        (
            "/arr-other/film.mkv",
            "/agent/film.mkv",
            "arr file outside mapped root",
        ),
        (
            "arr/film.mkv",
            "/agent/film.mkv",
            "arr file outside mapped root",
        ),
        (
            "/arr/film.mkv",
            "/different/film.mkv",
            "arr file mapping changed",
        ),
    ] {
        let mut observed = file();
        observed["path"] = json!(path);
        s.fake.reply(s.file_path, observed);
        let mut target = s.target.clone();
        target.agent_path = agent_path.into();
        assert_eq!(
            s.runtime.fingerprint(&target, Some(agent())).await,
            Err(expected),
            "{path} -> {agent_path}"
        );
    }
}

#[tokio::test]
async fn selection_skips_global_and_job_exclusions_by_id_or_guid_but_not_other_scopes() {
    use pitcrew_store::policy::{ExclusionKind, NewExclusion};
    // Fixed vectors assert that the release identity remains stable across restarts.
    const BLOCKED: u64 = 3_608_154_334_816_334_575;
    const WORKING: u64 = 7_867_616_952_826_437_831;
    for mode in [
        "global_id",
        "global_guid",
        "job_id",
        "job_guid",
        "released",
        "other_client",
    ] {
        let mut s = Fixture::new("radarr").await;
        let lease = s.job().await;
        let local = mode.starts_with("job_");
        let id_only = mode.ends_with("_id");
        let release = if id_only { BLOCKED } else { 99 };
        let identity = if id_only {
            ReleaseIdentity::default()
        } else {
            identity("blocked")
        };
        if local {
            let selected = s
                .runtime
                .store
                .apply_event(
                    &lease,
                    0,
                    "previous-attempt",
                    Event::SelectCandidate {
                        candidate: ReleaseCandidate {
                            release,
                            identity,
                            quality: QualityRelation::SameOrBetter,
                        },
                        access: access(),
                    },
                    100,
                )
                .await
                .unwrap();
            s.runtime
                .store
                .apply_event(
                    &lease,
                    selected.stored.revision,
                    "previous-failure",
                    Event::DownloadFailed {
                        grab: selected.stored.job.expected_observation().unwrap(),
                        download: 42,
                    },
                    100,
                )
                .await
                .unwrap();
            assert_eq!(
                s.runtime.store.load_job(1).await.unwrap().job.state(),
                State::Rejected
            );
        } else {
            let exclusion = s
                .runtime
                .store
                .exclude_release(
                    &NewExclusion {
                        client: if mode == "other_client" {
                            "other"
                        } else {
                            "films"
                        }
                        .into(),
                        release,
                        identity,
                        kind: ExclusionKind::DeadRelease,
                        reason: "Synthetic authoritative download failure".into(),
                        evidence: json!({"failed":true}),
                        pinned: false,
                        created_at: 100,
                    },
                    "exclude",
                )
                .await
                .unwrap();
            if mode == "released" {
                s.runtime
                    .store
                    .release_exclusion(
                        exclusion.id,
                        &pitcrew_store::policy::AdminAction {
                            actor: "operator".into(),
                            role: pitcrew_store::policy::PolicyRole::Admin,
                            note: "Reviewed synthetic release".into(),
                        },
                        100,
                    )
                    .await
                    .unwrap();
            }
        }
        s.runtime.select(&lease).await.unwrap();
        let outbox = s.runtime.store.outbox(1).await.unwrap();
        let selected = if matches!(mode, "other_client" | "released") {
            BLOCKED
        } else {
            WORKING
        };
        assert_eq!(outbox.len(), if local { 2 } else { 1 }, "{mode}");
        assert!(
            matches!(outbox.last().unwrap().intent.effect, Effect::Grab { release, .. } if release == selected),
            "{mode}"
        );
        assert_eq!(
            s.runtime.store.load_job(1).await.unwrap().job.state(),
            State::GrabIntent
        );
    }
}

#[tokio::test]
async fn selection_requires_a_current_grant_for_the_jobs_library() {
    for grant in [
        Access::Unknown,
        Access::Denied,
        Access::Granted {
            library: 2,
            expires_at: 1000,
        },
        Access::Granted {
            library: 1,
            expires_at: 100,
        },
        Access::Granted {
            library: 1,
            expires_at: 99,
        },
    ] {
        let mut s = Fixture::new("radarr").await;
        let lease = s.job().await;
        s.runtime.authorities.values_mut().next().unwrap().access = grant;
        assert_eq!(
            s.runtime.select(&lease).await,
            Err("access or installed identity changed during selection"),
            "{grant:?}"
        );
        assert!(s.runtime.store.outbox(1).await.unwrap().is_empty());
        assert_eq!(*s.fake.calls.lock().unwrap(), vec!["/api/v3/release"]);
    }
}

#[tokio::test]
async fn selection_requires_the_current_original_and_rereads_full_arr_metadata() {
    for field in ["size", "dateAdded", "quality", "registry"] {
        let mut s = Fixture::new("radarr").await;
        let lease = s.job().await;
        let expected = if field == "registry" {
            let mut fingerprint = s
                .runtime
                .fingerprint(&s.target, Some(agent()))
                .await
                .unwrap();
            fingerprint.agent.revision += 1;
            s.runtime
                .store
                .register_file("films", &s.target.resource, &fingerprint, 100)
                .await
                .unwrap();
            s.fake.calls.lock().unwrap().clear();
            "original file changed before selection"
        } else {
            let mut observed = file();
            observed[field] = match field {
                "size" => json!(100_000_002),
                "dateAdded" => json!("2026-01-02T00:00:00Z"),
                "quality" => json!({"quality":{"id":7},"revision":{"version":3,"real":1}}),
                _ => unreachable!(),
            };
            s.fake.reply(s.file_path, observed);
            "access or installed identity changed during selection"
        };
        assert_eq!(s.runtime.select(&lease).await, Err(expected), "{field}");
        assert!(s.runtime.store.outbox(1).await.unwrap().is_empty());
        if field == "registry" {
            assert!(s.fake.calls.lock().unwrap().is_empty());
        } else {
            assert_eq!(
                *s.fake.calls.lock().unwrap(),
                vec!["/api/v3/release", s.item_path, s.file_path]
            );
        }
    }
}

#[test]
fn catalog_export_replaces_a_relative_basename_with_private_valid_json() {
    let destination = tempfile::NamedTempFile::new_in(".").unwrap();
    std::fs::write(destination.path(), b"old incomplete catalog").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination.path(), std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
    let relative = std::path::Path::new(destination.path().file_name().unwrap());
    assert!(relative.is_relative());
    assert_eq!(relative.components().count(), 1);
    let catalog = CatalogSnapshot::default();
    catalog::export(relative, &catalog).unwrap();
    let bytes = std::fs::read(destination.path()).unwrap();
    let exported: CatalogSnapshot = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        serde_json::to_value(exported).unwrap(),
        serde_json::to_value(catalog).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(destination.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn release_size_requires_seventy_mib_for_same_and_bounded_lower_quality() {
    for lower_quality in [false, true] {
        for (case, size, accepted) in [
            ("missing", None, false),
            ("null", Some(Value::Null), false),
            ("zero", Some(json!(0)), false),
            ("small", Some(json!(1_048_576)), false),
            ("below_floor", Some(json!(73_400_319)), false),
            ("at_floor", Some(json!(73_400_320)), true),
        ] {
            let mut f = Fixture::new("radarr").await;
            f.runtime.lower_quality_minimum = Some(1080);
            let mut original = file();
            original["quality"]["quality"]["resolution"] = json!(2160);
            f.fake.reply(f.file_path, original.clone());
            let lease = f.job().await;
            let stored = f.runtime.store.load_job(1).await.unwrap();
            let before = f
                .runtime
                .store
                .apply_event(
                    &lease,
                    stored.revision,
                    "broken",
                    Event::BrokenFileConfirmed {
                        file: stored.job.current_original(),
                    },
                    100,
                )
                .await
                .unwrap()
                .stored;
            let mut release = candidate("size-boundary");
            release["quality"] = original["quality"].clone();
            if lower_quality {
                release["quality"]["quality"]["resolution"] = json!(1080);
            }
            match size {
                Some(size) => release["size"] = size,
                None => {
                    release.as_object_mut().unwrap().remove("size");
                }
            }
            f.fake.reply("/api/v3/release", json!([release]));
            f.runtime.select(&lease).await.unwrap();
            let after = f.runtime.store.load_job(1).await.unwrap();
            let outbox = f.runtime.store.outbox(1).await.unwrap();
            if !accepted {
                assert_eq!(after, before, "{case}, lower_quality={lower_quality}");
                assert!(outbox.is_empty());
            } else if lower_quality {
                assert_eq!(after.job.state(), State::AwaitingQualityApproval);
                assert_eq!(after.job.reserved_budget(), 0);
                assert!(outbox.is_empty());
                let (_, bound) = after.job.bounded_lower_quality().unwrap();
                assert_eq!(bound.minimum_resolution, 1080);
                assert_eq!(bound.candidate_resolution, 1080);
                assert_eq!(bound.original_resolution, 2160);
            } else {
                assert_eq!(after.job.state(), State::GrabIntent);
                assert_eq!(after.job.reserved_budget(), 1);
                assert_eq!(outbox.len(), 1);
                assert!(matches!(outbox[0].intent.effect, Effect::Grab { .. }));
            }
            let pool = sqlx::SqlitePool::connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(f._dir.path().join("intake.sqlite")),
            )
            .await
            .unwrap();
            let proofs: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                proofs,
                i64::from(accepted && lower_quality),
                "{case}, lower_quality={lower_quality}"
            );
        }
    }
}

#[tokio::test]
async fn lower_quality_selection_requires_explicit_floor_and_structured_resolution() {
    for (minimum, source, replacement, expected) in [
        (None, json!(2160), json!(1080), false),
        (Some(1080), json!(2160), json!(1080), true),
        (Some(720), json!(2160), json!(1080), true),
        (Some(1080), json!(2160), json!(720), false),
        (Some(720), json!(1080), json!(1080), false),
        (Some(720), json!(1080), json!(2160), false),
        (Some(720), json!(null), json!(1080), false),
        (Some(720), json!(2160), json!(null), false),
        (Some(720), json!(2160), json!("1080"), false),
        (Some(720), json!(2160), json!(0), false),
        (Some(720), json!(2160), json!(u64::MAX), false),
    ] {
        let mut f = Fixture::new("radarr").await;
        f.runtime.lower_quality_minimum = minimum;
        let mut original = file();
        original["quality"]["quality"]["resolution"] = source;
        f.fake.reply(f.file_path, original);
        let lease = f.job().await;
        let stored = f.runtime.store.load_job(1).await.unwrap();
        f.runtime
            .store
            .apply_event(
                &lease,
                stored.revision,
                "broken",
                Event::BrokenFileConfirmed {
                    file: stored.job.current_original(),
                },
                100,
            )
            .await
            .unwrap();
        let mut lower = candidate("lower");
        lower["quality"]["quality"]["id"] = json!(1);
        lower["quality"]["quality"]["resolution"] = replacement;
        f.fake.reply("/api/v3/release", json!([lower]));
        f.runtime.select(&lease).await.unwrap();
        let job = f.runtime.store.load_job(1).await.unwrap().job;
        assert_eq!(job.state() == State::AwaitingQualityApproval, expected);
        if expected {
            let (_, bound) = job.bounded_lower_quality().unwrap();
            assert_eq!(bound.minimum_resolution, minimum.unwrap());
            assert_eq!(bound.candidate_resolution, 1080);
            assert_eq!(bound.original_resolution, 2160);
        }
        assert_eq!(job.reserved_budget(), 0);
        assert!(f.runtime.store.outbox(1).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn lower_quality_search_prefers_equal_quality_and_refreshes_only_the_pending_release() {
    for same_available in [true, false] {
        let mut f = Fixture::new("sonarr").await;
        f.runtime.lower_quality_minimum = Some(1080);
        let mut original = file();
        original["quality"]["quality"]["resolution"] = json!(2160);
        f.fake.reply(f.file_path, original.clone());
        let lease = f.job().await;
        let stored = f.runtime.store.load_job(1).await.unwrap();
        f.runtime
            .store
            .apply_event(
                &lease,
                stored.revision,
                "broken",
                Event::BrokenFileConfirmed {
                    file: stored.job.current_original(),
                },
                100,
            )
            .await
            .unwrap();
        let mut lower = candidate("lower");
        lower["quality"]["quality"]["resolution"] = json!(1080);
        let mut same = candidate("same");
        same["quality"] = original["quality"].clone();
        f.fake.reply(
            "/api/v3/release",
            if same_available {
                json!([lower, same])
            } else {
                json!([lower.clone()])
            },
        );
        f.runtime.select(&lease).await.unwrap();
        let pending = f.runtime.store.load_job(1).await.unwrap();
        if same_available {
            assert_eq!(pending.job.state(), State::GrabIntent);
            continue;
        }
        assert_eq!(pending.job.state(), State::AwaitingQualityApproval);
        f.runtime.store.release_lease(&lease, 100).await.unwrap();
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f._dir.path().join("intake.sqlite")),
        )
        .await
        .unwrap();
        for refresh in [
            "same",
            "other",
            "none",
            "size_missing",
            "size_below_floor",
            "disabled",
            "floor_changed",
            "malformed",
            "timeout",
            "transport",
            "busy",
            "revoked",
        ] {
            // Every refusal starts with a usable proof. An empty table from the
            // preceding refusal cannot make invalidation look successful.
            *f.fake.failure.lock().unwrap() = None;
            lower["guid"] = json!("lower");
            lower["size"] = json!(100_000_000);
            f.fake.reply("/api/v3/release", json!([lower.clone()]));
            f.runtime.search_enabled = true;
            f.runtime.lower_quality_minimum = Some(1080);
            f.runtime.authorities.values_mut().next().unwrap().access = access();
            f.runtime.refresh_lower_quality().await.unwrap();
            let seeded: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(seeded, 1, "{refresh}");
            let busy = if refresh == "busy" {
                Some(
                    f.runtime
                        .store
                        .acquire_lease(1, "another-worker", 100, 60)
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            if refresh == "revoked" {
                f.runtime.authorities.values_mut().next().unwrap().access = Access::Denied;
            }
            lower["guid"] = json!(if refresh == "other" { "other" } else { "lower" });
            if refresh == "size_missing" {
                lower.as_object_mut().unwrap().remove("size");
            } else if refresh == "size_below_floor" {
                lower["size"] = json!(73_400_319);
            }
            f.fake.reply(
                "/api/v3/release",
                if refresh == "none" {
                    json!([])
                } else if refresh == "malformed" {
                    json!({"unsupported":true})
                } else {
                    json!([lower.clone()])
                },
            );
            f.runtime.search_enabled = refresh != "disabled";
            f.runtime.lower_quality_minimum = Some(if refresh == "floor_changed" {
                720
            } else {
                1080
            });
            *f.fake.failure.lock().unwrap() = match refresh {
                "timeout" => Some(pitcrew_upstream::Error::Timeout),
                "transport" => Some(pitcrew_upstream::Error::Transport),
                _ => None,
            };
            f.runtime.refresh_lower_quality().await.unwrap();
            if let Some(busy) = busy {
                f.runtime.store.release_lease(&busy, 100).await.unwrap();
            }
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, i64::from(refresh == "same"), "{refresh}");
            assert_eq!(f.runtime.store.load_job(1).await.unwrap(), pending);
        }
        f.runtime.search_enabled = true;
        f.runtime.lower_quality_minimum = Some(1080);
        f.runtime.authorities.values_mut().next().unwrap().access = access();
        f.runtime.refresh_lower_quality().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        f.runtime.authorities.clear();
        f.runtime.refresh_lower_quality().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_lower_quality")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "a batch without current authority must revoke old proof"
        );
    }
}
