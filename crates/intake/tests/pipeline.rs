use pitcrew_deciders::{
    Decider, DeciderKind, DecisionFuture, DecisionReport, RedactedEvidence, providers::Usage,
};
use pitcrew_diagnosis::{
    collectors::*,
    facts::*,
    rules::{Reason, RecommendedAction},
};
use pitcrew_domain::{
    FileIdentity,
    channels::{ChannelIdentity, IncomingMessage},
    media::{MediaEvidence, MediaProbe, ProbeOutcome as Probe, StreamFacts},
};
use pitcrew_intake::{pipeline::*, *};
use pitcrew_intake_store::Store;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

struct Catalog(Mutex<Result<ResolvedTarget, ResolveError>>);
impl CatalogResolver for Catalog {
    fn resolve(&self, _: &str, _: u64) -> Result<ResolvedTarget, ResolveError> {
        self.0.lock().unwrap().clone()
    }
}
struct Access(Mutex<Option<AccessGrant>>);
impl AccessProvider for Access {
    fn access(&self, _: &str, _: u64) -> Option<AccessGrant> {
        self.0.lock().unwrap().clone()
    }
}
struct Evidence {
    agent: AgentEvidence,
    calls: AtomicUsize,
    after: Option<Box<dyn Fn() + Send + Sync>>,
}
impl EvidenceSource for Evidence {
    fn collect<'a>(
        &'a self,
        target: &'a ResolvedTarget,
        account: &'a str,
        _: &'a Clock,
    ) -> EvidenceFuture<'a> {
        assert_eq!(target.agent_path, "/media/synthetic.mkv");
        assert_eq!(account, "17");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(after) = &self.after {
                after();
            }
            CollectedEvidence {
                agent: self.agent.clone(),
                sessions: Observation::Available(SessionEvidence {
                    transcodes: 2,
                    ..SessionEvidence::default()
                }),
            }
        })
    }
}
fn target() -> ResolvedTarget {
    ResolvedTarget {
        item: CatalogItem {
            id: "scope:episode:2".into(),
            title: "Synthetic Series".into(),
            aliases: vec![],
            library_id: "library-a".into(),
            library_name: "Series".into(),
            resolution: Resolution::Hd,
            season: Some(1),
            episode: Some(2),
        },
        resource: "arr:a:episode:2".into(),
        arr_instance_id: "arr-a".into(),
        arr_item_id: 2,
        arr_file_id: 9,
        agent_path: "/media/synthetic.mkv".into(),
        agent_file: Some(FileIdentity {
            id: 50,
            revision: 3,
        }),
        plex_rating_key: "12".into(),
        observed_at: 900,
        expires_at: 2000,
    }
}
fn grant() -> AccessGrant {
    AccessGrant {
        account_id: "17".into(),
        library_ids: vec!["library-a".into()],
        observed_at: 900,
        expires_at: 2000,
    }
}
fn media() -> MediaEvidence {
    MediaEvidence {
        file: FileIdentity {
            id: 50,
            revision: 3,
        },
        observed_at_unix_ms: 1_000_000,
        symlinks: None,
        mount: None,
        reads: vec![],
        header: Some(MediaProbe {
            outcome: Probe::Passed,
            duration_ms: Some(90_000),
            elapsed_ms: 1,
            timeout_ms: 1000,
            faults: vec![],
            streams: vec![StreamFacts::Video {
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                bit_depth: Some(8),
                dynamic_range: None,
            }],
            tested_ranges: vec![],
        }),
        decode: Some(MediaProbe {
            outcome: Probe::Failed,
            duration_ms: Some(90_000),
            elapsed_ms: 1,
            timeout_ms: 1000,
            faults: vec![],
            streams: vec![],
            tested_ranges: vec![],
        }),
        plex_failures: vec![],
    }
}
fn evidence() -> Evidence {
    Evidence {
        agent: AgentEvidence {
            media: Observation::Available(media()),
            plex: Observation::Available(PlexContext {
                matching_failures: 2,
                ..PlexContext::default()
            }),
        },
        calls: AtomicUsize::new(0),
        after: None,
    }
}
async fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    (dir, store)
}
fn response(kind: IssueKind) -> Value {
    let t = target();
    serde_json::to_value(IntakeService.handle_selected(
        &IncomingMessage {
            sender: ChannelIdentity {
                channel_id: "web".into(),
                platform_user_id: "17".into(),
            },
            message_id: "message".into(),
            conversation_id: "conversation".into(),
            text: "private viewer text /not-for-cloud token=secret".into(),
            received_at: 999,
        },
        Some(&AuthorizedViewer {
            account_id: "17".into(),
            library_ids: vec!["library-a".into()],
        }),
        &[t.item],
        "scope:episode:2",
        &[kind],
    ))
    .unwrap()
}
async fn persist(store: &Store, value: Value) -> i64 {
    store
        .persist_intake("web", "message", "17", "conversation", &value, 999)
        .await
        .unwrap();
    store.reports("17").await.unwrap()[0].id
}

