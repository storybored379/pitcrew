use super::*;
use pitcrew_upstream::{Method, Request, Response, ResponseFuture, Transport};
use std::{collections::VecDeque, sync::Mutex};

struct Script(Mutex<VecDeque<(&'static str, serde_json::Value)>>);
impl Transport for Script {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        let (path, body) = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url.path(), path);
        Box::pin(async move {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}
fn page(total: usize, records: Vec<serde_json::Value>) -> serde_json::Value {
    json!({"page":1,"pageSize":100,"totalRecords":total,"records":records})
}
fn history(id: usize, event: &str) -> serde_json::Value {
    json!({"id":id,"eventType":event,"date":"2026-01-01T00:00:00Z","downloadId":format!("download-{id}"),"episodeId":id})
}
async fn run(
    script: Vec<(&'static str, serde_json::Value)>,
) -> (tempfile::TempDir, App, Result<(), String>) {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir).await;
    let outcome = poll_with(&app, script).await;
    (dir, app, outcome)
}
async fn app(dir: &tempfile::TempDir) -> App {
    App::new(
        Store::open(dir.path().join("db")).await.unwrap(),
        vec![],
        None,
        BudgetLimits {
            global_hour: 10,
            global_day: 20,
            viewer_hour: 10,
            viewer_day: 20,
        },
        2,
        BTreeMap::new(),
    )
    .unwrap()
}
async fn poll_with(
    app: &App,
    script: Vec<(&'static str, serde_json::Value)>,
) -> Result<(), String> {
    let client = Client::new(
        Endpoint::new(
            "http://fake.invalid",
            Auth::arr("synthetic-key").unwrap(),
            Limits::default(),
        )
        .unwrap(),
        Script(Mutex::new(script.into())),
        Kind::Sonarr,
    );
    poll(app, "series", Kind::Sonarr, &client, 1234).await
}
#[tokio::test]
async fn all_history_events_keep_exact_kind_and_timestamp() {
    let events = [
        "grabbed",
        "downloadFolderImported",
        "downloadFailed",
        "episodeFileDeleted",
        "movieFileDeleted",
        "unrecognized",
    ];
    let (_dir, app, result) = run(vec![
        (
            "/api/v3/history",
            page(
                6,
                events
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| history(i + 1, e))
                    .collect(),
            ),
        ),
        ("/api/v3/queue", page(0, vec![])),
    ])
    .await;
    result.unwrap();
    let rows = app.store.observations().await.unwrap();
    assert_eq!(rows.len(), 5);
    for (row, kind) in rows.iter().zip([
        "Grab",
        "Download",
        "DownloadFailed",
        "EpisodeFileDelete",
        "MovieFileDelete",
    ]) {
        assert_eq!(row.at, 1234);
        assert_eq!(row.payload["kind"], kind);
    }
}
#[tokio::test]
async fn history_reads_until_exact_page_boundary_and_never_exceeds_ten_pages() {
    for total in [0usize, 100, 101, 200, 1000, 1001] {
        let pages = total.div_ceil(100).clamp(1, 10);
        let mut script = (0..pages)
            .map(|_| ("/api/v3/history", page(total, vec![])))
            .collect::<Vec<_>>();
        script.push(("/api/v3/queue", page(0, vec![])));
        let (_dir, _app, outcome) = run(script).await;
        assert_eq!(outcome.is_ok(), total <= 1000, "total {total}");
    }
    for count in [100, 101] {
        let records = (1..=count).map(|i| history(i, "grabbed")).collect();
        let mut script = vec![("/api/v3/history", page(100, records))];
        if count == 100 {
            script.push(("/api/v3/queue", page(0, vec![])));
        }
        let (_dir, app, outcome) = run(script).await;
        assert_eq!(outcome.is_ok(), count == 100);
        assert_eq!(
            app.store.observations().await.unwrap().len(),
            if count == 100 { 100 } else { 0 }
        );
    }
}
#[tokio::test]
async fn queue_page_limits_and_manual_import_request_budget_are_independent() {
    for (total, count, ok) in [(100, 100, true), (101, 0, false), (0, 101, false)] {
        let rows = (1..=count)
            .map(
                |id| json!({"id":id,"trackedDownloadState":"downloading","downloadId":"unblocked"}),
            )
            .collect();
        let (_dir, app, outcome) = run(vec![
            ("/api/v3/history", page(0, vec![])),
            ("/api/v3/queue", page(total, rows)),
        ])
        .await;
        assert_eq!(outcome.is_ok(), ok);
        assert_eq!(
            app.store.observations().await.unwrap().len(),
            count.min(100)
        );
    }
    for count in [0, 1, 10, 11] {
        let rows=(1..=count).map(|id|json!({"id":id,"trackedDownloadState":"importBlocked","downloadId":format!("download-{id}")})).collect();
        let mut script = vec![
            ("/api/v3/history", page(0, vec![])),
            ("/api/v3/queue", page(count, rows)),
        ];
        script.extend((0..count.min(10)).map(|_| ("/api/v3/manualimport", json!([]))));
        let (_dir, app, outcome) = run(script).await;
        assert_eq!(outcome.is_ok(), count <= 10);
        assert_eq!(app.store.observations().await.unwrap().len(), count);
    }
    let (_dir, _app, outcome) = run(vec![
        ("/api/v3/history", page(0, vec![])),
        (
            "/api/v3/queue",
            page(
                1,
                vec![json!({"id":1,"trackedDownloadState":"importBlocked"})],
            ),
        ),
    ])
    .await;
    outcome.unwrap();
}
#[tokio::test]
async fn history_stops_complete_at_rows_an_earlier_poll_stored() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(&dir).await;
    // Only fresh rows: the second page is still read.
    poll_with(
        &app,
        vec![
            (
                "/api/v3/history",
                page(200, vec![history(2, "grabbed"), history(1, "grabbed")]),
            ),
            ("/api/v3/history", page(200, vec![])),
            ("/api/v3/queue", page(0, vec![])),
        ],
    )
    .await
    .unwrap();
    // A long history overlapping the last poll is complete without paging on.
    poll_with(
        &app,
        vec![
            (
                "/api/v3/history",
                page(
                    555_738,
                    vec![
                        history(3, "grabbed"),
                        history(2, "grabbed"),
                        history(1, "grabbed"),
                    ],
                ),
            ),
            ("/api/v3/queue", page(0, vec![])),
        ],
    )
    .await
    .unwrap();
    assert_eq!(app.store.observations().await.unwrap().len(), 3);
}
