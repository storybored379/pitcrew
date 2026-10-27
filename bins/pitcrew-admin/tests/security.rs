mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use common::*;
use pitcrew_admin::{
    Error,
    actions::{Command, Outcome},
};
use std::sync::atomic::Ordering;

#[tokio::test]
async fn owner_only_pages_reject_anonymous_viewer_cookies_and_fake_headers() {
    let s = Setup::new(true).await;
    for path in [
        "/queue",
        "/reports/1",
        "/jobs",
        "/jobs/1",
        "/exclusions",
        "/capabilities",
        "/deliveries",
        "/shadow",
        "/accounting",
        "/health",
        "/requests",
    ] {
        let (status, headers, body) = call(
            &s.app,
            "GET",
            path,
            "pitcrew_session=viewer; pitcrew_admin_session=forged",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert!(body.contains("Sign in with an allowed"));
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
    }
    let (status, _, _) = request(
        &s.app,
        Request::builder()
            .uri("/queue")
            .header("X-Admin-Account", "1001")
            .header("Authorization", "Bearer forged")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for path in ["/", "/style.css"] {
        assert_eq!(call(&s.app, "GET", path, "", "").await.0, StatusCode::OK);
    }
}
#[tokio::test]
async fn plex_allowlist_pending_failure_and_single_use_cookie_flow() {
    let s = Setup::new(true).await;
    let (_, headers, body) = call(&s.app, "POST", "/auth/start", "", "").await;
    assert!(body.contains("https://app.plex.tv/auth#?clientID=installation-a"));
    assert!(body.contains("rel=\"noopener noreferrer\""));
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .ends_with("; Secure")
    );
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("HttpOnly; SameSite=Strict; Max-Age=600")
    );
    let state = cookie(&headers, "pitcrew_admin_login");
    *s.auth.account.lock().unwrap() = None;
    assert!(
        call(&s.app, "POST", "/auth/poll", &state, "")
            .await
            .2
            .contains("Waiting for Plex")
    );
    *s.auth.account.lock().unwrap() = Some("1002".into());
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", &state, "").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_sessions").await,
        0
    );
    *s.auth.unavailable.lock().unwrap() = true;
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", &state, "").await.0,
        StatusCode::BAD_GATEWAY
    );
    *s.auth.unavailable.lock().unwrap() = false;
    *s.auth.account.lock().unwrap() = Some("1001".into());
    let (status, headers, _) = call(&s.app, "POST", "/auth/poll", &state, "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/queue");
    assert!(
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|v| v.to_str().unwrap().contains("Max-Age=28800; Secure"))
    );
    assert_eq!(
        call(&s.app, "POST", "/auth/poll", &state, "").await.0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn session_expiry_logout_allowlist_recheck_and_duplicate_cookies_fail_closed() {
    let s = Setup::new(true).await;
    let (cookie, csrf) = s.session().await;
    assert_eq!(
        call(&s.app, "GET", "/queue", &cookie, "").await.0,
        StatusCode::OK
    );
    let duplicate = format!("{cookie}; {cookie}");
    assert_eq!(
        call(&s.app, "GET", "/queue", &duplicate, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE admin_sessions SET account='2002'")
        .execute(&s.admin_sql)
        .await
        .unwrap();
    assert_eq!(
        call(&s.app, "GET", "/queue", &cookie, "").await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE admin_sessions SET account='1001'")
        .execute(&s.admin_sql)
        .await
        .unwrap();
    s.time.store(28_899, Ordering::SeqCst);
    assert_eq!(
        call(&s.app, "GET", "/queue", &cookie, "").await.0,
        StatusCode::OK
    );
    s.time.store(28_900, Ordering::SeqCst);
    assert_eq!(
        call(&s.app, "GET", "/queue", &cookie, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    s.time.store(100, Ordering::SeqCst);
    let (status, headers, _) = call(
        &s.app,
        "POST",
        "/logout",
        &cookie,
        &encoded(&[("csrf", &csrf)]),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        call(&s.app, "GET", "/queue", &cookie, "").await.0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn post_requires_exact_single_origin_and_session_bound_csrf_before_actions() {
    let s = Setup::new(true).await;
    s.report().await;
    let (cookie, csrf) = s.session().await;
    let command = Command::Close { report: 1 };
    for origin in [
        None,
        Some("https://attacker.example.invalid"),
        Some("null"),
        Some("https://admin.example.invalid/"),
    ] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/actions")
            .header(header::COOKIE, &cookie);
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        assert_eq!(
            request(
                &s.app,
                req.body(Body::from(action(&csrf, "a", &command, "reviewed")))
                    .unwrap()
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let req = Request::builder()
        .method("POST")
        .uri("/actions")
        .header(header::COOKIE, &cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::ORIGIN, ORIGIN)
        .body(Body::from(action(&csrf, "a", &command, "reviewed")))
        .unwrap();
    assert_eq!(request(&s.app, req).await.0, StatusCode::FORBIDDEN);
    for bad in ["", "wrong", &format!("{csrf}x")] {
        assert_eq!(
            call(
                &s.app,
                "POST",
                "/actions",
                &cookie,
                &action(bad, "a", &command, "reviewed")
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_requests").await,
        0
    );
}
#[tokio::test]
async fn untrusted_action_fields_duplicates_large_bodies_and_ready_templates_rejected() {
    let s = Setup::new(true).await;
    s.report().await;
    let (cookie, csrf) = s.session().await;
    let base = action(&csrf, "a", &Command::Close { report: 1 }, "reviewed");
    for body in [
        format!("{base}&actor=someone"),
        format!("{base}&role=Admin"),
        format!("{base}&note=second"),
        "x".repeat(8193),
    ] {
        assert_eq!(
            call(&s.app, "POST", "/actions", &cookie, &body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    for command in [
        r#"{"action":"close","report":1,"actor":"forged"}"#,
        r#"{"action":"message","report":1,"template":"ready"}"#,
        r#"{"action":"message","report":1,"template":"received","recipient":"someone"}"#,
    ] {
        assert_eq!(
            call(
                &s.app,
                "POST",
                "/actions",
                &cookie,
                &encoded(&[
                    ("csrf", &csrf),
                    ("request_key", "a"),
                    ("command", command),
                    ("note", "reviewed")
                ])
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_requests").await,
        0
    );
    assert!(s.api.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn audit_page_survives_first_pending_action_regression() {
    let s = Setup::new(true).await;
    s.report().await;
    let (cookie, csrf) = s.session().await;
    assert_eq!(
        call(
            &s.app,
            "POST",
            "/actions",
            &cookie,
            &action(
                &csrf,
                "request-a",
                &Command::Close { report: 1 },
                "Reviewed report"
            )
        )
        .await
        .0,
        StatusCode::SEE_OTHER
    );
    let (status, _, body) = call(&s.app, "GET", "/requests", &cookie, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("pending_controller"));
    assert!(body.contains("Reviewed report"));
    assert!(body.contains("1001"));
}
#[tokio::test]
async fn duplicate_api_requests_send_once_and_ambiguous_results_stay_audited() {
    let s = Setup::new(true).await;
    s.exclusion().await;
    let (cookie, csrf) = s.session().await;
    *s.api.outcome.lock().unwrap() = Outcome::Uncertain;
    let body = action(
        &csrf,
        "release-a",
        &Command::Release { id: 1 },
        "Checked evidence",
    );
    let (a, b) = tokio::join!(
        call(&s.app, "POST", "/actions", &cookie, &body),
        call(&s.app, "POST", "/actions", &cookie, &body)
    );
    assert_eq!(a.0, StatusCode::SEE_OTHER);
    assert_eq!(b.0, StatusCode::SEE_OTHER);
    assert_eq!(s.api.calls.lock().unwrap().len(), 1);
    assert_eq!(
        s.app.journal.requests().await.unwrap()[0]["state"],
        "uncertain"
    );
    assert_eq!(
        call(&s.app, "POST", "/actions", &cookie, &body).await.0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(s.api.calls.lock().unwrap().len(), 1);
    assert_eq!(
        call(
            &s.app,
            "POST",
            "/actions",
            &cookie,
            &action(&csrf, "release-a", &Command::Release { id: 1 }, "different")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
#[tokio::test]
async fn stale_requests_unknown_records_and_unconfigured_api_never_dispatch() {
    let s = Setup::new(false).await;
    s.report().await;
    s.seams().await;
    s.job(1).await;
    s.exclusion().await;
    s.capabilities().await;
    let (cookie, csrf) = s.session().await;
    for command in [
        Command::Approve {
            report: 1,
            decided_at: 94,
        },
        Command::Deny {
            report: 1,
            decided_at: 96,
        },
        Command::LowerQuality {
            job: 1,
            revision: 0,
        },
        Command::Release { id: 9 },
        Command::Resend {
            job: 1,
            sequence: 1,
        },
    ] {
        assert_eq!(
            call(
                &s.app,
                "POST",
                "/actions",
                &cookie,
                &action(&csrf, "x", &command, "reviewed")
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    assert_eq!(
        call(
            &s.app,
            "POST",
            "/actions",
            &cookie,
            &action(&csrf, "x", &Command::Close { report: 9 }, "reviewed")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &s.app,
            "POST",
            "/actions",
            &cookie,
            &action(&csrf, "x", &Command::Release { id: 1 }, "reviewed")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        scalar(&s.admin_sql, "SELECT COUNT(*) FROM admin_requests").await,
        0
    );
    assert!(matches!(
        s.app.journal.pin("missing", 100).await,
        Err(Error::Unauthorized)
    ));
}
