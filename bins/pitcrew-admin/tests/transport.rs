use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
};
use pitcrew_admin::{
    actions::{AdminApi, Command, HttpAdminApi, Outcome},
    auth::{Auth, PlexAuth},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
type ResponseMap = BTreeMap<String, (StatusCode, String)>;
type Seen = Vec<(String, String, HeaderMap, Value)>;
#[derive(Clone, Default)]
struct Fake {
    responses: Arc<Mutex<ResponseMap>>,
    seen: Arc<Mutex<Seen>>,
}
async fn handler(State(fake): State<Fake>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 65536).await.unwrap();
    fake.seen.lock().unwrap().push((
        parts.method.to_string(),
        parts.uri.to_string(),
        parts.headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    ));
    let (status, body) = fake
        .responses
        .lock()
        .unwrap()
        .get(&parts.uri.to_string())
        .cloned()
        .unwrap_or((StatusCode::NOT_FOUND, String::new()));
    (
        status,
        [("location", "http://127.0.0.1:1/never-follow")],
        body,
    )
        .into_response()
}
struct Server {
    fake: Fake,
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let fake = Fake::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new().fallback(handler).with_state(fake.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { fake, url, task }
    }
    fn set(&self, path: &str, status: StatusCode, body: &str) {
        self.fake
            .responses
            .lock()
            .unwrap()
            .insert(path.into(), (status, body.into()));
    }
    fn json(&self, path: &str, value: Value) {
        self.set(path, StatusCode::OK, &value.to_string());
    }
}
#[tokio::test]
async fn plex_pin_identity_contract_uses_headers_and_discards_token() {
    let server = Server::new().await;
    server.json("/api/v2/pins?strong=true", json!({"id":42,"code":"ABCD"}));
    server.json("/api/v2/pins/42", json!({"authToken":"synthetic-secret"}));
    server.json("/api/v2/user", json!({"id":1001}));
    let auth = PlexAuth::new(&server.url, "install-a").unwrap();
    let pin = auth.start().await.unwrap();
    assert_eq!(pin.id, 42);
    assert_eq!(pin.code, "ABCD");
    assert_eq!(auth.poll(42).await.unwrap(), Some("1001".into()));
    let seen = server.fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].0, "POST");
    assert_eq!(seen[1].0, "GET");
    assert_eq!(seen[2].0, "GET");
    for (_, uri, headers, body) in seen.iter() {
        assert!(!uri.contains("synthetic-secret"));
        assert_eq!(headers["accept"], "application/json");
        assert_eq!(headers["x-plex-product"], "Pit Crew Admin");
        assert_eq!(headers["x-plex-client-identifier"], "install-a");
        assert!(body.is_null());
    }
    assert!(!seen[0].2.contains_key("x-plex-token"));
    assert!(!seen[1].2.contains_key("x-plex-token"));
    assert_eq!(seen[2].2["x-plex-token"], "synthetic-secret");
}
#[tokio::test]
async fn plex_pending_invalid_identity_and_invalid_pin_never_authenticate() {
    let server = Server::new().await;
    let auth = PlexAuth::new(&server.url, "install").unwrap();
    for value in [
        json!({}),
        json!({"authToken":null}),
        json!({"authToken":""}),
    ] {
        server.json("/api/v2/pins/7", value);
        assert_eq!(auth.poll(7).await.unwrap(), None);
    }
    for value in [
        json!({"id":0,"code":"AB"}),
        json!({"id":1,"code":""}),
        json!({"id":1}),
        json!({"id":-1,"code":"A"}),
    ] {
        server.json("/api/v2/pins?strong=true", value);
        assert!(auth.start().await.is_err());
    }
    server.json("/api/v2/pins/7", json!({"authToken":"token"}));
    for value in [
        json!({"id":0}),
        json!({"id":"1001"}),
        json!({"id":-1}),
        json!({"username":"1001"}),
    ] {
        server.json("/api/v2/user", value);
        assert!(auth.poll(7).await.is_err());
    }
}
#[tokio::test]
async fn plex_transport_rejects_redirects_errors_malformed_and_oversized_responses() {
    let server = Server::new().await;
    let auth = PlexAuth::new(&server.url, "install").unwrap();
    for status in [
        StatusCode::FOUND,
        StatusCode::UNAUTHORIZED,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        server.set(
            "/api/v2/pins?strong=true",
            status,
            r#"{"id":1,"code":"AB"}"#,
        );
        assert!(auth.start().await.is_err());
    }
    server.set("/api/v2/pins?strong=true", StatusCode::OK, "bad");
    assert!(auth.start().await.is_err());
    let valid = r#"{"id":1,"code":"AB"}"#;
    let exact = format!("{valid}{}", " ".repeat(65536 - valid.len()));
    server.set("/api/v2/pins?strong=true", StatusCode::OK, &exact);
    assert_eq!(auth.start().await.unwrap().code, "AB");
    server.set(
        "/api/v2/pins?strong=true",
        StatusCode::OK,
        &format!("{exact} "),
    );
    assert!(auth.start().await.is_err());
    assert_eq!(server.fake.seen.lock().unwrap().len(), 6);
}
#[tokio::test]
async fn admin_transport_is_narrow_authenticated_and_never_follows_or_retries() {
    let server = Server::new().await;
    let secret = "s".repeat(32);
    let api = HttpAdminApi::new(&server.url, &secret).unwrap();
    for (command, path, ack, body) in [
        (
            Command::Release { id: 3 },
            "/v1/admin/exclusions/release",
            json!({"released":true}),
            json!({"id":3,"note":"reviewed"}),
        ),
        (
            Command::Reset {
                client: "arr-a".into(),
            },
            "/v1/admin/capabilities/reset",
            json!({"reset":true}),
            json!({"client":"arr-a","note":"reviewed"}),
        ),
        (
            Command::Resend {
                job: 3,
                sequence: 4,
            },
            "/v1/admin/notifications/resend",
            json!({"scheduled":false}),
            json!({"job":3,"sequence":4,"request_key":"request-a","note":"reviewed"}),
        ),
    ] {
        server.json(path, ack);
        assert_eq!(
            api.submit(&command, "request-a", "reviewed").await,
            Outcome::Accepted
        );
        let seen = server.fake.seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert_eq!(last.0, "POST");
        assert_eq!(last.1, path);
        assert_eq!(last.2["authorization"], format!("Bearer {secret}"));
        assert_eq!(last.3, body);
    }
    let path = "/v1/admin/exclusions/release";
    let command = Command::Release { id: 3 };
    for (status, expected) in [
        (StatusCode::BAD_REQUEST, Outcome::Rejected),
        (StatusCode::UNAUTHORIZED, Outcome::Rejected),
        (StatusCode::CONFLICT, Outcome::Rejected),
        (StatusCode::FOUND, Outcome::Uncertain),
        (StatusCode::INTERNAL_SERVER_ERROR, Outcome::Uncertain),
    ] {
        server.set(path, status, "{}");
        assert_eq!(api.submit(&command, "k", "n").await, expected);
    }
    for body in ["broken", "{}", r#"{"released":1}"#, r#"{"scheduled":true}"#] {
        server.set(path, StatusCode::OK, body);
        assert_eq!(api.submit(&command, "k", "n").await, Outcome::Uncertain);
    }
    assert_eq!(
        api.submit(&Command::Close { report: 1 }, "k", "n").await,
        Outcome::Uncertain
    );
    assert_eq!(server.fake.seen.lock().unwrap().len(), 12);
}
#[tokio::test]
async fn admin_ack_size_limit_exact_and_unavailable_service_are_uncertain() {
    let server = Server::new().await;
    let api = HttpAdminApi::new(&server.url, &"s".repeat(32)).unwrap();
    let cmd = Command::Reset { client: "a".into() };
    let body = r#"{"reset":true}"#;
    let exact = format!("{body}{}", " ".repeat(65536 - body.len()));
    server.set("/v1/admin/capabilities/reset", StatusCode::OK, &exact);
    assert_eq!(api.submit(&cmd, "k", "n").await, Outcome::Accepted);
    server.set(
        "/v1/admin/capabilities/reset",
        StatusCode::OK,
        &format!("{exact} "),
    );
    assert_eq!(api.submit(&cmd, "k", "n").await, Outcome::Uncertain);
    server.task.abort();
    tokio::task::yield_now().await;
    assert_eq!(api.submit(&cmd, "k", "n").await, Outcome::Uncertain);
}
#[test]
fn admin_secrets_have_explicit_bounds_and_no_whitespace() {
    for secret in [
        "".into(),
        "a".repeat(31),
        "a".repeat(4097),
        format!("{} ", "a".repeat(32)),
        format!("{}\n", "a".repeat(32)),
        "é".repeat(32),
    ] {
        assert!(HttpAdminApi::new("https://admin.example.invalid/", &secret).is_err());
    }
    for secret in ["a".repeat(32), "!~".repeat(2048)] {
        assert!(HttpAdminApi::new("https://admin.example.invalid/", &secret).is_ok());
    }
}
