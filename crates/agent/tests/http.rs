use pitcrew_agent_core::http::*;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const SECRET: &str = "synthetic-test-secret-32-characters";
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Operation>>,
    error: Option<ApiError>,
}
impl Inspector for Fake {
    fn inspect(&self, operation: Operation) -> InspectFuture<'_> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(operation);
            self.error.map_or(Ok(json!({"fixture": true})), Err)
        })
    }
}
fn app(compatibility: bool) -> (App, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    (
        App::new(
            Auth::new(SECRET).unwrap(),
            compatibility,
            fake.clone(),
            6000,
        )
        .unwrap(),
        fake,
    )
}
fn request(target: &str) -> Vec<u8> {
    format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {SECRET}\r\n\r\n")
        .into_bytes()
}
#[test]
fn credentials_validate_and_compare_without_exposing_values() {
    for invalid in [
        String::new(),
        "a".repeat(31),
        "a".repeat(4097),
        format!("{} ", "a".repeat(32)),
        "é".repeat(32),
        "\n".repeat(32),
    ] {
        assert!(Auth::new(&invalid).is_err());
    }
    for length in [32, 4096] {
        assert!(Auth::new(&"a".repeat(length)).is_ok());
    }
    let auth = Auth::new(SECRET).unwrap();
    assert!(auth.accepts(SECRET));
    assert!(!auth.accepts(""));
    assert!(!auth.accepts(&SECRET.replace('s', "t")));
    for n in 0..SECRET.len() {
        let mut bytes = SECRET.as_bytes().to_vec();
        bytes[n] ^= 1;
        assert!(!auth.accepts(std::str::from_utf8(&bytes).unwrap()));
    }
}
#[tokio::test]
async fn auth_precedes_path_and_route_access() {
    let (app, fake) = app(true);
    for raw in [b"GET /v1/probe?path=/secret HTTP/1.1\r\n\r\n".to_vec(), request("/private").into_iter().map(|b| if b == b'y' { b'z' } else { b }).collect(), format!("GET /v1/health HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\nAuthorization: Bearer {SECRET}\r\n\r\n").into_bytes()] {
        let response = app.handle(&raw).await;
        assert_eq!(response, Response {status:401, body:json!({"error":"unauthorized"})});
    }
    assert!(fake.calls.lock().unwrap().is_empty());
    assert_eq!(
        app.handle(&request("/v1/health")).await,
        Response {
            status: 200,
            body: json!({"status":"ok","api_version":1})
        }
    );
}
#[tokio::test]
async fn routes_preserve_exact_operations_and_defaults() {
    let (app, fake) = app(true);
    let cases = [
        (
            "/v1/evidence?path=%2Fmedia%2Fa%20b",
            Operation::Evidence {
                path: "/media/a b".into(),
                decode: false,
            },
        ),
        (
            "/v1/evidence?path=/media/a&decode=true",
            Operation::Evidence {
                path: "/media/a".into(),
                decode: true,
            },
        ),
        (
            "/v1/evidence?path=/media/a&decode=false",
            Operation::Evidence {
                path: "/media/a".into(),
                decode: false,
            },
        ),
        (
            "/probe?path=/media/a",
            Operation::Probe {
                path: "/media/a".into(),
            },
        ),
        (
            "/v1/decodecheck?path=/media/a",
            Operation::Decode {
                path: "/media/a".into(),
            },
        ),
        (
            "/file?path=/media/a",
            Operation::File {
                path: "/media/a".into(),
            },
        ),
        (
            "/plexfails",
            Operation::PlexFailures {
                minutes: 60,
                compatibility: true,
            },
        ),
        (
            "/v1/plexfails?minutes=1",
            Operation::PlexFailures {
                minutes: 1,
                compatibility: false,
            },
        ),
        (
            "/v1/plexfails?minutes=1440",
            Operation::PlexFailures {
                minutes: 1440,
                compatibility: false,
            },
        ),
    ];
    for (target, expected) in cases {
        assert_eq!(
            app.handle(&request(target)).await,
            Response {
                status: 200,
                body: json!({"fixture":true})
            }
        );
        assert_eq!(fake.calls.lock().unwrap().pop(), Some(expected));
    }
    for target in ["/health", "/sessions", "/tracearr-health", "/watchdog-jev"] {
        assert_eq!(
            app.handle(&request(target)).await,
            Response {
                status: 503,
                body: json!({"error":"integration_unavailable"})
            }
        );
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn bounds_and_ambiguous_requests_fail_closed() {
    let (app, fake) = app(true);
    for target in [
        "/v1/probe",
        "/v1/file?path=x",
        "/v1/file?path=/a%00",
        "/v1/file?path=/a&path=/b",
        "/v1/file?path=/a&bad=1",
        "/v1/file?path=/a&decode=true",
        "/v1/evidence?path=/a&decode=1",
        "/v1/plexfails?minutes=0",
        "/v1/plexfails?minutes=1441",
        "/v1/plexfails?minutes=bad",
        "/v1/plexfails?path=/x",
        "//host/v1/health",
        "/v1/health#fragment",
        "http://host/v1/health",
    ] {
        assert_eq!(app.handle(&request(target)).await.status, 400, "{target}");
    }
    assert_eq!(
        app.handle(&request(&format!("/v1/file?path=/{}", "a".repeat(3072))))
            .await
            .status,
        400
    );
    assert_eq!(
        app.handle(&request(&format!("/v1/file?path=/{}", "a".repeat(3071))))
            .await
            .status,
        200
    );
    fake.calls.lock().unwrap().clear();
    for header in ["Content-Length: 0", "Transfer-Encoding: chunked"] {
        let raw = format!(
            "GET /v1/health HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\n{header}\r\n\r\n"
        );
        assert_eq!(
            app.handle(raw.as_bytes()).await,
            Response {
                status: 400,
                body: json!({"error":"invalid_framing"})
            }
        );
    }
    let mut pipelined = request("/v1/health");
    pipelined.extend(request("/v1/health"));
    assert_eq!(app.handle(&pipelined).await.status, 400);
    assert_eq!(app.handle(b"garbage").await.status, 400);
    assert_eq!(app.handle(&vec![b'a'; MAX_HEADERS + 1]).await.status, 431);
    assert_eq!(
        app.handle(&request(&format!("/{}", "a".repeat(MAX_TARGET))))
            .await
            .status,
        414
    );
    assert_eq!(
        app.handle(&request(&format!("/{}", "a".repeat(MAX_TARGET - 1))))
            .await
            .status,
        404
    );
    let post = String::from_utf8(request("/v1/health"))
        .unwrap()
        .replacen("GET", "POST", 1);
    assert_eq!(app.handle(post.as_bytes()).await.status, 405);
    assert_eq!(app.handle(&request("/v1/missing")).await.status, 404);
    assert!(fake.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn compatibility_is_explicit_and_backend_failures_are_sanitized() {
    let (app, _) = app(false);
    assert_eq!(app.handle(&request("/probe?path=/a")).await.status, 404);
    for (error, status, code) in [
        (ApiError::Forbidden, 403, "path_forbidden"),
        (ApiError::Busy, 503, "busy"),
        (ApiError::Unavailable, 503, "unavailable"),
        (ApiError::BadRequest, 400, "invalid_request"),
    ] {
        let app = App::new(
            Auth::new(SECRET).unwrap(),
            false,
            Arc::new(Fake {
                error: Some(error),
                ..Default::default()
            }),
            1,
        )
        .unwrap();
        assert_eq!(
            app.handle(&request("/v1/probe?path=/a")).await,
            Response {
                status,
                body: json!({"error":code})
            }
        );
        assert_eq!(app.handle(&request("/v1/health")).await.status, 429);
    }
    for limit in [0, 6001] {
        assert!(
            App::new(
                Auth::new(SECRET).unwrap(),
                false,
                Arc::new(Fake::default()),
                limit
            )
            .is_err()
        );
    }
}
#[tokio::test]
async fn wire_has_exact_length_and_closes() {
    let (app, _) = app(false);
    let (mut client, mut server) = tokio::io::duplex(16384);
    let task = tokio::spawn(async move {
        serve_connection(
            &mut server,
            &app,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    });
    client.write_all(&request("/v1/health")).await.unwrap();
    let mut wire = String::new();
    client.read_to_string(&mut wire).await.unwrap();
    task.await.unwrap();
    let (head, body) = wire.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 Response\r\n"));
    assert!(head.contains(&format!("Content-Length: {}\r\n", body.len())));
    assert!(head.contains("Connection: close\r\n"));
    assert!(head.contains("Cache-Control: no-store\r\n"));
    assert!(head.contains("Content-Type: application/json\r\n"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        json!({"status":"ok","api_version":1})
    );
}
#[tokio::test]
async fn partial_headers_deadline_and_oversized_wire() {
    for (input, status) in [(b"GET /".to_vec(), 408), (vec![b'x'; MAX_HEADERS + 1], 431)] {
        let (app, _) = app(false);
        let (mut client, mut server) = tokio::io::duplex(32768);
        let task = tokio::spawn(async move {
            serve_connection(
                &mut server,
                &app,
                Duration::from_millis(20),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        });
        client.write_all(&input).await.unwrap();
        let mut wire = String::new();
        client.read_to_string(&mut wire).await.unwrap();
        task.await.unwrap();
        assert!(wire.starts_with(&format!("HTTP/1.1 {status} ")), "{wire}");
    }
}
struct Hung;
impl Inspector for Hung {
    fn inspect(&self, _: Operation) -> InspectFuture<'_> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
async fn operation_deadline_returns_without_waiting() {
    let app = App::new(Auth::new(SECRET).unwrap(), false, Arc::new(Hung), 1).unwrap();
    let (mut client, mut server) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        serve_connection(
            &mut server,
            &app,
            Duration::from_secs(1),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    });
    client
        .write_all(&request("/v1/probe?path=/a"))
        .await
        .unwrap();
    let mut wire = String::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_string(&mut wire))
        .await
        .unwrap()
        .unwrap();
    task.await.unwrap();
    assert!(wire.starts_with("HTTP/1.1 504 "));
}
struct Large;
impl Inspector for Large {
    fn inspect(&self, _: Operation) -> InspectFuture<'_> {
        Box::pin(async { Ok(json!({"data":"x".repeat(MAX_RESPONSE)})) })
    }
}
#[tokio::test]
async fn response_output_bound_and_read_error() {
    let app = App::new(Auth::new(SECRET).unwrap(), false, Arc::new(Large), 1).unwrap();
    let (mut client, mut server) = tokio::io::duplex(8192);
    let task = tokio::spawn(async move {
        serve_connection(
            &mut server,
            &app,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    });
    client
        .write_all(&request("/v1/probe?path=/a"))
        .await
        .unwrap();
    let mut wire = String::new();
    client.read_to_string(&mut wire).await.unwrap();
    task.await.unwrap();
    assert!(wire.starts_with("HTTP/1.1 503 "));
    assert!(wire.ends_with("{\"error\":\"response_too_large\"}"));
    let (app, _) = self::app(false);
    let (client, mut server) = tokio::io::duplex(8192);
    drop(client);
    assert_eq!(
        serve_connection(
            &mut server,
            &app,
            Duration::from_secs(1),
            Duration::from_secs(1)
        )
        .await
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}
#[tokio::test]
async fn local_listener_serves_and_caps_connections() {
    let (app, _) = app(false);
    let app = Arc::new(app);
    for (connections, deadline) in [
        (0, Duration::from_secs(1)),
        (257, Duration::from_secs(1)),
        (1, Duration::ZERO),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                serve(listener, app.clone(), connections, deadline),
            )
            .await
            .expect("invalid limits must be rejected before accepting connections")
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(serve(listener, app, 1, Duration::from_secs(1)));
    let mut first = tokio::net::TcpStream::connect(address).await.unwrap();
    first.write_all(b"GET ").await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut second = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut b = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), second.read(&mut b))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    first.write_all(&request("/v1/health")[4..]).await.unwrap();
    let mut wire = String::new();
    tokio::time::timeout(Duration::from_secs(2), first.read_to_string(&mut wire))
        .await
        .unwrap()
        .unwrap();
    assert!(wire.starts_with("HTTP/1.1 200 "));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn exact_header_limit_is_accepted() {
    let (app, _) = app(false);
    let prefix =
        format!("GET /v1/health HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\nX-Padding: ");
    let raw = format!(
        "{}{}\r\n\r\n",
        prefix,
        "a".repeat(MAX_HEADERS - prefix.len() - 4)
    );
    assert_eq!(raw.len(), MAX_HEADERS);
    assert_eq!(app.handle(raw.as_bytes()).await.status, 200);
    let boundary_request = raw.clone();
    let raw = format!(
        "{}{}\r\n\r\n",
        prefix,
        "a".repeat(MAX_HEADERS - prefix.len() - 3)
    );
    assert_eq!(app.handle(raw.as_bytes()).await.status, 431);
    let (mut client, mut server) = tokio::io::duplex(16384);
    let task = tokio::spawn(async move {
        serve_connection(
            &mut server,
            &app,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    });
    client.write_all(boundary_request.as_bytes()).await.unwrap();
    let mut wire = String::new();
    client.read_to_string(&mut wire).await.unwrap();
    task.await.unwrap();
    assert!(wire.starts_with("HTTP/1.1 200 "));
}
struct ExactLimit;
impl Inspector for ExactLimit {
    fn inspect(&self, _: Operation) -> InspectFuture<'_> {
        Box::pin(async { Ok(json!("x".repeat(MAX_RESPONSE - 2))) })
    }
}
#[tokio::test]
async fn exact_response_limit_is_accepted_and_write_deadline_is_bounded() {
    let app = App::new(Auth::new(SECRET).unwrap(), false, Arc::new(ExactLimit), 2).unwrap();
    let (mut client, mut server) = tokio::io::duplex(16384);
    let task = tokio::spawn(async move {
        serve_connection(
            &mut server,
            &app,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    });
    client
        .write_all(&request("/v1/probe?path=/a"))
        .await
        .unwrap();
    let mut wire = String::new();
    client.read_to_string(&mut wire).await.unwrap();
    task.await.unwrap();
    assert!(wire.starts_with("HTTP/1.1 200 "));
    assert_eq!(wire.split_once("\r\n\r\n").unwrap().1.len(), MAX_RESPONSE);
    let (app, _) = self::app(false);
    let (mut client, mut server) = tokio::io::duplex(64);
    let task = tokio::spawn(async move {
        serve_connection(
            &mut server,
            &app,
            Duration::from_millis(50),
            Duration::from_secs(1),
        )
        .await
    });
    client.write_all(&request("/v1/health")).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
}

struct CancellationProbe {
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}
struct MarkCancelled(Arc<std::sync::atomic::AtomicBool>);
impl Drop for MarkCancelled {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Inspector for CancellationProbe {
    fn inspect(&self, _: Operation) -> InspectFuture<'_> {
        Box::pin(async {
            let _guard = MarkCancelled(self.cancelled.clone());
            self.started.notify_one();
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn stopping_server_cancels_in_flight_inspection_and_closes_connection() {
    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let app = Arc::new(
        App::new(
            Auth::new(SECRET).unwrap(),
            false,
            Arc::new(CancellationProbe {
                started: started.clone(),
                cancelled: cancelled.clone(),
            }),
            1,
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, app, 1, Duration::from_secs(60)));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(&request("/v1/probe?path=/synthetic"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let mut wire = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut wire))
        .await
        .unwrap()
        .unwrap();
    assert!(wire.is_empty());
    assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
}
