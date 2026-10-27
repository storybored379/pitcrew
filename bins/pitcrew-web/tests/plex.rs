use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
};
use pitcrew_web::auth::{Auth, AuthError, PlexClient};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
#[derive(Clone)]
struct Fake {
    responses: Arc<Mutex<BTreeMap<String, (StatusCode, String)>>>,
    seen: Arc<Mutex<Vec<(String, String, HeaderMap)>>>,
}
async fn handle(State(fake): State<Fake>, request: Request<Body>) -> Response {
    let path = request.uri().to_string();
    fake.seen.lock().unwrap().push((
        request.method().to_string(),
        path.clone(),
        request.headers().clone(),
    ));
    let (status, body) = fake
        .responses
        .lock()
        .unwrap()
        .get(&path)
        .cloned()
        .unwrap_or((StatusCode::NOT_FOUND, "missing".into()));
    (status, body).into_response()
}
struct Server {
    fake: Fake,
    client: PlexClient,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    fn set(&self, path: &str, value: serde_json::Value) {
        self.fake
            .responses
            .lock()
            .unwrap()
            .insert(path.into(), (StatusCode::OK, value.to_string()));
    }
}
async fn setup() -> Server {
    let fake = Fake {
        responses: Arc::new(Mutex::new(BTreeMap::new())),
        seen: Arc::new(Mutex::new(vec![])),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new().fallback(handle).with_state(fake.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let server = Server {
        fake,
        client: PlexClient::new(&url, &url, "expected-server", "installation").unwrap(),
        task,
    };
    server.set("/api/v2/pins?strong=true", json!({"id":42,"code":"ABCD"}));
    server.set("/api/v2/pins/42", json!({"authToken":"viewer-secret"}));
    server.set("/api/v2/user", json!({"id":17}));
    server.set(
        "/api/v2/resources?includeHttps=1",
        json!([{"clientIdentifier":"expected-server","provides":"client,server"}]),
    );
    server.set(
        "/library/sections",
        json!({"MediaContainer":{"size":2,"Directory":[{"key":"hd"},{"key":"tv"}]}}),
    );
    server
}
#[tokio::test]
async fn real_http_pin_identity_and_viewer_sections_contract() {
    let s = setup().await;
    let pin = s.client.create_pin().await.unwrap();
    assert_eq!(pin.id, 42);
    assert_eq!(pin.code, "ABCD");
    let account = s.client.poll(42).await.unwrap().unwrap();
    assert_eq!(account.id, "17");
    assert_eq!(account.token, "viewer-secret");
    assert_eq!(
        s.client.access(&account.token).await.unwrap(),
        vec!["hd", "tv"]
    );
    let seen = s.fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].0, "POST");
    for (i, (method, path, headers)) in seen.iter().enumerate() {
        assert!(!path.contains("viewer-secret"));
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(headers["x-plex-client-identifier"], "installation");
        assert_eq!(headers["x-plex-product"], "Pit Crew");
        if i >= 2 {
            assert_eq!(headers["x-plex-token"], "viewer-secret")
        } else {
            assert!(headers.get("x-plex-token").is_none());
        }
        if i != 0 {
            assert_eq!(method, "GET")
        }
    }
}
#[tokio::test]
async fn identity_membership_and_sections_are_fail_closed_without_cached_fallback() {
    let s = setup().await;
    for pin in [
        json!({}),
        json!({"authToken":null}),
        json!({"authToken":""}),
    ] {
        s.set("/api/v2/pins/42", pin);
        assert!(s.client.poll(42).await.unwrap().is_none());
    }
    s.set("/api/v2/pins/42", json!({"authToken":"secret"}));
    s.set("/api/v2/user", json!({"id":0}));
    assert!(matches!(
        s.client.poll(42).await,
        Err(AuthError::Unavailable)
    ));
    for value in [
        json!({"id":0,"code":"a"}),
        json!({"id":1,"code":""}),
        json!({"id":"bad"}),
    ] {
        s.set("/api/v2/pins?strong=true", value);
        assert!(matches!(
            s.client.create_pin().await,
            Err(AuthError::Unavailable)
        ));
    }
    for resources in [
        json!([]),
        json!([{"clientIdentifier":"another-server","provides":"server"}]),
        json!([{"clientIdentifier":"expected-server","provides":"client"}]),
    ] {
        s.set("/api/v2/resources?includeHttps=1", resources);
        assert_eq!(
            s.client.access("secret").await.unwrap_err(),
            AuthError::Denied
        );
    }
    s.set(
        "/api/v2/resources?includeHttps=1",
        json!([{"clientIdentifier":"expected-server","provides":"server"}]),
    );
    for sections in [
        json!({}),
        json!({"MediaContainer":{"size":1}}),
        json!({"MediaContainer":{"size":1,"Directory":[{"key":""}]}}),
        json!({"MediaContainer":{"size":0,"Directory":[{"key":"x"}]}}),
    ] {
        s.set("/library/sections", sections);
        assert_eq!(
            s.client.access("secret").await.unwrap_err(),
            AuthError::Unavailable
        );
    }
    s.set("/library/sections", json!({"MediaContainer":{"size":0}}));
    assert!(s.client.access("secret").await.unwrap().is_empty());
    s.fake.responses.lock().unwrap().insert(
        "/api/v2/resources?includeHttps=1".into(),
        (StatusCode::INTERNAL_SERVER_ERROR, "private-error".into()),
    );
    assert_eq!(
        s.client.access("secret").await.unwrap_err(),
        AuthError::Unavailable
    );
    assert!(!AuthError::Unavailable.to_string().contains("private-error"));
}
#[tokio::test]
async fn response_size_boundary_is_enforced_before_json_decode() {
    let s = setup().await;
    let mut body = json!({"id":42,"code":"ABCD"}).to_string();
    body.extend(std::iter::repeat_n(' ', 1_048_576 - body.len()));
    s.fake.responses.lock().unwrap().insert(
        "/api/v2/pins?strong=true".into(),
        (StatusCode::OK, body.clone()),
    );
    assert_eq!(s.client.create_pin().await.unwrap().id, 42);
    body.push(' ');
    s.fake
        .responses
        .lock()
        .unwrap()
        .insert("/api/v2/pins?strong=true".into(), (StatusCode::OK, body));
    assert!(matches!(
        s.client.create_pin().await,
        Err(AuthError::Unavailable)
    ));
}
#[test]
fn endpoint_configuration_has_no_embedded_credentials_queries_or_path_escape() {
    for url in [
        "broken",
        "ftp://example.test/",
        "https://noreply@example.test/",
        "https://example.test/?secret=1",
        "https://example.test/#x",
        "https://example.test/path",
    ] {
        assert!(matches!(
            PlexClient::new(url, "https://example.test/", "s", "c"),
            Err(AuthError::Configuration)
        ));
        assert!(matches!(
            PlexClient::new("https://example.test/", url, "s", "c"),
            Err(AuthError::Configuration)
        ));
    }
    for (server, client) in [("", "c"), ("s", "")] {
        assert!(matches!(
            PlexClient::new(
                "https://example.test/",
                "https://example.test/",
                server,
                client
            ),
            Err(AuthError::Configuration)
        ));
    }
}
