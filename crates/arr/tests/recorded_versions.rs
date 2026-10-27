//! Synthetic field-shape contracts for the exact owner-selected versions.
//! All identifying values are invented; these do not certify upstream writes.
use pitcrew_arr::{Client, Kind, Page};
use pitcrew_upstream::{
    Auth, Endpoint, Limits, Method, Request, Response, ResponseFuture, Transport,
};
use serde_json::Value;
use std::sync::Mutex;
struct Recording {
    fixture: Value,
    paths: Mutex<Vec<String>>,
}
impl Transport for &Recording {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get);
        let path = request.url.path().to_owned();
        self.paths.lock().unwrap().push(path.clone());
        let name = match path.as_str() {
            "/api/v3/system/status" => "status",
            "/api/v3/episode/101" | "/api/v3/movie/101" => "item",
            "/api/v3/episodefile/301" | "/api/v3/moviefile/301" => "file",
            "/api/v3/queue" => "queue",
            "/api/v3/history" => "history",
            "/api/v3/blocklist" => "blocklist",
            _ => panic!("unexpected route"),
        };
        let body = serde_json::to_vec(&self.fixture[name]).unwrap();
        Box::pin(async move { Ok(Response { status: 200, body }) })
    }
}
#[tokio::test]
async fn exact_version_read_contracts_retain_file_revision_and_nullable_failure_evidence() {
    for (kind, version, source) in [
        (
            Kind::Sonarr,
            "4.0.19.2979",
            include_str!("fixtures/sonarr-4.0.19.2979.json"),
        ),
        (
            Kind::Radarr,
            "6.3.0.10514",
            include_str!("fixtures/radarr-6.3.0.10514.json"),
        ),
    ] {
        let recorded = Recording {
            fixture: serde_json::from_str(source).unwrap(),
            paths: Mutex::default(),
        };
        let client = Client::new(
            Endpoint::new(
                "https://arr.invalid/",
                Auth::arr("synthetic-key").unwrap(),
                Limits::default(),
            )
            .unwrap(),
            &recorded,
            kind,
        );
        assert_eq!(client.version().await.unwrap(), version);
        assert_eq!(client.installed_file(101).await.unwrap(), 301);
        let file = client.file(301).await.unwrap();
        assert_eq!(file.id, 301);
        assert_eq!(file.date_added.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert!(
            file.quality.unwrap()["revision"]["version"]
                .as_u64()
                .is_some()
        );
        let queue = client.queue(Page::default()).await.unwrap();
        let history = client.history(Page::default()).await.unwrap();
        assert_eq!(
            client.blocklist(Page::default()).await.unwrap().records[0].id,
            501
        );
        if kind == Kind::Sonarr {
            assert!(queue.records.is_empty());
            assert_eq!(history.records[0].data.as_ref().unwrap()["fileId"], "301");
            assert_eq!(history.records[1].event_type, "downloadFailed");
            assert!(history.records[1].data.as_ref().unwrap()["size"].is_null());
            assert!(history.records[1].data.as_ref().unwrap()["downloadClientName"].is_null());
        } else {
            assert_eq!(queue.records[0].size, Some(2147483648.0));
            assert_eq!(queue.records[0].sizeleft, Some(0.0));
            assert_eq!(queue.records[0].movie_id, Some(101));
            assert_eq!(
                queue.records[0].tracked_download_state.as_deref(),
                Some("importPending")
            );
            assert_eq!(
                history.records[0].data.as_ref().unwrap()["indexerId"],
                "701"
            );
        }
        assert_eq!(recorded.paths.lock().unwrap().len(), 6);
    }
}