#[tokio::test]
async fn selected_report_collects_exact_file_runs_rules_and_commits_redacted_seam_once() {
    let (dir, store) = setup().await;
    let id = persist(&store, response(IssueKind::Playback)).await;
    let t = target();
    let c = Catalog(Mutex::new(Ok(t.clone())));
    let a = Access(Mutex::new(Some(grant())));
    let e = evidence();
    let p = Pipeline::new(&store, &c, &a, &e, &|| 1_000_000);
    assert_eq!(p.diagnose(id).await.unwrap(), Outcome::Persisted);
    assert_eq!(p.diagnose(id).await.unwrap(), Outcome::AlreadyDecided);
    assert_eq!(e.calls.load(Ordering::SeqCst), 1);
    let d = store.diagnosis(id).await.unwrap().unwrap();
    assert_eq!(d.decision.decision.action, RecommendedAction::Replace);
    assert_eq!(d.decision.decision.reason, Reason::DecodeFailed);
    assert_eq!(d.resource, t.resource);
    assert_eq!(d.library_id, t.item.library_id);
    assert_eq!(d.arr_instance_id, t.arr_instance_id);
    assert_eq!(d.arr_item_id, 2);
    assert_eq!(d.arr_file_id, 9);
    assert_eq!(d.agent_file, t.agent_file);
    assert_eq!(d.agent_observed_at, Some(1000));
    assert_eq!(
        (d.access_observed_at, d.access_expires_at, d.decided_at),
        (900, 2000, 1000)
    );
    assert_eq!(d.evidence_summary.playback.sessions.transcodes, 2);
    assert_eq!(d.evidence_summary.plex_logs.matching_failures, 2);
    let comparison = store.diagnosis_comparison(id).await.unwrap().unwrap();
    assert_eq!(comparison["primary"], "rules");
    let public = serde_json::to_string(&(d.evidence_summary, comparison)).unwrap();
    for private in ["Synthetic", "/media", "token=", "viewer", "scope:"] {
        assert!(!public.contains(private));
    }
    let reopened = Store::open(dir.path().join("intake.sqlite")).await.unwrap();
    assert_eq!(reopened.diagnosis(id).await.unwrap(), Some(d));
}

