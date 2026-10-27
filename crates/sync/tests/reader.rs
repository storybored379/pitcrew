use pitcrew_sync::{Error, reader::*};
use pitcrew_upstream::{
    Auth, Endpoint, Limits, Method, Request, Response, ResponseFuture, Transport,
};
use serde_json::{Value, json};
use std::sync::Mutex;

struct Fixture {
    expected_path: &'static str,
    query: Vec<(&'static str, &'static str)>,
    response: Mutex<Option<Result<Value, pitcrew_upstream::Error>>>,
}
impl Transport for Fixture {
    fn send(&self, r: Request) -> ResponseFuture<'_> {
        assert_eq!(r.method, Method::Get);
        assert!(r.body.is_none());
        assert_eq!(r.url.path(), self.expected_path);
        let query = r.url.query_pairs().collect::<Vec<_>>();
        assert_eq!(query.len(), self.query.len());
        for (key, value) in &self.query {
            assert!(query.iter().any(|(k, v)| k == key && v == value));
        }
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "X-Plex-Token" && v == "synthetic")
        );
        let response = self.response.lock().unwrap().take().unwrap();
        Box::pin(async {
            response.map(|v| Response {
                status: 200,
                body: serde_json::to_vec(&v).unwrap(),
            })
        })
    }
}
fn reader(
    path: &'static str,
    query: Vec<(&'static str, &'static str)>,
    value: Value,
    max_rows: usize,
) -> Reader<Fixture> {
    Reader::new(
        Endpoint::new(
            "http://fixture.invalid",
            Auth::plex("synthetic").unwrap(),
            Limits::default(),
        )
        .unwrap(),
        Fixture {
            expected_path: path,
            query,
            response: Mutex::new(Some(Ok(value))),
        },
        max_rows,
    )
    .unwrap()
}
fn pq(offset: &'static str, kind: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("type", kind),
        ("X-Plex-Container-Start", offset),
        ("X-Plex-Container-Size", "2"),
        ("sort", "id:asc"),
    ]
}
fn raw_item() -> Value {
    json!({"ratingKey":"11","type":"movie","title":"Synthetic Movie"})
}
fn raw_page(size: u64, offset: u64, total: u64, items: Vec<Value>) -> Value {
    json!({"MediaContainer":{"size":size,"offset":offset,"totalSize":total,"Metadata":items}})
}

