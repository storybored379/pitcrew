//! Synthetic contracts for mutation request boundaries. Rejected requests must
//! not reach the transport, even when a valid live permit exists.
use pitcrew_arr::{Client, ImportFile, Kind, Page};
use pitcrew_upstream::{
    Auth, Endpoint, Error, Limits, LivePermit, Mode, Request, Response, ResponseFuture, Transport,
};
use serde_json::{Value, json};
use std::sync::Mutex;

struct Recording {
    body: Value,
    requests: Mutex<Vec<Request>>,
}

impl Transport for &Recording {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&self.body).unwrap(),
            })
        })
    }
}

fn recording(body: Value) -> Recording {
    Recording {
        body,
        requests: Mutex::default(),
    }
}

fn client(fake: &Recording, kind: Kind) -> Client<&Recording> {
    Client::new(
        Endpoint::new(
            "https://arr.invalid/",
            Auth::arr("synthetic-key").unwrap(),
            Limits::default(),
        )
        .unwrap(),
        fake,
        kind,
    )
}

fn file(kind: Kind) -> ImportFile {
    ImportFile {
        path: "/downloads/invented.mkv".into(),
        download_id: "download-7".into(),
        series_id: (kind == Kind::Sonarr).then_some(3),
        movie_id: (kind == Kind::Radarr).then_some(5),
        episode_ids: if kind == Kind::Sonarr {
            vec![5]
        } else {
            vec![]
        },
        quality: json!({"quality":{"id":4}}),
        languages: vec![json!({"id":1})],
        release_group: "Invented".into(),
    }
}

#[tokio::test]
async fn installed_file_requires_a_positive_integer_for_the_correct_media_kind() {
    for (kind, key, wrong_key) in [
        (Kind::Sonarr, "episodeFileId", "movieFileId"),
        (Kind::Radarr, "movieFileId", "episodeFileId"),
    ] {
        for body in [
            json!({}),
            json!({key:0}),
            json!({key:-1}),
            json!({key:"8"}),
            json!({key:1.5}),
            json!({wrong_key:8}),
        ] {
            let fake = recording(body);
            let c = client(&fake, kind);
            assert_eq!(c.kind(), kind);
            assert_eq!(c.installed_file(5).await, Err(Error::InvalidResponse));
        }
        let fake = recording(json!({key:1}));
        let c = client(&fake, kind);
        assert_eq!(c.installed_file(0).await, Err(Error::InvalidInput));
        assert!(fake.requests.lock().unwrap().is_empty());
        assert_eq!(c.installed_file(5).await, Ok(1));
    }
}

#[tokio::test]
async fn pagination_preserves_requested_boundaries_and_never_sends_zero_size() {
    let fake = recording(json!({"page":1,"pageSize":1,"totalRecords":0,"records":[]}));
    let c = client(&fake, Kind::Sonarr);
    for page in [
        Page { number: 1, size: 1 },
        Page {
            number: u32::MAX,
            size: 1000,
        },
    ] {
        c.queue(page).await.unwrap();
        let requests = fake.requests.lock().unwrap();
        let query: Vec<_> = requests
            .last()
            .unwrap()
            .url
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(
            query,
            vec![
                ("page".into(), page.number.to_string()),
                ("pageSize".into(), page.size.to_string())
            ]
        );
    }
    assert!(matches!(
        c.queue(Page { number: 1, size: 0 }).await,
        Err(Error::InvalidInput)
    ));
    assert_eq!(fake.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn import_batch_and_path_boundaries_accept_maximum_and_reject_larger_values() {
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    for kind in [Kind::Sonarr, Kind::Radarr] {
        let fake = recording(json!({"id":3}));
        let c = client(&fake, kind);
        let mut longest = file(kind);
        longest.path = format!("/{}", "p".repeat(4095));
        assert_eq!(
            c.manual_import(&permit, &[longest.clone()]).await.unwrap(),
            json!({"id":3})
        );
        assert_eq!(
            c.manual_import(&permit, &vec![file(kind); 100])
                .await
                .unwrap(),
            json!({"id":3})
        );
        assert_eq!(fake.requests.lock().unwrap().len(), 2);
        assert_eq!(
            c.manual_import(&permit, &vec![file(kind); 101]).await,
            Err(Error::InvalidInput)
        );
        longest.path.push('p');
        assert_eq!(
            c.manual_import(&permit, &[longest]).await,
            Err(Error::InvalidInput)
        );
        for path in ["", "/downloads/bad\0path"] {
            let mut input = file(kind);
            input.path = path.into();
            assert_eq!(
                c.manual_import(&permit, &[input]).await,
                Err(Error::InvalidInput)
            );
        }
        for quality in [Value::Null, json!([]), json!("HD")] {
            let mut input = file(kind);
            input.quality = quality;
            assert_eq!(
                c.manual_import(&permit, &[input]).await,
                Err(Error::InvalidInput)
            );
        }
        assert_eq!(fake.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn each_media_identity_field_is_required_and_cross_kind_fields_are_rejected() {
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    let fake = recording(json!({"id":3}));
    let mut episodes = vec![file(Kind::Sonarr); 6];
    episodes[0].series_id = None;
    episodes[1].series_id = Some(0);
    episodes[2].movie_id = Some(5);
    episodes[3].episode_ids.clear();
    episodes[4].episode_ids = vec![0];
    episodes[5].episode_ids.push(0);
    for input in episodes {
        assert_eq!(
            client(&fake, Kind::Sonarr)
                .manual_import(&permit, &[input])
                .await,
            Err(Error::InvalidInput)
        );
    }
    let mut movies = vec![file(Kind::Radarr); 4];
    movies[0].movie_id = None;
    movies[1].movie_id = Some(0);
    movies[2].series_id = Some(3);
    movies[3].episode_ids = vec![5];
    for input in movies {
        assert_eq!(
            client(&fake, Kind::Radarr)
                .manual_import(&permit, &[input])
                .await,
            Err(Error::InvalidInput)
        );
    }
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn identifiers_reject_control_characters_and_lengths_over_the_exact_byte_limit() {
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    let fake = recording(json!({"id":3}));
    let c = client(&fake, Kind::Sonarr);
    let longest = "x".repeat(4096);
    assert_eq!(
        c.grab(&permit, 5, &longest, 3).await.unwrap(),
        json!({"id":3})
    );
    for identifier in [
        "".into(),
        "x".repeat(4097),
        "bad\0id".into(),
        "bad\tid".into(),
        "bad\u{7f}id".into(),
    ] {
        assert_eq!(
            c.grab(&permit, 5, &identifier, 3).await,
            Err(Error::InvalidInput)
        );
        let mut input = file(Kind::Sonarr);
        input.download_id = identifier;
        assert_eq!(
            c.manual_import(&permit, &[input]).await,
            Err(Error::InvalidInput)
        );
    }
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}