#[tokio::test]
async fn unsupported_reports_and_resolution_failures_never_collect() {
    for (value, mapping, expected) in [
        (
            json!({"outcome":"clarify","issues":[],"clarification":"episode","report_ids":[],"reply":"Choose an episode","suspicious_input":false}),
            Ok(target()),
            Outcome::Ignored,
        ),
        (
            {
                let mut v = response(IssueKind::Playback);
                v["issues"] = json!([]);
                v
            },
            Ok(target()),
            Outcome::Clarify(Clarification::Title),
        ),
        (
            {
                let mut v = response(IssueKind::Playback);
                v["issues"] = json!([v["issues"][0], v["issues"][0]]);
                v
            },
            Ok(target()),
            Outcome::Clarify(Clarification::Title),
        ),
        (
            response(IssueKind::Playback),
            Err(ResolveError::EpisodeRequired),
            Outcome::Clarify(Clarification::Episode),
        ),
        (
            response(IssueKind::Playback),
            Err(ResolveError::Ambiguous),
            Outcome::Clarify(Clarification::Library),
        ),
        (
            response(IssueKind::Playback),
            Err(ResolveError::Unavailable),
            Outcome::CatalogUnavailable,
        ),
    ] {
        let (_dir, s) = setup().await;
        let id = persist(&s, value).await;
        let c = Catalog(Mutex::new(mapping));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        assert_eq!(
            Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
                .diagnose(id)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(e.calls.load(Ordering::SeqCst), 0);
        assert!(s.diagnosis(id).await.unwrap().is_none());
    }
    let (_dir, s) = setup().await;
    let c = Catalog(Mutex::new(Ok(target())));
    let a = Access(Mutex::new(Some(grant())));
    let e = evidence();
    assert!(matches!(
        Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
            .diagnose(900)
            .await,
        Err(Error::InvalidReport)
    ));
    let id = persist(&s, json!({"bad":true})).await;
    assert!(matches!(
        Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
            .diagnose(id)
            .await,
        Err(Error::InvalidReport)
    ));
}

#[tokio::test]
async fn stale_unknown_mismatched_and_revoked_access_never_authorizes_collection() {
    let mut cases = vec![None];
    for change in 0..5 {
        let mut g = grant();
        match change {
            0 => g.account_id = "other".into(),
            1 => g.library_ids.clear(),
            2 => g.expires_at = 1000,
            3 => g.observed_at = 1001,
            _ => g.library_ids = vec!["different".into()],
        };
        cases.push(Some(g));
    }
    for grant in cases {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(IssueKind::Playback)).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(grant));
        let e = evidence();
        assert_eq!(
            Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
                .diagnose(id)
                .await
                .unwrap(),
            Outcome::Unauthorized
        );
        assert_eq!(e.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn invalid_catalog_fields_and_whole_show_cannot_become_exact_diagnoses() {
    for change in 0..12 {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(IssueKind::Playback)).await;
        let mut t = target();
        match change {
            0 => t.item.id = "different".into(),
            1 => t.resource.clear(),
            2 => t.item.library_id.clear(),
            3 => t.arr_instance_id.clear(),
            4 => t.arr_item_id = 0,
            5 => t.arr_file_id = 0,
            6 => t.agent_path.clear(),
            7 => t.observed_at = 1001,
            8 => t.expires_at = 1000,
            9 => t.item.episode = None,
            10 => t.item.season = None,
            _ => {
                t.observed_at = 1000;
                t.expires_at = 1001;
            }
        }
        let c = Catalog(Mutex::new(Ok(t)));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        let expected = match change {
            9 | 10 => Outcome::Clarify(Clarification::Episode),
            11 => Outcome::Persisted,
            _ => Outcome::CatalogUnavailable,
        };
        assert_eq!(
            Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
                .diagnose(id)
                .await
                .unwrap(),
            expected,
            "change {change}"
        );
    }
}

#[tokio::test]
async fn missing_or_stale_identity_is_unknown_and_first_probe_can_bind_identity() {
    for change in 0..7 {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(IssueKind::Playback)).await;
        let mut e = evidence();
        let mut t = target();
        let mut m = media();
        match change {
            0 => m.file.id = 0,
            1 => m.file.revision += 1,
            2 => m.observed_at_unix_ms = 1_000_001,
            3 => m.observed_at_unix_ms = 699_999,
            4 => m.observed_at_unix_ms = 700_000,
            5 => t.agent_file = None,
            _ => (),
        }
        e.agent.media = if change == 6 {
            Observation::Unavailable(Unavailable::Timeout)
        } else {
            Observation::Available(m)
        };
        let c = Catalog(Mutex::new(Ok(t)));
        let a = Access(Mutex::new(Some(grant())));
        assert_eq!(
            Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
                .diagnose(id)
                .await
                .unwrap(),
            Outcome::Persisted
        );
        let d = s.diagnosis(id).await.unwrap().unwrap();
        if matches!(change, 4 | 5) {
            assert_eq!(d.decision.decision.action, RecommendedAction::Replace);
            assert!(d.agent_file.is_some());
        } else {
            assert_ne!(d.decision.decision.action, RecommendedAction::Replace);
            assert!(d.agent_file.is_none());
            assert!(d.agent_observed_at.is_none());
        }
    }
}

