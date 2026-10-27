mod common;
use axum::{Router, body::Body, http::Request, response::IntoResponse};
use common::*;
use pitcrew_admin::{actions::Command, journal::Journal};
use serde_json::json;
use std::{
    process::{Child, Command as Process, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn plex(request: Request<Body>) -> impl IntoResponse {
    let value = match request.uri().path() {
        "/api/v2/pins" => json!({"id":42,"code":"SYNTHETIC"}),
        "/api/v2/pins/42" => json!({"authToken":"synthetic-token"}),
        "/api/v2/user" => json!({"id":1001}),
        _ => panic!("unexpected fake Plex request"),
    };
    axum::Json(value)
}
#[tokio::test]
async fn actual_binary_recovers_uncertain_intents_and_completes_owner_login_without_media_calls() {
    let s = Setup::new(false).await;
    s.report().await;
    s.app
        .journal
        .request(
            "1001",
            "interrupted",
            &Command::Reset {
                client: "arr-a".into(),
            },
            "Interrupted before acknowledgment",
            1,
        )
        .await
        .unwrap();
    let plex_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plex_url = format!("http://{}/", plex_listener.local_addr().unwrap());
    let plex_task = tokio::spawn(async move {
        axum::serve(plex_listener, Router::new().fallback(plex))
            .await
            .unwrap()
    });
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = reservation.local_addr().unwrap();
    drop(reservation);
    let origin = format!("http://{bind}");
    let path = s.dir.path().join("runtime.toml");
    let quoted =
        |path: std::path::PathBuf| toml::Value::String(path.to_str().unwrap().into()).to_string();
    std::fs::write(&path,format!("[admin]\ndatabase={}\nengine_database={}\nintake_database={}\nbind=\"{bind}\"\npublic_origin=\"{origin}\"\nplex_auth_url=\"{plex_url}\"\nclient_id=\"synthetic\"\nadmin_account_ids=[\"1001\"]\n",quoted(s.dir.path().join("admin.sqlite")),quoted(s.dir.path().join("engine.sqlite")),quoted(s.dir.path().join("intake.sqlite")))).unwrap();
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut child = ChildGuard(
        Process::new(env!("CARGO_BIN_EXE_pitcrew-admin"))
            .args(["--config", path.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut ready = false;
    for _ in 0..100 {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "admin binary exited during startup"
        );
        if let Ok(response) = http.get(&origin).send().await {
            assert_eq!(response.status(), 200);
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(ready, "admin binary did not start");
    let recovered = s.app.journal.requests().await.unwrap();
    assert_eq!(recovered[0]["state"], "uncertain");
    assert!(recovered[0]["updated_at"].as_u64().unwrap() >= before);
    let response = http.get(format!("{origin}/queue")).send().await.unwrap();
    assert_eq!(response.status(), 401);
    let response = http
        .post(format!("{origin}/auth/start"))
        .header("origin", &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let login = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(
        !response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
    let response = http
        .post(format!("{origin}/auth/poll"))
        .header("origin", &origin)
        .header("cookie", login)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let session = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with("pitcrew_admin_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let journal = Journal::open(&s.dir.path().join("admin.sqlite"))
        .await
        .unwrap();
    let credentials = journal
        .session(session.split_once('=').unwrap().1, before)
        .await
        .unwrap();
    assert_eq!(credentials.account, "1001");
    let response = http
        .get(format!("{origin}/queue"))
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("Report 1"));
    let response = http
        .post(format!("{origin}/logout"))
        .header("origin", &origin)
        .header("cookie", &session)
        .body(encoded(&[("csrf", &credentials.csrf)]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    assert_eq!(
        http.get(format!("{origin}/queue"))
            .header("cookie", session)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(scalar(&s.engine_sql, "SELECT COUNT(*) FROM jobs").await, 0);
    plex_task.abort();
}
