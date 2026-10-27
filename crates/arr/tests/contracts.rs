use pitcrew_arr::*;
use pitcrew_upstream::{
    Auth, Endpoint, Error, Limits, LivePermit, Method, Mode, Request, Response, ResponseFuture,
    Transport,
};
use serde_json::{Value, json};
use std::sync::Mutex;
struct Fake {
    requests: Mutex<Vec<Request>>,
    body: Value,
    deny_writes: bool,
}
impl Transport for &Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert!(
            !self.deny_writes || request.method == Method::Get,
            "shadow client attempted a write"
        );
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&self.body).unwrap(),
            })
        })
    }
}
fn fake(body: Value, deny_writes: bool) -> Fake {
    Fake {
        requests: Mutex::new(vec![]),
        body,
        deny_writes,
    }
}
fn client(fake: &Fake, kind: Kind) -> Client<&Fake> {
    Client::new(
        Endpoint::new(
            "https://arr.invalid/arr",
            Auth::arr("synthetic-key").unwrap(),
            Limits::default(),
        )
        .unwrap(),
        fake,
        kind,
    )
}
fn page(records: Value) -> Value {
    json!({"page": 1, "pageSize": 100, "totalRecords": records.as_array().unwrap().len(), "records": records})
}
#[tokio::test]
async fn queue_history_blocklist_are_typed_read_only_pages() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let q = fake(
            page(
                json!([{"id": 8, "downloadId":"synthetic-download", "status":"completed", "trackedDownloadState":"importBlocked", "statusMessages":[{"title":"import", "messages":["synthetic warning"]}]}]),
            ),
            true,
        );
        let result = client(&q, kind).queue(Page::default()).await.unwrap();
        assert_eq!(result.records[0].id, 8);
        assert_eq!(
            result.records[0].download_id.as_deref(),
            Some("synthetic-download")
        );
        assert!(result.records[0].episode_id.is_none());
        assert_eq!(result.total_records, 1);
        assert_eq!(
            q.requests.lock().unwrap()[0].url.path(),
            "/arr/api/v3/queue"
        );
        let h = fake(
            page(
                json!([{"id": 9, "eventType":"downloadFolderImported", "date":"2025-01-01T00:00:00Z", "data":{"droppedPath":"/media/synthetic.mkv"}}]),
            ),
            true,
        );
        let history = client(&h, kind).history(Page::default()).await.unwrap();
        assert_eq!(history.records[0].event_type, "downloadFolderImported");
        {
            let requests = h.requests.lock().unwrap();
            let query: Vec<_> = requests[0].url.query_pairs().collect();
            assert!(query.contains(&("sortDirection".into(), "descending".into())));
        }
        let b = fake(
            page(json!([{"id": 10, "sourceTitle":"Synthetic.Release", "message":"unavailable"}])),
            true,
        );
        assert_eq!(
            client(&b, kind)
                .blocklist(Page::default())
                .await
                .unwrap()
                .records[0]
                .id,
            10
        );
        assert_eq!(
            b.requests.lock().unwrap()[0].url.path(),
            "/arr/api/v3/blocklist"
        );
    }
}
#[tokio::test]
async fn file_search_and_candidates_preserve_identity_and_unknowns() {
    for (kind, file_path, key) in [
        (Kind::Sonarr, "episodefile", "episodeId"),
        (Kind::Radarr, "moviefile", "movieId"),
    ] {
        let f = fake(
            json!({"id": 12, "path":"/media/synthetic.mkv", "size":12345}),
            true,
        );
        assert_eq!(client(&f, kind).file(12).await.unwrap().size, Some(12345));
        assert_eq!(
            f.requests.lock().unwrap()[0].url.path(),
            format!("/arr/api/v3/{file_path}/12")
        );
        let r = fake(
            json!([{"guid":"synthetic-guid", "indexerId": 3, "rejections":[]}]),
            true,
        );
        let result = client(&r, kind).releases(19).await.unwrap();
        assert!(result[0].approved.is_none());
        assert_eq!(result[0].guid, "synthetic-guid");
        assert_eq!(
            r.requests.lock().unwrap()[0]
                .url
                .query_pairs()
                .collect::<Vec<_>>(),
            vec![(key.into(), "19".into())]
        );
        let m = fake(
            json!([{"id": 7, "path":"/download/synthetic.mkv", "downloadId":"synthetic&download", "rejections":[]}]),
            true,
        );
        assert_eq!(
            client(&m, kind)
                .manual_import_candidates("synthetic&download")
                .await
                .unwrap()[0]
                .id,
            7
        );
        assert_eq!(
            m.requests.lock().unwrap()[0]
                .url
                .query_pairs()
                .collect::<Vec<_>>(),
            vec![
                ("downloadId".into(), "synthetic&download".into()),
                ("filterExistingFiles".into(), "false".into())
            ]
        );
    }
}
#[tokio::test]
async fn writes_require_permit_and_explicit_safe_wire_payloads() {
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    for (kind, key, file_path) in [
        (Kind::Sonarr, "episodeId", "episodefile"),
        (Kind::Radarr, "movieId", "moviefile"),
    ] {
        let f = fake(json!({"id": 44}), false);
        let c = client(&f, kind);
        c.grab(&permit, 19, "synthetic-guid", 3).await.unwrap();
        c.delete_file(&permit, 12).await.unwrap();
        c.blocklist_queue(&permit, 8).await.unwrap();
        let file = ImportFile {
            path: "/download/synthetic.mkv".into(),
            download_id: "synthetic-download".into(),
            series_id: (kind == Kind::Sonarr).then_some(2),
            movie_id: (kind == Kind::Radarr).then_some(19),
            episode_ids: if kind == Kind::Sonarr {
                vec![19]
            } else {
                vec![]
            },
            quality: json!({"quality":{"id": 4}}),
            languages: vec![],
            release_group: "Synthetic".into(),
        };
        c.manual_import(&permit, &[file]).await.unwrap();
        let requests = f.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method, Method::Post);
        assert_eq!(requests[0].body.as_ref().unwrap()[key], 19);
        assert_eq!(
            requests[1].url.path(),
            format!("/arr/api/v3/{file_path}/12")
        );
        assert_eq!(requests[1].method, Method::Delete);
        assert_eq!(
            requests[2].url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("removeFromClient".into(), "false".into()),
                ("blocklist".into(), "true".into()),
                ("skipRedownload".into(), "true".into()),
                ("changeCategory".into(), "false".into())
            ]
        );
        assert_eq!(requests[3].url.path(), "/arr/api/v3/command");
        let body = requests[3].body.as_ref().unwrap();
        assert_eq!(body["name"], "ManualImport");
        assert_eq!(body["importMode"], "auto");
        assert_eq!(body["files"][0]["downloadId"], "synthetic-download");
        assert!(
            requests
                .iter()
                .all(|r| !r.url.path().contains("/history/failed"))
        );
    }
}
#[tokio::test]
async fn invalid_requests_never_reach_transport() {
    let f = fake(json!({}), true);
    let c = client(&f, Kind::Sonarr);
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    assert!(
        c.queue(Page {
            number: 0,
            size: 100
        })
        .await
        .is_err()
    );
    assert!(
        c.history(Page {
            number: 1,
            size: 1001
        })
        .await
        .is_err()
    );
    assert!(c.file(0).await.is_err());
    assert!(c.releases(0).await.is_err());
    assert!(c.manual_import_candidates("").await.is_err());
    assert_eq!(
        c.grab(&permit, 1, "synthetic", 0).await,
        Err(Error::InvalidInput)
    );
    assert_eq!(
        c.grab(&permit, 1, "bad\nvalue", 1).await,
        Err(Error::InvalidInput)
    );
    assert_eq!(
        c.manual_import(&permit, &[]).await,
        Err(Error::InvalidInput)
    );
    assert_eq!(c.delete_file(&permit, 0).await, Err(Error::InvalidInput));
    assert_eq!(
        c.blocklist_queue(&permit, 0).await,
        Err(Error::InvalidInput)
    );
    assert!(f.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn incomplete_or_changed_required_schema_never_becomes_empty_success() {
    let f = fake(json!({"records":[]}), true);
    assert!(matches!(
        client(&f, Kind::Sonarr).queue(Page::default()).await,
        Err(Error::InvalidResponse)
    ));
    let f = fake(
        page(json!([{"id": 5, "eventType": 999, "date":"2025-01-01"}])),
        true,
    );
    assert!(matches!(
        client(&f, Kind::Sonarr).history(Page::default()).await,
        Err(Error::InvalidResponse)
    ));
}

#[test]
fn queue_adapter_preserves_unknown_evidence_and_never_guesses_legacy_actor() {
    use pitcrew_arbiter::shadow::{Action, LegacyAction, MediaKind, Reason, compare};
    let queue: QueueRecord =
        serde_json::from_value(json!({"id":8,"trackedDownloadState":"importBlocked"})).unwrap();
    let candidate: ImportCandidate =
        serde_json::from_value(json!({"id":7,"path":"/download/synthetic.mkv","rejections":[]}))
            .unwrap();
    let correlation = || shadow::Correlation {
        job: 4,
        library: 2,
        download: 8,
        observed_at: 100,
    };
    let o = shadow::observe_queue(
        Kind::Sonarr,
        correlation(),
        &queue,
        Some(std::slice::from_ref(&candidate)),
    );
    assert_eq!(o.import_blocked, Some(true));
    assert_eq!(o.facts.no_rejections, Some(true));
    assert_eq!(o.kind, MediaKind::Episode);
    assert_eq!(o.legacy_action, LegacyAction::Unknown);
    assert_eq!(compare(&o).decision.action, Action::Hold);
    assert_eq!(compare(&o).decision.reason, Reason::AccessUnknown);
    let mut rejected = candidate.clone();
    rejected.rejections = Some(vec![json!({"reason":"synthetic rejection"})]);
    assert_eq!(
        shadow::observe_queue(Kind::Radarr, correlation(), &queue, Some(&[rejected]))
            .facts
            .no_rejections,
        Some(false)
    );
    for candidates in [
        None,
        Some(vec![]),
        Some(vec![ImportCandidate {
            rejections: None,
            ..candidate
        }]),
    ] {
        assert_eq!(
            shadow::observe_queue(Kind::Radarr, correlation(), &queue, candidates.as_deref())
                .facts
                .no_rejections,
            None
        );
    }
}

#[tokio::test]
async fn nullable_diagnostic_fields_from_official_schema_remain_unknown() {
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let q = fake(
            page(
                json!([{"id":8,"statusMessages":null},{"id":10,"statusMessages":[{"title":null,"messages":null}]}]),
            ),
            true,
        );
        let rows = client(&q, kind)
            .queue(Page::default())
            .await
            .unwrap()
            .records;
        assert!(rows[0].status_messages.is_none());
        assert!(
            rows[1].status_messages.as_ref().unwrap()[0]
                .messages
                .is_none()
        );
        let h = fake(
            page(json!([{"id":9,"eventType":"grabbed","date":"2025-01-01T00:00:00Z","data":null}])),
            true,
        );
        assert!(
            client(&h, kind)
                .history(Page::default())
                .await
                .unwrap()
                .records[0]
                .data
                .is_none()
        );
    }
}
