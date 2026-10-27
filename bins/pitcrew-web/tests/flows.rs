use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use pitcrew_intake_store::Store;
use pitcrew_web::{
    App, Config,
    auth::{Account, Auth, AuthError, AuthFuture, LoginPin},
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tower::ServiceExt;
struct FakeAuth {
    access: Mutex<Result<Vec<String>, AuthError>>,
    pending: Mutex<bool>,
}
impl Auth for FakeAuth {
    fn create_pin(&self) -> AuthFuture<'_, LoginPin> {
        Box::pin(async {
            Ok(LoginPin {
                id: 42,
                code: "PIN-CODE".into(),
            })
        })
    }
    fn poll(&self, id: u64) -> AuthFuture<'_, Option<Account>> {
        Box::pin(async move {
            assert_eq!(id, 42);
            if *self.pending.lock().unwrap() {
                Ok(None)
            } else {
                Ok(Some(Account {
                    id: "viewer".into(),
                    token: "test-token".into(),
                }))
            }
        })
    }
    fn access<'a>(&'a self, token: &'a str) -> AuthFuture<'a, Vec<String>> {
        Box::pin(async move {
            assert_eq!(token, "test-token");
            self.access.lock().unwrap().clone()
        })
    }
}
fn config() -> serde_json::Value {
    json!({"database":"unused.sqlite","public_origin":"https://crew.example","plex_server_url":"https://media.example/","plex_server_id":"server-id","client_id":"installation-id","catalog":[
    {"id":"film","title":"Copper Moon <script> \" ' &","library_id":"hd","library_name":"Standard","resolution":"hd","season":null,"episode":null},
    {"id":"episode","title":"Signal Garden","library_id":"hd","library_name":"Standard","resolution":"hd","season":2,"episode":3},
    {"id":"secret","title":"Hidden Movie","library_id":"uhd","library_name":"Ultra HD","resolution":"uhd","season":null,"episode":null}]})
}
struct Setup {
    _dir: tempfile::TempDir,
    app: Arc<App>,
    auth: Arc<FakeAuth>,
    time: Arc<AtomicU64>,
}
async fn setup() -> Setup {
    setup_config(config()).await
}
async fn setup_config(config_value: serde_json::Value) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("web.sqlite")).await.unwrap();
    let auth = Arc::new(FakeAuth {
        access: Mutex::new(Ok(vec!["hd".into()])),
        pending: Mutex::new(false),
    });
    let time = Arc::new(AtomicU64::new(100));
    let t = time.clone();
    let app = Arc::new(App::new(
        store,
        auth.clone(),
        Config::parse(&config_value.to_string()).unwrap(),
        Arc::new(move || t.load(Ordering::SeqCst)),
    ));
    Setup {
        _dir: dir,
        app,
        auth,
        time,
    }
}
async fn call(
    app: &Arc<App>,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> (StatusCode, header::HeaderMap, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, "https://crew.example")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().router().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let body = String::from_utf8(to_bytes(body, 2_000_000).await.unwrap().to_vec()).unwrap();
    assert_eq!(parts.headers["cache-control"], "no-store");
    assert_eq!(parts.headers["x-content-type-options"], "nosniff");
    assert_eq!(parts.headers["referrer-policy"], "no-referrer");
    assert!(
        parts.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    (parts.status, parts.headers, body)
}
fn cookie(headers: &header::HeaderMap, name: &str) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|s| s.to_str().ok())
        .find(|s| s.starts_with(name))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