#[tokio::test]
async fn lease_and_catalog_changes_during_collection_fail_closed() {
    for change in 0..5 {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(IssueKind::Playback)).await;
        let c = Arc::new(Catalog(Mutex::new(Ok(target()))));
        let a = Arc::new(Access(Mutex::new(Some(grant()))));
        let now = Arc::new(AtomicU64::new(1_000_000));
        let mut e = evidence();
        let (cc, aa, nn) = (c.clone(), a.clone(), now.clone());
        e.after = Some(Box::new(move || match change {
            0 => {
                nn.store(2_000_000, Ordering::SeqCst);
            }
            1 => {
                *aa.0.lock().unwrap() = None;
            }
            2 => {
                cc.0.lock().unwrap().as_mut().unwrap().arr_file_id = 10;
            }
            3 => {
                nn.store(1_300_001, Ordering::SeqCst);
            }
            _ => {
                cc.0.lock().unwrap().as_mut().unwrap().expires_at = 1000;
            }
        }));
        let clock_now = now.clone();
        let clock = move || clock_now.load(Ordering::SeqCst);
        let outcome = Pipeline::new(&s, c.as_ref(), a.as_ref(), &e, &clock)
            .diagnose(id)
            .await
            .unwrap();
        if change <= 1 {
            assert_eq!(outcome, Outcome::Unauthorized);
        } else if change == 3 {
            assert_eq!(outcome, Outcome::Persisted);
            assert!(s.diagnosis(id).await.unwrap().unwrap().agent_file.is_none());
        } else {
            assert_eq!(outcome, Outcome::ChangedDuringCollection);
        }
    }
}

#[tokio::test]
async fn wizard_advisory_branches_do_not_replace_even_when_bytes_are_bad() {
    for (kind, issue, answer, action) in [
        (
            IssueKind::Buffering,
            "buffering",
            "helped",
            RecommendedAction::Close,
        ),
        (
            IssueKind::Buffering,
            "buffering",
            "not_tried",
            RecommendedAction::NotifyAdmin,
        ),
        (
            IssueKind::Playback,
            "ads",
            "on_server",
            RecommendedAction::ExplainPlayOrigin,
        ),
        (
            IssueKind::Playback,
            "other",
            "details",
            RecommendedAction::NotifyAdmin,
        ),
        (
            IssueKind::Playback,
            "playback",
            "everything_broken",
            RecommendedAction::NotifyAdmin,
        ),
        (
            IssueKind::Subtitles,
            "subtitles",
            "english",
            RecommendedAction::NotifyAdmin,
        ),
        (
            IssueKind::NoAudio,
            "no_audio",
            "checked",
            RecommendedAction::Replace,
        ),
        (
            IssueKind::WrongEpisode,
            "wrong_episode",
            "different_episode",
            RecommendedAction::Replace,
        ),
        (
            IssueKind::Quality,
            "quality",
            "whole_time",
            RecommendedAction::Replace,
        ),
        (
            IssueKind::Playback,
            "video_broken",
            "whole_time",
            RecommendedAction::Replace,
        ),
    ] {
        let (_dir, s) = setup().await;
        let mut v = response(kind);
        v["wizard"] = json!({"issue":issue,"answer":answer,"details":"private"});
        if matches!(issue, "ads" | "other") {
            v["issues"][0]["kinds"] = json!([]);
        }
        let id = persist(&s, v).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        assert_eq!(
            Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
                .diagnose(id)
                .await
                .unwrap(),
            Outcome::Persisted
        );
        assert_eq!(
            s.diagnosis(id)
                .await
                .unwrap()
                .unwrap()
                .decision
                .decision
                .action,
            action,
            "{issue} {answer}"
        );
    }
}