#[tokio::test]
async fn bounded_incremental_pagination_validates_each_response() {
    let r = reader(
        "/library/sections/2/all",
        pq("0", "4"),
        raw_page(2, 0, 3, vec![raw_item(); 2]),
        2,
    );
    let page = r.plex_page(2, true, Cursor::default(), 2).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(
        page.next,
        Some(Cursor {
            offset: 2,
            updated_since: None
        })
    );
    let mut query = pq("2", "1");
    query.push(("updatedAt>=", "50"));
    let r = reader(
        "/library/sections/2/all",
        query,
        raw_page(1, 2, 3, vec![raw_item()]),
        2,
    );
    let page = r
        .plex_page(
            2,
            false,
            Cursor {
                offset: 2,
                updated_since: Some(50),
            },
            2,
        )
        .await
        .unwrap();
    assert_eq!(page.next, None);
    assert_eq!(page.items[0].title, "Synthetic Movie");
    let r = reader(
        "/library/sections/2/all",
        pq("0", "1"),
        raw_page(0, 0, 0, vec![]),
        2,
    );
    assert!(
        r.plex_page(2, false, Cursor::default(), 2)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for value in [
        raw_page(1, 1, 2, vec![raw_item()]),
        raw_page(2, 0, 2, vec![raw_item()]),
        raw_page(3, 0, 3, vec![raw_item(); 3]),
        raw_page(1, 0, 0, vec![raw_item()]),
        raw_page(0, 0, 1, vec![]),
    ] {
        let r = reader("/library/sections/2/all", pq("0", "1"), value, 3);
        assert_eq!(
            r.plex_page(2, false, Cursor::default(), 2).await,
            Err(Error::Malformed)
        );
    }
    let r = reader(
        "/library/sections/2/all",
        pq("0", "1"),
        raw_page(2, 0, 2, vec![raw_item(); 2]),
        1,
    );
    assert_eq!(
        r.plex_page(2, false, Cursor::default(), 2).await,
        Err(Error::Bound)
    );
    let r = reader(
        "/library/sections/2/all",
        pq("18446744073709551615", "1"),
        raw_page(1, u64::MAX, u64::MAX, vec![raw_item()]),
        2,
    );
    assert_eq!(
        r.plex_page(
            2,
            false,
            Cursor {
                offset: u64::MAX,
                updated_since: None
            },
            2
        )
        .await,
        Err(Error::Bound)
    );
    for (section, size) in [(0, 2), (2, 0), (2, 1001)] {
        let r = reader("/never", vec![], json!({}), 1);
        assert_eq!(
            r.plex_page(section, false, Cursor::default(), size).await,
            Err(Error::Malformed)
        );
    }
}

#[tokio::test]
async fn all_read_paths_and_nonpaginated_bounds() {
    let r = reader(
        "/library/sections",
        vec![],
        json!({"MediaContainer":{"size":1,"Directory":[{"key":"2","title":"Shows","type":"show"}]}}),
        1,
    );
    assert_eq!(
        r.sections().await.unwrap(),
        vec![Section {
            key: "2".into(),
            title: "Shows".into(),
            kind: "show".into()
        }]
    );
    let r = reader(
        "/library/sections",
        vec![],
        json!({"MediaContainer":{"size":0}}),
        1,
    );
    assert!(r.sections().await.unwrap().is_empty());
    let r = reader(
        "/library/sections",
        vec![],
        json!({"MediaContainer":{"size":1}}),
        1,
    );
    assert_eq!(r.sections().await, Err(Error::Malformed));
    let r = reader(
        "/api/v3/series",
        vec![],
        json!([{"id":8,"title":"Synthetic Show"}]),
        1,
    );
    assert_eq!(r.series().await.unwrap()[0].id, 8);
    let r = reader(
        "/api/v3/episode",
        vec![("seriesId", "8")],
        json!([{"id":12,"seriesId":8,"seasonNumber":1,"episodeNumber":2,"episodeFileId":14}]),
        1,
    );
    assert_eq!(r.episodes(8).await.unwrap()[0].episode_file_id, 14);
    let r = reader(
        "/api/v3/episodefile",
        vec![("seriesId", "8")],
        json!([{"id":14,"path":"/media/item.mkv","seriesId":8}]),
        1,
    );
    assert_eq!(r.episode_files(8).await.unwrap()[0].id, 14);
    let r = reader(
        "/api/v3/movie",
        vec![],
        json!([{"id":3,"title":"Synthetic Film","movieFile":{"id":4,"path":"/media/film.mkv","movieId":3}}]),
        1,
    );
    assert_eq!(
        r.movies().await.unwrap()[0].movie_file.as_ref().unwrap().id,
        4
    );
    let r = reader(
        "/api/v3/moviefile",
        vec![("movieId", "3")],
        json!([{"id":4,"path":"/media/film.mkv","movieId":3}]),
        1,
    );
    assert_eq!(r.movie_files(3).await.unwrap()[0].movie_id, Some(3));
    let r = reader(
        "/api/v3/series",
        vec![],
        json!([{"id":8,"title":"A"},{"id":9,"title":"B"}]),
        1,
    );
    assert_eq!(r.series().await, Err(Error::Bound));
    let r = reader("/never", vec![], json!([]), 1);
    assert_eq!(r.episodes(0).await, Err(Error::Malformed));
    assert_eq!(r.episode_files(0).await, Err(Error::Malformed));
    assert_eq!(r.movie_files(0).await, Err(Error::Malformed));
}

#[tokio::test]
async fn reader_bounds_and_transport_errors_fail_closed() {
    for cap in [0, 100_001] {
        assert!(
            Reader::new(
                Endpoint::new(
                    "http://fixture.invalid",
                    Auth::plex("synthetic").unwrap(),
                    Limits::default()
                )
                .unwrap(),
                Fixture {
                    expected_path: "/never",
                    query: vec![],
                    response: Mutex::new(None)
                },
                cap
            )
            .is_err()
        );
    }
    assert!(
        Reader::new(
            Endpoint::new(
                "http://fixture.invalid",
                Auth::plex("synthetic").unwrap(),
                Limits::default()
            )
            .unwrap(),
            Fixture {
                expected_path: "/never",
                query: vec![],
                response: Mutex::new(None)
            },
            100_000
        )
        .is_ok()
    );
    let r = reader("/api/v3/series", vec![], json!(null), 1);
    assert_eq!(r.series().await, Err(Error::Upstream));
    let r = reader("/library/sections", vec![], json!(null), 1);
    assert_eq!(r.sections().await, Err(Error::Upstream));
    let r = reader("/library/sections/2/all", pq("0", "1"), json!(null), 1);
    assert_eq!(
        r.plex_page(2, false, Cursor::default(), 2).await,
        Err(Error::Upstream)
    );
}

#[tokio::test]
async fn history_is_bounded_and_cannot_cross_account_boundaries() {
    let pq = vec![
        ("accountID", "7"),
        ("X-Plex-Container-Start", "3"),
        ("X-Plex-Container-Size", "2"),
        ("sort", "viewedAt:desc"),
    ];
    let item = json!({"ratingKey":"11","librarySectionID":"2","accountID":"7","viewedAt":100});
    let r = reader(
        "/status/sessions/history/all",
        pq.clone(),
        json!({"MediaContainer":{"size":1,"Metadata":[item.clone()]}}),
        2,
    );
    assert_eq!(
        r.recent_plays(7, 3, 2).await.unwrap(),
        vec![HistoryItem {
            rating_key: "11".into(),
            section_id: "2".into(),
            account_id: "7".into(),
            at: 100
        }]
    );
    for value in [
        json!({"MediaContainer":{"size":2,"Metadata":[item.clone()]}}),
        json!({"MediaContainer":{"size":3,"Metadata":[item.clone(),item.clone(),item.clone()]}}),
        json!({"MediaContainer":{"size":1,"Metadata":[{"ratingKey":"11","librarySectionID":"2","accountID":"8","viewedAt":100}]}}),
    ] {
        assert_eq!(
            reader("/status/sessions/history/all", pq.clone(), value, 3)
                .recent_plays(7, 3, 2)
                .await,
            Err(Error::Malformed)
        );
    }
    let tq = vec![
        ("apikey", "synthetic"),
        ("cmd", "get_history"),
        ("user_id", "7"),
        ("start", "3"),
        ("length", "2"),
    ];
    let item = json!({"rating_key":"11","section_id":"2","user_id":"7","date":100});
    let r = reader(
        "/api/v2",
        tq.clone(),
        json!({"response":{"result":"success","data":{"data":[item.clone()]}}}),
        2,
    );
    assert_eq!(
        r.tautulli_recent("synthetic", 7, 3, 2).await.unwrap(),
        vec![HistoryItem {
            rating_key: "11".into(),
            section_id: "2".into(),
            account_id: "7".into(),
            at: 100
        }]
    );
    for value in [
        json!({"response":{"result":"error","data":{"data":[]}}}),
        json!({"response":{"result":"success","data":{"data":[item.clone(),item.clone(),item.clone()]}}}),
        json!({"response":{"result":"success","data":{"data":[{"rating_key":"11","section_id":"2","user_id":"8","date":100}]}}}),
    ] {
        assert_eq!(
            reader("/api/v2", tq.clone(), value, 3)
                .tautulli_recent("synthetic", 7, 3, 2)
                .await,
            Err(Error::Malformed)
        );
    }
    for (user, size) in [(0, 2), (7, 0), (7, 1001)] {
        let r = reader("/never", vec![], json!(null), 1);
        assert_eq!(r.recent_plays(user, 3, size).await, Err(Error::Malformed));
        assert_eq!(
            r.tautulli_recent("synthetic", user, 3, size).await,
            Err(Error::Malformed)
        );
    }
    assert_eq!(
        reader("/api/v2", tq, json!(null), 2)
            .tautulli_recent("synthetic", 7, 3, 2)
            .await,
        Err(Error::Upstream)
    );
    assert_eq!(
        reader("/status/sessions/history/all", pq, json!(null), 2)
            .recent_plays(7, 3, 2)
            .await,
        Err(Error::Upstream)
    );
}

#[tokio::test]
async fn history_accepts_full_pages_and_numeric_account_ids() {
    let pq = vec![
        ("accountID", "7"),
        ("X-Plex-Container-Start", "3"),
        ("X-Plex-Container-Size", "2"),
        ("sort", "viewedAt:desc"),
    ];
    let item = json!({"ratingKey":"11","librarySectionID":2,"accountID":7,"viewedAt":100});
    assert_eq!(
        reader(
            "/status/sessions/history/all",
            pq,
            json!({"MediaContainer":{"size":2,"Metadata":[item.clone(),item]}}),
            2
        )
        .recent_plays(7, 3, 2)
        .await
        .unwrap()
        .len(),
        2
    );
    let tq = vec![
        ("apikey", "synthetic"),
        ("cmd", "get_history"),
        ("user_id", "7"),
        ("start", "3"),
        ("length", "2"),
    ];
    let item = json!({"rating_key":11,"section_id":2,"user_id":7,"date":100});
    let rows = reader(
        "/api/v2",
        tq,
        json!({"response":{"result":"success","data":{"data":[item.clone(),item]}}}),
        2,
    )
    .tautulli_recent("synthetic", 7, 3, 2)
    .await
    .unwrap();
    assert_eq!(rows[0].rating_key, "11");
    assert_eq!(rows.len(), 2);
    let r = reader("/never", vec![], json!(null), 1);
    assert_eq!(r.tautulli_recent("", 7, 3, 2).await, Err(Error::Malformed));
}
