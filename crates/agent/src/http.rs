//! Authenticated, bounded HTTP/1 GET transport. One request per connection; no
//! request bodies, keep-alive, forwarding headers or upstream network access.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Semaphore,
};

pub const MAX_HEADERS: usize = 8192;
pub const MAX_TARGET: usize = 4096;
pub const MAX_RESPONSE: usize = 1_048_576;

/// No Debug implementation: neither the original credential nor its hash is logged.
pub struct Auth([u8; 32]);
impl Auth {
    pub fn new(secret: &str) -> Result<Self, &'static str> {
        if !(32..=4096).contains(&secret.len()) || !secret.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("agent secret must contain 32..4096 visible ASCII bytes");
        }
        Ok(Self(Sha256::digest(secret.as_bytes()).into()))
    }
    pub fn accepts(&self, bearer: &str) -> bool {
        let candidate: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
        bool::from(self.0.ct_eq(&candidate))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Evidence { path: String, decode: bool },
    Probe { path: String },
    Decode { path: String },
    File { path: String },
    PlexFailures { minutes: u32, compatibility: bool },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiError {
    BadRequest,
    Forbidden,
    Busy,
    Unavailable,
}
pub type InspectFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'a>>;
pub trait Inspector: Send + Sync {
    fn inspect(&self, operation: Operation) -> InspectFuture<'_>;
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Value,
}
impl Response {
    fn error(status: u16, code: &str) -> Self {
        Self {
            status,
            body: json!({"error": code}),
        }
    }
    fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }
}

struct Window {
    start: Instant,
    requests: u32,
}
pub struct App {
    auth: Auth,
    compatibility: bool,
    inspector: Arc<dyn Inspector>,
    window: Mutex<Window>,
    requests_per_minute: u32,
}
impl App {
    pub fn new(
        auth: Auth,
        compatibility: bool,
        inspector: Arc<dyn Inspector>,
        requests_per_minute: u32,
    ) -> Result<Self, &'static str> {
        if !(1..=6000).contains(&requests_per_minute) {
            return Err("invalid request rate limit");
        }
        Ok(Self {
            auth,
            compatibility,
            inspector,
            window: Mutex::new(Window {
                start: Instant::now(),
                requests: 0,
            }),
            requests_per_minute,
        })
    }
    fn allow_request(&self, now: Instant) -> bool {
        let Ok(mut window) = self.window.lock() else {
            return false;
        };
        if now.duration_since(window.start) >= Duration::from_secs(60) {
            window.start = now;
            window.requests = 0;
        }
        if window.requests >= self.requests_per_minute {
            return false;
        }
        window.requests += 1;
        true
    }
    /// Parse and authenticate before inspecting paths, reading files or dispatching.
    pub async fn handle(&self, raw: &[u8]) -> Response {
        if !self.allow_request(Instant::now()) {
            return Response::error(429, "rate_limited");
        }
        if raw.len() > MAX_HEADERS {
            return Response::error(431, "headers_too_large");
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        let consumed = match request.parse(raw) {
            Ok(httparse::Status::Complete(size)) => size,
            _ => return Response::error(400, "invalid_request"),
        };
        // Reject ambiguous framing, bodies and pipelining, even on GET.
        if consumed != raw.len()
            || request.headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("transfer-encoding")
                    || h.name.eq_ignore_ascii_case("content-length")
            })
        {
            return Response::error(400, "invalid_framing");
        }
        let credentials = request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("authorization"))
            .collect::<Vec<_>>();
        let authorized = credentials.len() == 1
            && std::str::from_utf8(credentials[0].value)
                .ok()
                .and_then(|s| s.strip_prefix("Bearer "))
                .is_some_and(|s| self.auth.accepts(s));
        if !authorized {
            return Response::error(401, "unauthorized");
        }
        if request.method != Some("GET") {
            return Response::error(405, "get_required");
        }
        let target = request.path.unwrap_or_default();
        if target.len() > MAX_TARGET {
            return Response::error(414, "target_too_long");
        }
        if !target.starts_with('/') || target.starts_with("//") || target.contains('#') {
            return Response::error(400, "invalid_target");
        }
        let (route, query) = target.split_once('?').unwrap_or((target, ""));
        let params: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
        if params
            .iter()
            .enumerate()
            .any(|(i, (key, _))| params[..i].iter().any(|(other, _)| key == other))
        {
            return Response::error(400, "duplicate_parameter");
        }
        let get = |key: &str| {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_ref())
        };
        let versioned = route.starts_with("/v1/");
        if !versioned && !self.compatibility {
            return Response::error(404, "not_found");
        }
        let route = if versioned { &route[3..] } else { route };
        if route == "/health" && versioned {
            return Response::ok(json!({"status": "ok", "api_version": 1}));
        }
        if matches!(
            route,
            "/health" | "/sessions" | "/tracearr-health" | "/watchdog-jev"
        ) {
            return Response::error(503, "integration_unavailable");
        }
        let operation = match route {
            "/plexfails" => {
                if params.iter().any(|(key, _)| key != "minutes") {
                    return Response::error(400, "unknown_parameter");
                }
                let minutes = match get("minutes").unwrap_or("60").parse::<u32>() {
                    Ok(value @ 1..=1440) => value,
                    _ => return Response::error(400, "invalid_minutes"),
                };
                Operation::PlexFailures {
                    minutes,
                    compatibility: !versioned,
                }
            }
            "/evidence" | "/probe" | "/decodecheck" | "/file" => {
                if params
                    .iter()
                    .any(|(key, _)| key != "path" && !(key == "decode" && route == "/evidence"))
                {
                    return Response::error(400, "unknown_parameter");
                }
                let Some(path) = get("path")
                    .filter(|p| p.starts_with('/') && !p.contains('\0') && p.len() <= 3072)
                else {
                    return Response::error(400, "invalid_path");
                };
                let path = path.to_owned();
                match route {
                    "/evidence" => {
                        let decode = match get("decode").unwrap_or("false") {
                            "true" => true,
                            "false" => false,
                            _ => return Response::error(400, "invalid_decode"),
                        };
                        Operation::Evidence { path, decode }
                    }
                    "/probe" => Operation::Probe { path },
                    "/decodecheck" => Operation::Decode { path },
                    _ => Operation::File { path },
                }
            }
            _ => return Response::error(404, "not_found"),
        };
        match self.inspector.inspect(operation).await {
            Ok(value) => Response::ok(value),
            Err(error) => match error {
                ApiError::BadRequest => Response::error(400, "invalid_request"),
                ApiError::Forbidden => Response::error(403, "path_forbidden"),
                ApiError::Busy => Response::error(503, "busy"),
                ApiError::Unavailable => Response::error(503, "unavailable"),
            },
        }
    }
}