struct Shadow {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Value>>>,
    now: Option<(Arc<AtomicU64>, u64)>,
}
impl Decider for Shadow {
    fn kind(&self) -> DeciderKind {
        DeciderKind::Anthropic
    }
    fn decide<'a>(&'a self, e: &'a RedactedEvidence) -> DecisionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push(serde_json::to_value(e).unwrap());
        if let Some((now, at)) = &self.now {
            now.store(*at, Ordering::SeqCst);
        }
        Box::pin(async {
            Ok(DecisionReport {
                decision: pitcrew_diagnosis::rules::Decision {
                    kind: pitcrew_diagnosis::rules::DiagnosisKind::SelfResolved,
                    action: RecommendedAction::Close,
                    constraint: None,
                    reason: Reason::SelfResolved,
                },
                usage: Usage::default(),
            })
        })
    }
}
#[tokio::test]
async fn shadows_require_consent_never_override_rules_and_cannot_extend_evidence_freshness() {
    for delayed in [false, true] {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(IssueKind::Playback)).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock_now = now.clone();
        let clock = move || clock_now.load(Ordering::SeqCst);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(vec![]));
        let shadow = || {
            Box::new(Shadow {
                calls: calls.clone(),
                seen: seen.clone(),
                now: delayed.then(|| (now.clone(), 1_301_000)),
            }) as Box<dyn Decider>
        };
        assert!(matches!(
            Pipeline::new(&s, &c, &a, &e, &clock).with_shadows(vec![shadow()], false),
            Err(Error::ConsentRequired)
        ));
        // A local model needs no cloud consent; any cloud model still does.
        assert!(
            Pipeline::new(&s, &c, &a, &e, &clock)
                .with_shadow_sets(vec![], vec![shadow()], false)
                .is_ok()
        );
        assert!(matches!(
            Pipeline::new(&s, &c, &a, &e, &clock).with_shadow_sets(
                vec![shadow()],
                vec![shadow()],
                false
            ),
            Err(Error::ConsentRequired)
        ));
        let p = Pipeline::new(&s, &c, &a, &e, &clock)
            .with_shadows(vec![shadow()], true)
            .unwrap();
        assert_eq!(
            p.diagnose(id).await.unwrap(),
            if delayed {
                Outcome::ChangedDuringCollection
            } else {
                Outcome::Persisted
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let text = serde_json::to_string(&*seen.lock().unwrap()).unwrap();
        assert!(!text.contains("private"));
        assert!(!text.contains("/media"));
        if !delayed {
            let d = s.diagnosis(id).await.unwrap().unwrap();
            assert_eq!(d.decider_kind, DeciderKind::Rules);
            assert_eq!(d.decision.decision.action, RecommendedAction::Replace);
            assert_eq!(
                s.diagnosis_comparison(id).await.unwrap().unwrap()["entries"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
}

#[tokio::test]
async fn audio_language_facts_require_an_exact_successful_complete_stream_list() {
    for (language, tag, probe, expected) in [
        (
            "english",
            Some("eng"),
            Probe::Passed,
            TrackPresence::Present,
        ),
        ("spanish", Some("es"), Probe::Passed, TrackPresence::Present),
        ("spanish", Some("eng"), Probe::Passed, TrackPresence::Absent),
        ("english", None, Probe::Passed, TrackPresence::Unknown),
        (
            "english",
            Some("spa"),
            Probe::Failed,
            TrackPresence::Unknown,
        ),
        (
            "other_language",
            Some("spa"),
            Probe::Passed,
            TrackPresence::Unknown,
        ),
    ] {
        let (_dir, s) = setup().await;
        let mut v = response(IssueKind::AudioLanguage);
        v["wizard"] = json!({"answer":language});
        let id = persist(&s, v).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let mut e = evidence();
        if let Observation::Available(m) = &mut e.agent.media {
            let h = m.header.as_mut().unwrap();
            h.outcome = probe;
            h.streams.push(StreamFacts::Audio {
                codec: "aac".into(),
                channels: 2,
                language: tag.map(str::to_owned),
            });
        }
        Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
            .diagnose(id)
            .await
            .unwrap();
        assert_eq!(
            s.diagnosis(id)
                .await
                .unwrap()
                .unwrap()
                .evidence_summary
                .audio
                .requested_track,
            expected
        );
    }
}

proptest::proptest! {
    #[test]
    fn access_never_outlives_its_snapshot(observed in 0u64..10000, ttl in 1u64..10000, now in 0u64..20000) {
        let g=AccessGrant {observed_at:observed,expires_at:observed+ttl,..grant()};
        proptest::prop_assert_eq!(g.permits("17","library-a",now),observed<=now && now<observed+ttl);
    }
}

#[tokio::test]
async fn final_evidence_age_uses_milliseconds_and_rejects_clock_regression() {
    for (at, expected) in [
        (1_300_000, Outcome::Persisted),
        (1_300_001, Outcome::ChangedDuringCollection),
        (999_999, Outcome::ChangedDuringCollection),
    ] {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(IssueKind::Playback)).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock_now = now.clone();
        let clock = move || clock_now.load(Ordering::SeqCst);
        let shadow = Shadow {
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(vec![])),
            now: Some((now, at)),
        };
        let p = Pipeline::new(&s, &c, &a, &e, &clock)
            .with_shadows(vec![Box::new(shadow)], true)
            .unwrap();
        assert_eq!(p.diagnose(id).await.unwrap(), expected, "final millis {at}");
    }
}

#[tokio::test]
async fn audio_unknown_tags_and_partial_headers_never_prove_missing_language() {
    for (tag, fault, expected) in [
        ("und", false, TrackPresence::Unknown),
        ("fre", false, TrackPresence::Absent),
        ("", false, TrackPresence::Unknown),
        ("unknown", false, TrackPresence::Unknown),
        ("ENG", false, TrackPresence::Present),
        ("spa", true, TrackPresence::Unknown),
    ] {
        let (_dir, s) = setup().await;
        let mut v = response(IssueKind::AudioLanguage);
        v["wizard"] = json!({"answer":"english"});
        let id = persist(&s, v).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let mut e = evidence();
        if let Observation::Available(m) = &mut e.agent.media {
            let h = m.header.as_mut().unwrap();
            h.streams.push(StreamFacts::Audio {
                codec: "aac".into(),
                channels: 2,
                language: Some(tag.into()),
            });
            if fault {
                h.faults.push(pitcrew_domain::media::MediaFault::NoDuration);
            }
        }
        Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
            .diagnose(id)
            .await
            .unwrap();
        assert_eq!(
            s.diagnosis(id)
                .await
                .unwrap()
                .unwrap()
                .evidence_summary
                .audio
                .requested_track,
            expected,
            "tag {tag}, fault {fault}"
        );
    }
}
struct ConcurrentEvidence {
    barrier: tokio::sync::Barrier,
    calls: AtomicUsize,
}
impl EvidenceSource for ConcurrentEvidence {
    fn collect<'a>(
        &'a self,
        _: &'a ResolvedTarget,
        _: &'a str,
        _: &'a Clock,
    ) -> EvidenceFuture<'a> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait().await;
            let mut m = media();
            m.observed_at_unix_ms = 999_999 + n as u64;
            CollectedEvidence {
                agent: AgentEvidence {
                    media: Observation::Available(m),
                    ..AgentEvidence::default()
                },
                ..CollectedEvidence::default()
            }
        })
    }
}
#[tokio::test]
async fn concurrent_workers_keep_the_first_diagnosis_without_spurious_failure() {
    let (_dir, s) = setup().await;
    let id = persist(&s, response(IssueKind::Playback)).await;
    let c = Catalog(Mutex::new(Ok(target())));
    let a = Access(Mutex::new(Some(grant())));
    let e = ConcurrentEvidence {
        barrier: tokio::sync::Barrier::new(2),
        calls: AtomicUsize::new(0),
    };
    let p = Pipeline::new(&s, &c, &a, &e, &|| 1_000_000);
    let (one, two) = tokio::join!(p.diagnose(id), p.diagnose(id));
    let outcomes = [one.unwrap(), two.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == Outcome::Persisted)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == Outcome::AlreadyDecided)
            .count(),
        1
    );
    assert!(s.diagnosis_comparison(id).await.unwrap().is_some());
}

