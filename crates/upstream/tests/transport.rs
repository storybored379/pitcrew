use pitcrew_upstream::*;
use serde_json::{Value, json};
use std::{sync::Mutex, time::Duration};

struct Stub {
    response: Mutex<Option<Result<Response, Error>>>,
    requests: Mutex<Vec<Request>>,
}
impl Stub {
    fn response(status: u16, body: &[u8]) -> Self {
        Self {
            response: Mutex::new(Some(Ok(Response {
                status,
                body: body.to_vec(),
            }))),
            requests: Mutex::new(vec![]),
        }
    }
}
impl Transport for Stub {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async { self.response.lock().unwrap().take().unwrap() })
    }
}
impl Transport for &Stub {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        (*self).send(request)
    }
}
fn endpoint(limits: Limits) -> Endpoint {
    Endpoint::new(
        "https://upstream.invalid/proxy",
        Auth::arr("synthetic-test-key").unwrap(),
        limits,
    )
    .unwrap()
}
#[test]
fn rejects_unsafe_configuration_without_echoing_values() {
    for url in [
        "file:///tmp/a",
        "https://user:noreply@upstream.invalid",
        "https://upstream.invalid?token=secret",
        "https://upstream.invalid/#secret",
        "broken secret",
    ] {
        assert!(Endpoint::new(url, Auth::arr("key").unwrap(), Limits::default()).is_err());
    }
    for secret in ["", "secret\r\nHeader: value", "two words", "☃"] {
        assert!(Auth::arr(secret).is_err());
    }
    assert!(Auth::arr(&"x".repeat(4097)).is_err());
    assert!(
        Endpoint::new(
            "https://upstream.invalid",
            Auth::arr("key").unwrap(),
            Limits {
                timeout: Duration::ZERO,
                ..Limits::default()
            }
        )
        .is_err()
    );
    assert!(
        Endpoint::new(
            "https://upstream.invalid",
            Auth::arr("key").unwrap(),
            Limits {
                max_response_bytes: 0,
                ..Limits::default()
            }
        )
        .is_err()
    );
    assert!(HttpTransport::new().is_ok()); // construction performs no requests
}
#[test]
fn live_requires_both_explicit_mode_and_store() {
    assert_eq!(Mode::default(), Mode::Shadow);
    for (mode, store) in [
        (Mode::Shadow, false),
        (Mode::Shadow, true),
        (Mode::Live, false),
    ] {
        assert!(matches!(
            LivePermit::authorize(mode, store),
            Err(Error::ShadowDenied)
        ));
    }
    assert!(LivePermit::authorize(Mode::Live, true).is_ok());
}
#[tokio::test]
async fn proxy_prefix_headers_and_encoded_query_are_preserved() {
    let fake = Stub::response(200, b"{}");
    let client = JsonClient::new(endpoint(Limits::default()), &fake);
    let _: Value = client
        .get(
            "api/v3/manualimport",
            vec![("downloadId".into(), "synthetic &x=secret/#".into())],
        )
        .await
        .unwrap();
    let requests = fake.requests.lock().unwrap();
    let r = &requests[0];
    assert_eq!(r.url.path(), "/proxy/api/v3/manualimport");
    assert_eq!(
        r.url.query_pairs().collect::<Vec<_>>(),
        vec![("downloadId".into(), "synthetic &x=secret/#".into())]
    );
    assert!(!r.url.as_str().contains("synthetic-test-key"));
    assert_eq!(
        r.headers,
        vec![
            ("X-Api-Key".into(), "synthetic-test-key".into()),
            ("Accept".into(), "application/json".into())
        ]
    );
    assert_eq!(r.method, Method::Get);
    assert!(r.body.is_none());
}
#[tokio::test]
async fn malformed_and_oversized_responses_fail_closed() {
    for (status, body, expected) in [
        (401, b"secret".as_slice(), Error::Status(401)),
        (302, b"secret".as_slice(), Error::Status(302)),
        (200, b"secret".as_slice(), Error::InvalidResponse),
        (200, b"123456789".as_slice(), Error::TooLarge),
    ] {
        let fake = Stub::response(status, body);
        let client = JsonClient::new(
            endpoint(Limits {
                max_response_bytes: 8,
                ..Limits::default()
            }),
            &fake,
        );
        assert_eq!(
            client.get::<Value>("api/v3/queue", vec![]).await,
            Err(expected)
        );
        assert!(!expected.to_string().contains("secret"));
    }
}
#[tokio::test]
async fn endpoint_path_cannot_escape_origin_or_proxy() {
    let fake = Stub::response(200, b"{}");
    let client = JsonClient::new(endpoint(Limits::default()), &fake);
    for path in [
        "",
        "/api/v3",
        "../outside",
        "a/../b",
        "https://evil.invalid",
        "a?secret=yes",
        "a%2fb",
        "a\\b",
    ] {
        assert_eq!(
            client.get::<Value>(path, vec![]).await,
            Err(Error::InvalidInput)
        );
    }
    assert!(fake.requests.lock().unwrap().is_empty());
}
struct Pending;
impl Transport for Pending {
    fn send(&self, _: Request) -> ResponseFuture<'_> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test(start_paused = true)]
async fn deadline_covers_transport_and_does_not_retry_write() {
    let client = JsonClient::new(
        endpoint(Limits {
            timeout: Duration::from_secs(2),
            ..Limits::default()
        }),
        Pending,
    );
    assert_eq!(
        client.get::<Value>("api/v3/queue", vec![]).await,
        Err(Error::Timeout)
    );
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    assert_eq!(
        client
            .write::<Value>(
                &permit,
                Method::Post,
                "api/v3/release",
                vec![],
                Some(json!({}))
            )
            .await,
        Err(Error::Timeout)
    );
}
#[tokio::test]
async fn empty_delete_is_valid_and_get_cannot_use_write_surface() {
    let fake = Stub::response(204, b"");
    let client = JsonClient::new(endpoint(Limits::default()), &fake);
    let permit = LivePermit::authorize(Mode::Live, true).unwrap();
    assert_eq!(
        client
            .write::<Value>(&permit, Method::Get, "api/v3/queue", vec![], None)
            .await,
        Err(Error::InvalidInput)
    );
    assert_eq!(
        client
            .write::<Value>(&permit, Method::Delete, "api/v3/queue/3", vec![], None)
            .await,
        Ok(Value::Null)
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn agent_versioned_routes_header_and_validation() {
    for (route, number) in [
        ("health", 0),
        ("probe", 1),
        ("decodecheck", 2),
        ("file", 3),
        ("plexfails", 4),
    ] {
        let fake = Stub::response(200, b"{}");
        let ep = Endpoint::new(
            "https://agent.invalid/",
            Auth::bearer("synthetic-agent-key").unwrap(),
            Limits::default(),
        )
        .unwrap();
        let client = agent::Client::new(ep, &fake);
        match number {
            0 => client.health().await,
            1 => client.probe("/media/synthetic + test.mkv").await,
            2 => client.decode("/media/synthetic.mkv").await,
            3 => client.file("/media/synthetic.mkv").await,
            _ => client.plex_failures(60).await,
        }
        .unwrap();
        let requests = fake.requests.lock().unwrap();
        let r = &requests[0];
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.url.path(), format!("/v1/{route}"));
        assert_eq!(
            r.headers[0],
            ("Authorization".into(), "Bearer synthetic-agent-key".into())
        );
    }
    let fake = Stub::response(200, b"{}");
    let client = agent::Client::new(endpoint(Limits::default()), &fake);
    assert_eq!(client.probe("relative").await, Err(Error::InvalidInput));
    assert_eq!(client.decode("/nul\0").await, Err(Error::InvalidInput));
    assert_eq!(
        client.file(&format!("/{}", "x".repeat(3072))).await,
        Err(Error::InvalidInput)
    );
    assert_eq!(client.plex_failures(0).await, Err(Error::InvalidInput));
    assert_eq!(client.plex_failures(1441).await, Err(Error::InvalidInput));
    assert!(client.evidence("relative", true).await.is_err());
    assert!(fake.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn agent_evidence_retains_exact_identity_and_every_tested_range() {
    let body = json!({"file":{"id":17,"revision":3},"observed_at_unix_ms":123,"symlinks":null,"mount":null,"reads":[],"header":null,"decode":{"outcome":"Passed","duration_ms":100000,"elapsed_ms":40,"timeout_ms":180000,"faults":[],"streams":[],"tested_ranges":[{"start_ms":0,"end_ms":10000},{"start_ms":40000,"end_ms":50000},{"start_ms":80000,"end_ms":90000}]},"plex_failures":[]});
    let fake = Stub::response(200, &serde_json::to_vec(&body).unwrap());
    let client = agent::Client::new(endpoint(Limits::default()), &fake);
    let evidence = client
        .evidence("/media/synthetic + test.mkv", true)
        .await
        .unwrap();
    assert_eq!(
        evidence.file,
        pitcrew_domain::FileIdentity {
            id: 17,
            revision: 3
        }
    );
    assert_eq!(
        evidence
            .decode
            .unwrap()
            .tested_ranges
            .iter()
            .map(|r| r.start_ms)
            .collect::<Vec<_>>(),
        vec![0, 40000, 80000]
    );
    let requests = fake.requests.lock().unwrap();
    let r = &requests[0];
    assert_eq!(r.url.path(), "/proxy/v1/evidence");
    assert_eq!(
        r.url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("path".into(), "/media/synthetic + test.mkv".into()),
            ("decode".into(), "true".into())
        ]
    );
}