/// A fixed connection permit remains held until this returns. The header read,
/// complete operation and response write each have a deadline. No user input is logged.
pub async fn serve_connection<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    app: &App,
    header_timeout: Duration,
    operation_timeout: Duration,
) -> std::io::Result<()> {
    let response = match tokio::time::timeout(header_timeout, read_headers(stream)).await {
        Ok(Ok(raw)) => match tokio::time::timeout(operation_timeout, app.handle(&raw)).await {
            Ok(response) => response,
            Err(_) => Response::error(504, "request_timeout"),
        },
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidData => {
            Response::error(431, "headers_too_large")
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => Response::error(408, "header_timeout"),
    };
    let mut body = serde_json::to_vec(&response.body)?;
    let status = if body.len() > MAX_RESPONSE {
        body = br#"{"error":"response_too_large"}"#.to_vec();
        503
    } else {
        response.status
    };
    let header = format!(
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    tokio::time::timeout(header_timeout, async {
        stream.write_all(header.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

async fn read_headers<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        data.extend_from_slice(&buffer[..read]);
        if data.len() > MAX_HEADERS {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(data);
        }
    }
}

/// Call only with a listener explicitly created by the process. No outbound clients.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    max_connections: usize,
    operation_timeout: Duration,
) -> std::io::Result<()> {
    if !(1..=256).contains(&max_connections) || operation_timeout.is_zero() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let permits = Arc::new(Semaphore::new(max_connections));
    // Dropping the server aborts all request futures. Their media supervisors
    // observe closed result channels and kill/reap process groups.
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let app = app.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _ = serve_connection(&mut stream, &app, Duration::from_secs(5), operation_timeout).await;
                });
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Empty;
    impl Inspector for Empty {
        fn inspect(&self, _: Operation) -> InspectFuture<'_> {
            Box::pin(async { Ok(Value::Null) })
        }
    }
    #[test]
    fn rate_window_resets_at_exact_minute_and_counts_each_request() {
        let app = App::new(
            Auth::new(&"x".repeat(32)).unwrap(),
            false,
            Arc::new(Empty),
            2,
        )
        .unwrap();
        let start = app.window.lock().unwrap().start;
        assert!(app.allow_request(start));
        assert!(app.allow_request(start));
        assert!(!app.allow_request(start));
        assert!(!app.allow_request(start + Duration::from_secs(59)));
        assert!(app.allow_request(start + Duration::from_secs(60)));
        assert!(app.allow_request(start + Duration::from_secs(60)));
        assert!(!app.allow_request(start + Duration::from_secs(60)));
        assert!(!app.allow_request(start + Duration::from_secs(119)));
        assert!(app.allow_request(start + Duration::from_secs(120)));
    }
    #[tokio::test]
    async fn header_reader_accepts_limit_and_rejects_eof() {
        let mut data = vec![b'x'; MAX_HEADERS - 4];
        data.extend_from_slice(b"\r\n\r\n");
        assert_eq!(read_headers(&mut data.as_slice()).await.unwrap(), data);
        assert_eq!(
            read_headers(&mut b"incomplete".as_slice())
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
}