#[tokio::test]
async fn finite_report_facts_preserve_symptoms_movie_resolution_and_suspicion() {
    for (kind, complaint) in [
        (IssueKind::Playback, Complaint::Playback),
        (IssueKind::Buffering, Complaint::Playback),
        (IssueKind::Quality, Complaint::VideoBroken),
        (IssueKind::NoAudio, Complaint::NoAudio),
        (IssueKind::WrongEpisode, Complaint::WrongEpisode),
        (IssueKind::AudioLanguage, Complaint::WrongAudioLanguage),
        (IssueKind::Subtitles, Complaint::Subtitles),
    ] {
        let (_dir, s) = setup().await;
        let id = persist(&s, response(kind)).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
            .with_shadows(vec![], false)
            .unwrap()
            .diagnose(id)
            .await
            .unwrap();
        let facts = s.diagnosis(id).await.unwrap().unwrap().evidence_summary;
        assert_eq!(facts.complaint, complaint);
        assert_eq!(facts.scope, Scope::Episode);
        assert!(!facts.playback.is_4k);
        assert!(!facts.injection_suspected);
    }
    let (_dir, s) = setup().await;
    let mut v = response(IssueKind::Playback);
    v["suspicious_input"] = json!(true);
    let id = persist(&s, v).await;
    let mut t = target();
    t.item.season = None;
    t.item.episode = None;
    t.item.resolution = Resolution::Uhd;
    let c = Catalog(Mutex::new(Ok(t)));
    let a = Access(Mutex::new(Some(grant())));
    let e = evidence();
    Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
        .diagnose(id)
        .await
        .unwrap();
    let d = s.diagnosis(id).await.unwrap().unwrap();
    assert_eq!(d.evidence_summary.scope, Scope::Movie);
    assert!(d.evidence_summary.playback.is_4k);
    assert!(d.evidence_summary.injection_suspected);
    assert_eq!(d.decision.decision.reason, Reason::UntrustedInstructions);
}

