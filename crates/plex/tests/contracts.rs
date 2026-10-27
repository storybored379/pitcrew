use pitcrew_plex::Client;
use pitcrew_upstream::*;
use serde_json::{Value, json};
use std::sync::Mutex;
struct Fake {
    body: Value,
    requests: Mutex<Vec<Request>>,
}
impl Transport for &Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get);
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            Ok(Response {
                status: 200,
                body: serde_json::to_vec(&self.body).unwrap(),
            })
        })
    }
}
fn client(f: &Fake) -> Client<&Fake> {
    Client::new(
        Endpoint::new(
            "https://plex.invalid",
            Auth::plex("synthetic-plex-key").unwrap(),
            Limits::default(),
        )
        .unwrap(),
        f,
    )
}
#[tokio::test]
async fn sections_metadata_sessions_use_json_envelope_and_header_token() {
    for (number, route, body) in [
        (
            0,
            "/library/sections",
            json!({"MediaContainer":{"size":1,"Directory":[{"key":"1","type":"movie","title":"Synthetic"}]}}),
        ),
        (
            1,
            "/library/metadata/2",
            json!({"MediaContainer":{"size":1,"Metadata":[{"ratingKey":"2","Media":[{"Part":[{"file":"/media/synthetic.mkv"}]}]}]}}),
        ),
        (
            2,
            "/status/sessions",
            json!({"MediaContainer":{"size":1,"Metadata":[{"ratingKey":"2","sessionKey":"7","Session":{"id":"synthetic-session"}}]}}),
        ),
    ] {
        let fake = Fake {
            body,
            requests: Mutex::new(vec![]),
        };
        let c = client(&fake);
        match number {
            0 => assert_eq!(c.sections().await.unwrap()[0].key, "1"),
            1 => assert_eq!(c.metadata(2).await.unwrap()[0].rating_key, "2"),
            _ => assert_eq!(
                c.sessions().await.unwrap()[0].session_key.as_deref(),
                Some("7")
            ),
        };
        let requests = fake.requests.lock().unwrap();
        let r = &requests[0];
        assert_eq!(r.url.path(), route);
        assert!(r.url.query().is_none());
        assert_eq!(
            r.headers[0],
            ("X-Plex-Token".into(), "synthetic-plex-key".into())
        );
    }
}
#[tokio::test]
async fn missing_or_partial_sessions_are_unknown_never_zero_streams() {
    for body in [
        json!({}),
        json!({"MediaContainer":{}}),
        json!({"MediaContainer":{"size":1}}),
        json!({"MediaContainer":{"size":2,"Metadata":[{}]}}),
    ] {
        let fake = Fake {
            body,
            requests: Mutex::new(vec![]),
        };
        assert!(matches!(
            client(&fake).sessions().await,
            Err(Error::InvalidResponse)
        ));
    }
    let fake = Fake {
        body: json!({"MediaContainer":{"size":0}}),
        requests: Mutex::new(vec![]),
    };
    assert!(client(&fake).sessions().await.unwrap().is_empty());
    assert!(matches!(
        client(&fake).metadata(0).await,
        Err(Error::InvalidInput)
    ));
}
#[tokio::test]
async fn identity_returns_only_a_well_formed_machine_identifier() {
    for (body, expected) in [
        (
            json!({"MediaContainer":{"machineIdentifier":"synthetic-0a1b"}}),
            Some("synthetic-0a1b"),
        ),
        (json!({"MediaContainer":{"machineIdentifier":""}}), None),
        (
            json!({"MediaContainer":{"machineIdentifier":"a/../b"}}),
            None,
        ),
        (
            json!({"MediaContainer":{"machineIdentifier":"a".repeat(129)}}),
            None,
        ),
        (
            json!({"MediaContainer":{"machineIdentifier":"a".repeat(128)}}),
            Some(&*"a".repeat(128)),
        ),
        (json!({"MediaContainer":{}}), None),
    ] {
        let fake = Fake {
            body,
            requests: Mutex::new(vec![]),
        };
        let result = client(&fake).identity().await;
        match expected {
            Some(id) => assert_eq!(result.unwrap(), id),
            None => assert!(result.is_err()),
        }
        assert_eq!(fake.requests.lock().unwrap()[0].url.path(), "/identity");
    }
}