async fn login(s: &Setup) -> String {
    let (status, headers, body) = call(&s.app, "POST", "/auth/start", "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("https://app.plex.tv/auth#?clientID=installation-id&amp;code=PIN-CODE"));
    let login = cookie(&headers, "pitcrew_login");
    let (status, headers, _) = call(&s.app, "POST", "/auth/poll", &login, "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/report");
    assert!(headers.get_all(header::SET_COOKIE).iter().all(|v| {
        let v = v.to_str().unwrap();
        v.contains("Secure") && v.contains("HttpOnly") && v.contains("SameSite=Lax")
    }));
    let session = cookie(&headers, "pitcrew_session");
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", &login, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    session
}
fn hidden(html: &str, name: &str) -> String {
    let prefix = format!("name=\"{name}\" value=\"");
    html.split(&prefix)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .into()
}
fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().copied())
        .finish()
}
#[tokio::test]
async fn complete_browser_flow_is_scoped_escaped_durable_and_never_claims_ready() {
    let s = setup().await;
    let (status, _, html) = call(&s.app, "GET", "/", "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Sign in with Plex"));
    assert!(html.contains("lang=\"en\""));
    assert_eq!(
        call(&s.app, "GET", "/report", "", "").await.0,
        StatusCode::UNAUTHORIZED
    );
    let session = login(&s).await;
    let (_, _, html) = call(&s.app, "GET", "/report", &session, "").await;
    assert!(html.contains("Copper Moon &lt;script&gt; &quot; &#39; &amp;"));
    assert!(!html.contains("<script>"));
    assert!(!html.contains("Hidden Movie"));
    assert!(html.contains("Signal Garden · S2 E3"));
    let (_, _, html) = call(&s.app, "GET", "/report?q=signal", &session, "").await;
    assert!(!html.contains("Copper Moon"));
    assert!(html.contains("Signal Garden"));
    assert!(
        call(&s.app, "GET", "/report?q=missing", &session, "")
            .await
            .2
            .contains("No matching titles")
    );
    assert_eq!(
        call(&s.app, "GET", "/report?item=secret", &session, "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (_, _, html) = call(
        &s.app,
        "GET",
        "/report?item=film&issue=playback",
        &session,
        "",
    )
    .await;
    let csrf = hidden(&html, "csrf");
    let nonce = hidden(&html, "nonce");
    assert_eq!(nonce.len(), 32);
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &nonce),
        ("item", "film"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
        ("text", "stops at the beginning"),
    ]);
    for attempt in 0..2 {
        let (status, _, html) = call(&s.app, "POST", "/report", &session, &body).await;
        assert_eq!(status, StatusCode::OK);
        if attempt == 0 {
            assert!(html.contains("Report received"));
            assert!(html.contains("Follow any checks or repairs in My reports."));
        } else {
            assert!(html.contains("Report already received"));
        }
    }
    assert_eq!(s.app.store.reports("viewer").await.unwrap().len(), 1);
    let (status, _, html) = call(&s.app, "GET", "/my-reports", &session, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Received · awaiting review"));
    assert!(!html.contains("Hidden Movie"));
    let row = s.app.store.reports("viewer").await.unwrap().remove(0);
    assert_eq!(row.response["issues"][0]["item_id"], "film");
    assert_eq!(row.response["issues"][0]["kinds"], json!(["playback"]));
    assert_eq!(row.created_at, 100);
    let body = form(&[("csrf", &csrf)]);
    let (status, _, html) = call(&s.app, "POST", "/link", &session, &body).await;
    assert_eq!(status, StatusCode::OK);
    let code = html
        .split("class=\"code\">")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap();
    assert_eq!(
        s.app
            .store
            .redeem_link(code, "test", "chat-user", 100)
            .await
            .unwrap(),
        "viewer"
    );
    assert!(
        call(&s.app, "GET", "/link", &session, "")
            .await
            .2
            .contains("Create a link code")
    );
    let (status, headers, _) = call(&s.app, "POST", "/logout", &session, &body).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        call(&s.app, "GET", "/my-reports", &session, "").await.0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn csrf_origin_revocation_expiry_and_invalid_input_fail_closed() {
    let s = setup().await;
    let session = login(&s).await;
    let (_, _, html) = call(
        &s.app,
        "GET",
        "/report?item=film&issue=playback",
        &session,
        "",
    )
    .await;
    let csrf = hidden(&html, "csrf");
    let nonce = hidden(&html, "nonce");
    for body in ["", "csrf=wrong"] {
        assert_eq!(
            call(&s.app, "POST", "/link", &session, body).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let request = Request::builder()
        .method("POST")
        .uri("/auth/start")
        .header(header::ORIGIN, "https://evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        s.app
            .clone()
            .router()
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let request = Request::builder()
        .method("POST")
        .uri("/auth/start")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        s.app
            .clone()
            .router()
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    *s.auth.access.lock().unwrap() = Err(AuthError::Unavailable);
    let body = form(&[("csrf", &csrf)]);
    assert_eq!(
        call(&s.app, "POST", "/link", &session, &body).await.0,
        StatusCode::BAD_GATEWAY
    );
    *s.auth.access.lock().unwrap() = Err(AuthError::Denied);
    assert_eq!(
        call(&s.app, "GET", "/report", &session, "").await.0,
        StatusCode::FORBIDDEN
    );
    *s.auth.access.lock().unwrap() = Ok(vec!["uhd".into()]);
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &nonce),
        ("item", "film"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(s.app.store.reports("viewer").await.unwrap().is_empty());
    *s.auth.access.lock().unwrap() = Ok(vec!["hd".into()]);
    for extras in [
        vec![],
        vec![("nonce", "")],
        vec![("nonce", &nonce), ("item", "film"), ("issue", "unknown")],
    ] {
        let mut fields = vec![("csrf", csrf.as_str())];
        fields.extend(extras);
        assert_eq!(
            call(&s.app, "POST", "/report", &session, &form(&fields))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let text = "a".repeat(2001);
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &nonce),
        ("item", "film"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
        ("text", &text),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::BAD_REQUEST
    );
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &"n".repeat(129)),
        ("item", "film"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::BAD_REQUEST
    );
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &"n".repeat(128)),
        ("item", "film"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
        ("text", &"a".repeat(2000)),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&s.app, "POST", "/link", &session, &"a".repeat(8193))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    s.time.store(604900, Ordering::SeqCst);
    assert_eq!(
        call(&s.app, "GET", "/report", &session, "").await.0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn pending_login_boundaries_and_public_response_paths() {
    let s = setup().await;
    *s.auth.pending.lock().unwrap() = true;
    let (_, headers, _) = call(&s.app, "POST", "/auth/start", "", "").await;
    let login = cookie(&headers, "pitcrew_login");
    assert!(
        call(&s.app, "POST", "/auth/poll", &login, "")
            .await
            .2
            .contains("Waiting for Plex")
    );
    s.time.store(700, Ordering::SeqCst);
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", &login, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", "", "").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&s.app, "DELETE", "/report", "", "").await.0,
        StatusCode::NOT_FOUND
    );
    let (status, headers, css) = call(&s.app, "GET", "/style.css", "", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    assert!(css.contains(":focus-visible"));
    for _ in 0..10 {
        assert_eq!(
            call(&s.app, "POST", "/auth/start", "", "").await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(&s.app, "POST", "/auth/start", "", "").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Expired challenges still consume the bounded upstream-poll budget.
    for _ in 0..98 {
        assert_eq!(
            call(&s.app, "POST", "/auth/poll", "", "").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", "", "").await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}
#[test]
fn configuration_defaults_and_invalid_origins_catalogs_are_rejected() {
    let parsed = Config::parse(&config().to_string()).unwrap();
    assert_eq!(parsed.bind.to_string(), "127.0.0.1:7401");
    assert_eq!(parsed.plex_auth_url, "https://plex.tv/");
    for origin in [
        "https://crew.example/",
        "http://crew.example",
        "https://user:noreply@crew.example",
        "bad",
        "https://crew.example/path",
        "https://crew.example?q=1",
        "https://crew.example#frag",
    ] {
        let mut c = config();
        c["public_origin"] = json!(origin);
        assert!(Config::parse(&c.to_string()).is_err(), "{origin}");
    }
    for origin in [
        "http://127.0.0.1:9000",
        "http://[::1]:9000",
        "http://localhost:9000",
        "https://crew.example",
    ] {
        let mut c = config();
        c["public_origin"] = json!(origin);
        assert!(Config::parse(&c.to_string()).is_ok(), "{origin}");
    }
    let mut c = config();
    c["catalog"][1]["id"] = json!("film");
    assert!(Config::parse(&c.to_string()).is_err());
    for field in ["id", "library_id"] {
        let mut c = config();
        c["catalog"][0][field] = json!("");
        assert!(Config::parse(&c.to_string()).is_err());
    }
    assert!(Config::parse("{}").is_err());
}
#[tokio::test]
async fn forged_movie_episode_report_is_rejected_and_duplicates_do_not_spend_quota() {
    let s = setup().await;
    let session = login(&s).await;
    let (_, _, html) = call(
        &s.app,
        "GET",
        "/report?item=film&issue=playback",
        &session,
        "",
    )
    .await;
    let csrf = hidden(&html, "csrf");
    let bad = form(&[
        ("csrf", &csrf),
        ("nonce", "wrong-episode"),
        ("item", "film"),
        ("issue", "wrong_episode"),
        ("answer", "not_sure"),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &bad).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(s.app.store.reports("viewer").await.unwrap().is_empty());
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", "same-submission"),
        ("item", "film"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
    ]);
    for _ in 0..6 {
        assert_eq!(
            call(&s.app, "POST", "/report", &session, &body).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(s.app.store.reports("viewer").await.unwrap().len(), 1);
    for index in 0..4 {
        let body = body.replace("same-submission", &format!("new-{index}"));
        assert_eq!(
            call(&s.app, "POST", "/report", &session, &body).await.0,
            StatusCode::OK
        );
    }
    let sixth = body.replace("same-submission", "sixth");
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &sixth).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(s.app.store.reports("viewer").await.unwrap().len(), 5);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::OK
    );
    s.time.store(86400, Ordering::SeqCst);
    let (first, retry) = tokio::join!(
        call(&s.app, "POST", "/report", &session, &sixth),
        call(&s.app, "POST", "/report", &session, &sixth)
    );
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(retry.0, StatusCode::OK);
    assert_eq!(s.app.store.reports("viewer").await.unwrap().len(), 6);
}
#[tokio::test]
async fn symptom_questions_are_validated_and_saved_with_exact_episode() {
    let s = setup().await;
    let session = login(&s).await;
    let (_, _, film) = call(&s.app, "GET", "/report?item=film", &session, "").await;
    assert!(!film.contains("value=\"wrong_episode\""));
    assert!(
        call(&s.app, "GET", "/report?item=episode", &session, "")
            .await
            .2
            .contains("Pick the problem")
    );
    let cases = [
        ("playback", "just_this_one", "other stuff"),
        ("buffering", "still_bad", "pausing"),
        ("subtitles", "english", "Which language"),
        ("audio_language", "spanish", "Which language"),
        ("no_audio", "checked", "device muted"),
        ("wrong_episode", "different_episode", "actually get"),
        ("quality", "whole_time", "whole way"),
    ];
    for (index, (issue, answer, question)) in cases.into_iter().enumerate() {
        s.time.store(100 + index as u64 * 86400, Ordering::SeqCst);
        // Each case signs in again so the final day does not cross session expiry.
        let (_, _, html) = call(
            &s.app,
            "GET",
            &format!("/report?item=episode&issue={issue}"),
            &session,
            "",
        )
        .await;
        assert!(html.contains(question));
        let csrf = hidden(&html, "csrf");
        let nonce = hidden(&html, "nonce");
        let bad = form(&[
            ("csrf", &csrf),
            ("nonce", &nonce),
            ("item", "episode"),
            ("issue", issue),
            ("answer", "forged"),
        ]);
        assert_eq!(
            call(&s.app, "POST", "/report", &session, &bad).await.0,
            StatusCode::BAD_REQUEST
        );
        let body = form(&[
            ("csrf", &csrf),
            ("nonce", &nonce),
            ("item", "episode"),
            ("issue", issue),
            ("answer", answer),
            ("text", "Synthetic details"),
        ]);
        assert_eq!(
            call(&s.app, "POST", "/report", &session, &body).await.0,
            StatusCode::OK
        );
        let row = s.app.store.reports("viewer").await.unwrap().remove(0);
        assert_eq!(
            row.response["wizard"],
            json!({"issue":issue,"answer":answer,"details":"Synthetic details","disposition":"diagnostic"})
        );
    }
    let (_, _, html) = call(&s.app, "GET", "/my-reports", &session, "").await;
    assert!(html.contains("S2 E3"));
    for symptom in [
        "Playback",
        "Buffering",
        "Subtitles",
        "Audio language",
        "No sound",
        "Wrong episode",
        "Picture quality",
    ] {
        assert!(html.contains(symptom));
    }
    assert_eq!(
        call(
            &s.app,
            "GET",
            "/report?item=film&issue=wrong_episode",
            &session,
            ""
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &s.app,
            "GET",
            "/report?item=film&issue=forged",
            &session,
            ""
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn advisory_ads_other_and_broken_video_keep_their_intent() {
    let s = setup().await;
    let session = login(&s).await;
    for (index, (issue, answer, details, disposition, copy, kinds)) in [
        (
            "ads",
            "check_source",
            "",
            "advice",
            "Open your shared library",
            json!([]),
        ),
        (
            "other",
            "describe",
            "The credits are missing",
            "review",
            "Your report is saved.",
            json!([]),
        ),
        (
            "video_broken",
            "confirmed",
            "",
            "diagnostic",
            "Report received",
            json!(["playback"]),
        ),
        (
            "buffering",
            "helped",
            "",
            "self_resolved",
            "Glad that helped",
            json!(["buffering"]),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        s.time.store(100 + index as u64 * 86400, Ordering::SeqCst);
        let (status, _, html) = call(
            &s.app,
            "GET",
            &format!("/report?item=film&issue={issue}"),
            &session,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{issue}");
        let body = form(&[
            ("csrf", &hidden(&html, "csrf")),
            ("nonce", &hidden(&html, "nonce")),
            ("item", "film"),
            ("issue", issue),
            ("answer", answer),
            ("text", details),
        ]);
        let (status, _, receipt) = call(&s.app, "POST", "/report", &session, &body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(receipt.contains(copy), "{receipt}");
        let row = s.app.store.reports("viewer").await.unwrap().remove(0);
        assert_eq!(row.response["wizard"]["issue"], issue);
        assert_eq!(row.response["wizard"]["disposition"], disposition);
        assert_eq!(row.response["issues"][0]["kinds"], kinds);
    }
    let (_, _, reports) = call(&s.app, "GET", "/my-reports", &session, "").await;
    for copy in [
        "Ads or unexpected version",
        "Something else",
        "Video looks broken",
        "Self-resolved",
        "Advice provided",
    ] {
        assert!(reports.contains(copy), "{copy}: {reports}");
    }
}

#[tokio::test]
async fn whole_show_requires_a_visible_exact_episode_before_reporting() {
    let s = setup().await;
    let session = login(&s).await;
    let (_, _, catalog) = call(&s.app, "GET", "/report", &session, "").await;
    assert!(catalog.contains("/report?show=episode"));
    assert!(!catalog.contains("/report?show=film"));
    let (status, _, html) = call(&s.app, "GET", "/report?show=episode", &session, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Which episode"));
    assert!(html.contains("/report?item=episode"));
    assert!(!html.contains("Send report"));
    assert_eq!(
        call(&s.app, "GET", "/report?show=film", &session, "")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&s.app, "GET", "/report?show=secret", &session, "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let csrf = hidden(&html, "csrf");
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", "whole-show"),
        ("show", "episode"),
        ("issue", "playback"),
        ("answer", "just_this_one"),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::BAD_REQUEST
    );
    assert!(s.app.store.reports("viewer").await.unwrap().is_empty());
}

fn sync_snapshot() -> serde_json::Value {
    let cfg = config();
    let mut records = serde_json::Map::new();
    for item in cfg["catalog"].as_array().unwrap() {
        records.insert(item["id"].as_str().unwrap().into(),json!({"item":item,"plex_rating_key":"101","series_id":"series-a","target":null,"observed_at":90,"expires_at":200}));
    }
    let mut second = records["episode"].clone();
    second["item"]["id"] = json!("second");
    second["item"]["episode"] = json!(2);
    records.insert("second".into(), second.clone());
    second["item"]["id"] = json!("remake");
    second["item"]["episode"] = json!(1);
    second["series_id"] = json!("series-remake");
    records.insert("remake".into(), second);
    let mut foreign = records["second"].clone();
    foreign["item"]["id"] = json!("other-library-episode");
    foreign["item"]["library_id"] = json!("uhd");
    records.insert("other-library-episode".into(), foreign);
    json!({"records":records,"activity":[
        {"account_id":"viewer","item_id":"episode","kind":"RecentPlay","at":99},
        {"account_id":"viewer","item_id":"secret","kind":"RecentPlay","at":100},
        {"account_id":"another-viewer","item_id":"film","kind":"RecentPlay","at":100},
        {"account_id":"viewer","item_id":"film","kind":"Watchlist","at":97},
        {"account_id":"viewer","item_id":"episode","kind":"Watchlist","at":98}
    ]})
}
fn section<'a>(html: &'a str, name: &str) -> &'a str {
    html.split(&format!("<section aria-label=\"{name}\">"))
        .nth(1)
        .unwrap()
        .split("</section>")
        .next()
        .unwrap()
}
#[tokio::test]
async fn synced_recent_watchlist_and_series_follow_current_access_and_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.json");
    let mut snapshot = sync_snapshot();
    std::fs::write(&path, snapshot.to_string()).unwrap();
    let mut cfg = config();
    cfg["catalog_snapshot_file"] = json!(path);
    // The configured fallback must never leak into an enabled synced picker.
    cfg["catalog"][0]["title"] = json!("Stale fallback");
    let s = setup_config(cfg).await;
    let session = login(&s).await;
    let (_, _, html) = call(&s.app, "GET", "/report", &session, "").await;
    assert!(!html.contains("/report?show=second"));
    assert!(html.contains("/report?show=remake"));
    let recent = section(&html, "Recently played");
    assert!(recent.contains("Signal Garden · S2 E3"));
    assert!(!recent.contains("Copper Moon"));
    assert!(!html.contains("Hidden Movie"));
    assert!(!html.contains("Stale fallback"));
    let watchlist = section(&html, "Your watchlist");
    assert!(watchlist.contains("Copper Moon &lt;script&gt;"));
    assert!(watchlist.contains("Signal Garden · Whole show"));
    assert!(watchlist.contains("/report?show=episode"));
    assert!(watchlist.contains("/report?item=film"));
    assert!(recent.contains("/report?item=episode"));
    *s.auth.access.lock().unwrap() = Ok(vec!["hd".into(), "uhd".into()]);
    let (_, _, show) = call(&s.app, "GET", "/report?show=episode", &session, "").await;
    *s.auth.access.lock().unwrap() = Ok(vec!["hd".into()]);
    assert!(!show.contains("/report?item=film"));
    assert!(!show.contains("/report?item=other-library-episode"));
    assert!(show.contains("/report?item=episode"));
    assert!(show.contains("/report?item=second"));
    assert!(!show.contains("/report?item=remake"));
    assert!(show.find("S2 E2").unwrap() < show.find("S2 E3").unwrap());
    let (_, _, remake) = call(&s.app, "GET", "/report?show=remake", &session, "").await;
    assert!(remake.contains("/report?item=remake"));
    assert!(!remake.contains("/report?item=episode"));
    assert!(
        !call(&s.app, "GET", "/report?q=signal", &session, "")
            .await
            .2
            .contains("Recently played")
    );
    // A sync export is reread on the next request without restarting the web app.
    snapshot["records"]["episode"]["item"]["title"] = json!("Updated Series");
    std::fs::write(&path, snapshot.to_string()).unwrap();
    assert!(
        call(&s.app, "GET", "/report", &session, "")
            .await
            .2
            .contains("Updated Series")
    );
    *s.auth.access.lock().unwrap() = Ok(vec!["uhd".into()]);
    let (_, _, html) = call(&s.app, "GET", "/report", &session, "").await;
    assert!(section(&html, "Recently played").contains("Hidden Movie"));
    assert!(!html.contains("Updated Series"));
    assert!(!html.contains("Copper Moon"));
    *s.auth.access.lock().unwrap() = Ok(vec!["hd".into()]);
    s.time.store(200, Ordering::SeqCst);
    let (_, _, html) = call(&s.app, "GET", "/report", &session, "").await;
    assert!(html.contains("No recent plays"));
    assert!(html.contains("No watchlist titles"));
    assert!(!html.contains("Updated Series"));
    assert_eq!(
        call(&s.app, "GET", "/report?item=film", &session, "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn missing_invalid_oversized_sync_export_fails_closed_and_bounds_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-catalog.json");
    let mut cfg = config();
    cfg["catalog_snapshot_file"] = json!(path);
    let s = setup_config(cfg).await;
    let session = login(&s).await;
    for contents in [
        None,
        Some("private-invalid-json"),
        Some(&(" ".repeat(16 * 1024 * 1024 - 13) + r#"{"records":{}}"#)),
    ] {
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        let (status, _, html) = call(&s.app, "GET", "/report", &session, "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(html.contains("catalog is temporarily unavailable"));
        assert!(!html.contains("Copper Moon"));
        assert!(!html.contains("private-"));
    }
    let suffix = r#"{"records":{}}"#;
    let data = " ".repeat(16 * 1024 * 1024 - suffix.len()) + suffix;
    std::fs::write(&path, data).unwrap();
    assert_eq!(
        call(&s.app, "GET", "/report", &session, "").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn other_requires_bounded_details_and_cannot_forge_diagnostic_disposition() {
    let s = setup().await;
    let session = login(&s).await;
    let (_, _, html) = call(&s.app, "GET", "/report?item=film&issue=other", &session, "").await;
    assert!(html.contains("maxlength=\"140\" required"));
    assert!(!html.contains("Optional</span>"));
    assert!(html.contains("Describe the problem <span"));
    let csrf = hidden(&html, "csrf");
    for details in ["".into(), " \n ".into(), "雨".repeat(141)] {
        let body = form(&[
            ("csrf", &csrf),
            ("nonce", "other"),
            ("item", "film"),
            ("issue", "other"),
            ("answer", "describe"),
            ("text", &details),
        ]);
        assert_eq!(
            call(&s.app, "POST", "/report", &session, &body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", "other"),
        ("item", "film"),
        ("issue", "other"),
        ("answer", "describe"),
        ("text", &"雨".repeat(140)),
        ("disposition", "diagnostic"),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::OK
    );
    let report = s.app.store.reports("viewer").await.unwrap().remove(0);
    assert_eq!(report.response["wizard"]["disposition"], "review");
    assert_eq!(report.response["wizard"]["details"], "雨".repeat(140));
    assert_eq!(report.response["issues"][0]["kinds"], json!([]));
    let (_, _, html) = call(
        &s.app,
        "GET",
        "/report?item=film&issue=buffering",
        &session,
        "",
    )
    .await;
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &hidden(&html, "nonce")),
        ("item", "film"),
        ("issue", "buffering"),
        ("answer", "not_tried"),
    ]);
    let (status, _, receipt) = call(&s.app, "POST", "/report", &session, &body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(receipt.contains("Try these playback checks"));
    assert_eq!(
        s.app
            .store
            .reports("viewer")
            .await
            .unwrap()
            .remove(0)
            .response["wizard"]["disposition"],
        "advice"
    );
    assert_eq!(
        call(
            &s.app,
            "GET",
            "/report?item=episode&show=episode",
            &session,
            ""
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn retry_cannot_claim_a_different_outcome_than_the_durable_report() {
    let s = setup().await;
    let session = login(&s).await;
    let (_, _, html) = call(
        &s.app,
        "GET",
        "/report?item=film&issue=buffering",
        &session,
        "",
    )
    .await;
    let csrf = hidden(&html, "csrf");
    let nonce = hidden(&html, "nonce");
    let body = form(&[
        ("csrf", &csrf),
        ("nonce", &nonce),
        ("item", "film"),
        ("issue", "buffering"),
        ("answer", "still_bad"),
    ]);
    assert_eq!(
        call(&s.app, "POST", "/report", &session, &body).await.0,
        StatusCode::OK
    );
    for answer in ["helped", "still_bad"] {
        let retry = body.replace("still_bad", answer);
        let (status, _, receipt) = call(&s.app, "POST", "/report", &session, &retry).await;
        assert_eq!(status, StatusCode::OK);
        assert!(receipt.contains("Report already received"), "{receipt}");
        assert!(!receipt.contains("self-resolved"));
        assert!(!receipt.contains("A repair has not been started"));
        let rows = s.app.store.reports("viewer").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].response["wizard"]["disposition"], "diagnostic");
        assert_eq!(rows[0].response["wizard"]["answer"], "still_bad");
    }
}

#[tokio::test]
async fn large_libraries_bound_search_and_keep_every_episode_reachable() {
    let mut cfg = config();
    let episode = cfg["catalog"][1].clone();
    cfg["catalog"] = json!(
        (0..201)
            .map(|index| {
                let mut item = episode.clone();
                item["id"] = json!(format!("entry-{index:03}"));
                item["season"] = json!(1);
                item["episode"] = json!(index + 1);
                item
            })
            .collect::<Vec<_>>()
    );
    let many = cfg["catalog"].as_array().unwrap().clone();
    let s = setup_config(cfg).await;
    let session = login(&s).await;
    let (_, _, search) = call(&s.app, "GET", "/report?q=Signal", &session, "").await;
    assert!(search.contains("Showing the first 100 matches"));
    assert!(search.contains("item=entry-099"));
    assert!(!search.contains("item=entry-100"));
    let (_, _, first) = call(&s.app, "GET", "/report?show=entry-000", &session, "").await;
    assert!(first.contains("item=entry-099"));
    assert!(!first.contains("item=entry-100"));
    assert!(first.contains("Next episodes"));
    assert!(!first.contains("Previous episodes"));
    assert!(first.contains("show=entry-000&amp;page=1"));
    let (_, _, middle) = call(&s.app, "GET", "/report?show=entry-000&page=1", &session, "").await;
    assert!(!middle.contains("item=entry-099"));
    assert!(middle.contains("item=entry-100"));
    assert!(middle.contains("item=entry-199"));
    assert!(!middle.contains("item=entry-200"));
    assert!(middle.contains("Previous episodes"));
    assert!(middle.contains("show=entry-000&amp;page=0"));
    assert!(middle.contains("Next episodes"));
    assert!(middle.contains("show=entry-000&amp;page=2"));
    let (_, _, last) = call(&s.app, "GET", "/report?show=entry-000&page=2", &session, "").await;
    assert!(last.contains("item=entry-200"));
    assert!(!last.contains("item=entry-199"));
    assert!(last.contains("Previous episodes"));
    assert!(!last.contains("Next episodes"));
    for page in ["3", "bad", "-1", "18446744073709551615"] {
        assert_eq!(
            call(
                &s.app,
                "GET",
                &format!("/report?show=entry-000&page={page}"),
                &session,
                ""
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    for count in [100, 200] {
        let mut cfg = config();
        cfg["catalog"] = json!(&many[..count]);
        let exact = setup_config(cfg).await;
        let session = login(&exact).await;
        let (_, _, search) = call(&exact.app, "GET", "/report?q=Signal", &session, "").await;
        assert_eq!(
            search.contains("Showing the first 100 matches"),
            count > 100
        );
        let page = count / 100 - 1;
        let (_, _, last) = call(
            &exact.app,
            "GET",
            &format!("/report?show=entry-000&page={page}"),
            &session,
            "",
        )
        .await;
        assert!(!last.contains("Next episodes"));
        assert_eq!(
            call(
                &exact.app,
                "GET",
                &format!("/report?show=entry-000&page={}", page + 1),
                &session,
                ""
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn my_reports_renders_saved_progress_without_leaking_or_inventing_status() {
    let s = setup().await;
    let session = login(&s).await;
    let response = json!({"outcome":"report","issues":[{"item_id":"film","kinds":["playback"]}]});
    s.app
        .store
        .persist_web_report("viewer", "progress", &response, 100)
        .await
        .unwrap();
    s.app
        .store
        .persist_web_report("other-viewer", "private", &response, 100)
        .await
        .unwrap();
    let id = s.app.store.reports("viewer").await.unwrap()[0].id;
    let other = s.app.store.reports("other-viewer").await.unwrap()[0].id;
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(s._dir.path().join("web.sqlite")),
    )
    .await
    .unwrap();
    sqlx::query("CREATE TABLE report_progress(report_id INTEGER PRIMARY KEY,job_id INTEGER,state TEXT,updated_at INTEGER,viewer_copy_key TEXT)").execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO report_progress VALUES(?,NULL,'could_not_fix',101,'report.could_not_fix')",
    )
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    for (state, copy) in [
        ("received", "Received · awaiting review"),
        (
            "needs_review",
            "Needs review · an admin needs to check this report",
        ),
        (
            "advice",
            "Advice available · no automatic repair was started",
        ),
        (
            "working",
            "Working on it · the repair has not been verified yet",
        ),
        ("fixed", "Fixed · the replacement passed playback checks"),
        (
            "could_not_fix",
            "Could not fix automatically · an admin needs to help",
        ),
        ("<script>unknown</script>", "Received · awaiting review"),
    ] {
        sqlx::query("INSERT INTO report_progress VALUES(?,7,?,101,'<script>untrusted copy</script>') ON CONFLICT(report_id) DO UPDATE SET state=excluded.state").bind(id).bind(state).execute(&pool).await.unwrap();
        let (status, _, html) = call(&s.app, "GET", "/my-reports", &session, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(copy), "missing {state}: {html}");
        assert!(!html.contains("<script>"));
        assert!(!html.contains(&format!("Report {other}</small>")));
        if state != "could_not_fix" {
            assert!(!html.contains("Could not fix automatically"));
        }
        if state != "fixed" {
            assert!(!html.contains("Fixed ·"));
        }
    }
    // An authenticated report no longer grants visibility after library revocation.
    *s.auth.access.lock().unwrap() = Ok(vec!["uhd".into()]);
    let (_, _, html) = call(&s.app, "GET", "/my-reports", &session, "").await;
    assert!(!html.contains("Copper Moon"));
    assert!(!html.contains(&format!("Report {id}</small>")));
}