#[tokio::test]
async fn malformed_wizard_and_uncommittable_future_report_return_explicit_errors() {
    for wizard in [json!(null), json!({"answer":17})] {
        let (_dir, s) = setup().await;
        let mut v = response(IssueKind::Playback);
        v["wizard"] = wizard;
        let id = persist(&s, v).await;
        let c = Catalog(Mutex::new(Ok(target())));
        let a = Access(Mutex::new(Some(grant())));
        let e = evidence();
        assert!(matches!(
            Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
                .diagnose(id)
                .await,
            Err(Error::InvalidReport)
        ));
        assert_eq!(e.calls.load(Ordering::SeqCst), 0);
    }
    let (_dir, s) = setup().await;
    s.persist_intake(
        "web",
        "future",
        "17",
        "conversation",
        &response(IssueKind::Playback),
        1001,
    )
    .await
    .unwrap();
    let id = s.reports("17").await.unwrap()[0].id;
    let c = Catalog(Mutex::new(Ok(target())));
    let a = Access(Mutex::new(Some(grant())));
    let e = evidence();
    assert!(matches!(
        Pipeline::new(&s, &c, &a, &e, &|| 1_000_000)
            .diagnose(id)
            .await,
        Err(Error::Storage(pitcrew_intake_store::Error::Invalid))
    ));
    assert!(s.diagnosis(id).await.unwrap().is_none());
}

#[tokio::test]
async fn explicit_wizard_intent_survives_generic_playback_normalization() {
    for (issue, answer, complaint, action, reason) in [
        (
            "other",
            "describe",
            Complaint::Other,
            RecommendedAction::NotifyAdmin,
            Reason::InsufficientEvidence,
        ),
        (
            "video_broken",
            "confirmed",
            Complaint::VideoBroken,
            RecommendedAction::CollectEvidence,
            Reason::UnverifiedViewerReport,
        ),
    ] {
        let (_dir, store) = setup().await;
        // The generic intake symptom cannot erase the explicit wizard intent.
        // Old or independently hosted intake adapters may retain this symptom
        // for a review report instead of clearing its diagnostic-kind list.
        let mut value = response(IssueKind::Playback);
        value["wizard"] = json!({"issue":issue,"answer":answer});
        let id = persist(&store, value).await;
        let catalog = Catalog(Mutex::new(Ok(target())));
        let access = Access(Mutex::new(Some(grant())));
        let mut source = evidence();
        if issue == "video_broken"
            && let Observation::Available(media) = &mut source.agent.media
        {
            // A successful header alone cannot disprove a viewer's broken
            // picture or authorize replacing the file. More evidence is needed.
            media.decode = None;
        }
        assert_eq!(
            Pipeline::new(&store, &catalog, &access, &source, &|| 1_000_000)
                .diagnose(id)
                .await
                .unwrap(),
            Outcome::Persisted
        );
        let diagnosis = store.diagnosis(id).await.unwrap().unwrap();
        assert_eq!(diagnosis.evidence_summary.complaint, complaint, "{issue}");
        assert_eq!(diagnosis.decision.decision.action, action, "{issue}");
        assert_eq!(diagnosis.decision.decision.reason, reason, "{issue}");
    }
}
