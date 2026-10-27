#![allow(dead_code)]
use axum::{
    body::{Body, to_bytes},
    http::{
        Request, StatusCode,
        header::{self, HeaderMap},
    },
};
use pitcrew_admin::{
    actions::{AdminApi, ApiFuture, Command, Outcome},
    auth::{Auth, AuthFuture, LoginPin},
    config::Config,
    journal::Journal,
    read::Sources,
    web::App,
};
use pitcrew_domain::*;
use pitcrew_store::{BudgetLimits, NewJob, Store};
use serde_json::{Value, json};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tower::ServiceExt;

pub const ORIGIN: &str = "https://admin.example.invalid";
pub struct FakeAuth {
    pub account: Mutex<Option<String>>,
    pub unavailable: Mutex<bool>,
}
impl Auth for FakeAuth {
    fn start(&self) -> AuthFuture<'_, LoginPin> {
        Box::pin(async move {
            if *self.unavailable.lock().unwrap() {
                return Err(pitcrew_admin::Error::Upstream);
            }
            Ok(LoginPin {
                id: 42,
                code: "SYNTHETIC-PIN".into(),
            })
        })
    }
    fn poll(&self, pin: u64) -> AuthFuture<'_, Option<String>> {
        Box::pin(async move {
            assert_eq!(pin, 42);
            if *self.unavailable.lock().unwrap() {
                return Err(pitcrew_admin::Error::Upstream);
            }
            Ok(self.account.lock().unwrap().clone())
        })
    }
}
pub type ApiCall = (Command, String, String);
pub struct FakeApi {
    pub calls: Mutex<Vec<ApiCall>>,
    pub outcome: Mutex<Outcome>,
}
impl AdminApi for FakeApi {
    fn submit<'a>(&'a self, command: &'a Command, key: &'a str, note: &'a str) -> ApiFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((command.clone(), key.into(), note.into()));
            *self.outcome.lock().unwrap()
        })
    }
}
pub fn config(dir: &std::path::Path) -> Config {
    Config {
        database: dir.join("admin.sqlite"),
        engine_database: dir.join("engine.sqlite"),
        intake_database: dir.join("intake.sqlite"),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_origin: ORIGIN.into(),
        plex_auth_url: "https://plex.example.invalid/".into(),
        client_id: "installation-a".into(),
        admin_account_ids: vec!["1001".into()],
        api: None,
        rates: Default::default(),
    }
}
pub fn spec(id: u64) -> NewJob {
    NewJob {
        id,
        viewer: "viewer-a".into(),
        resource: format!("media-{id}"),
        library: 7,
        original: FileIdentity { id: 4, revision: 1 },
        constraints: Constraints::default(),
        policy: Policy {
            max_grabs: 2,
            stage_timeout: 30,
            total_timeout: 300,
        },
        timeouts: StageTimeouts {
            download: 60,
            import: 40,
            decode: 50,
            verify_retry: 20,
        },
        created_at: 0,
    }
}
pub async fn pool(path: &std::path::Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}
pub struct Setup {
    pub dir: tempfile::TempDir,
    pub app: Arc<App>,
    pub auth: Arc<FakeAuth>,
    pub api: Arc<FakeApi>,
    pub time: Arc<AtomicU64>,
    pub engine: Store,
    pub engine_sql: SqlitePool,
    pub intake_sql: SqlitePool,
    pub admin_sql: SqlitePool,
}
impl Setup {
    pub async fn new(api_enabled: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let engine = Store::open(&config.engine_database).await.unwrap();
        pitcrew_intake_store::Store::open(&config.intake_database)
            .await
            .unwrap();
        let sources = Sources::open(&config.engine_database, &config.intake_database)
            .await
            .unwrap();
        let journal = Journal::open(&config.database).await.unwrap();
        let engine_sql = pool(&config.engine_database).await;
        let intake_sql = pool(&config.intake_database).await;
        let admin_sql = pool(&config.database).await;
        let auth = Arc::new(FakeAuth {
            account: Mutex::new(Some("1001".into())),
            unavailable: Mutex::new(false),
        });
        let api = Arc::new(FakeApi {
            calls: Mutex::new(Vec::new()),
            outcome: Mutex::new(Outcome::Accepted),
        });
        let time = Arc::new(AtomicU64::new(100));
        let t = time.clone();
        let app = Arc::new(
            App::new(
                journal,
                sources,
                auth.clone(),
                if api_enabled { Some(api.clone()) } else { None },
                config,
                Arc::new(move || t.load(Ordering::SeqCst)),
            )
            .unwrap(),
        );
        Self {
            dir,
            app,
            auth,
            api,
            time,
            engine,
            engine_sql,
            intake_sql,
            admin_sql,
        }
    }
    pub async fn session(&self) -> (String, String) {
        let (status, headers, body) = call(&self.app, "POST", "/auth/start", "", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("SYNTHETIC-PIN"));
        let login = cookie(&headers, "pitcrew_admin_login");
        let (status, headers, _) = call(&self.app, "POST", "/auth/poll", &login, "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let cookie = cookie(&headers, "pitcrew_admin_session");
        let token = cookie.split_once('=').unwrap().1;
        let session = self.app.journal.session(token, 100).await.unwrap();
        (cookie, session.csrf)
    }
    pub async fn report(&self) {
        sqlx::query("INSERT INTO web_intakes(channel,message,account,conversation,response,created) VALUES('web','message-a','1001','conversation-a',?,90)")
            .bind(json!({"outcome":"report","issues":[{"item_id":"episode-a","kinds":["playback"]}],"details":"<script>alert('x')</script> & \" ' {{CONTENT}}"}).to_string()).execute(&self.intake_sql).await.unwrap();
    }
    pub async fn seams(&self) {
        // The intake store already created `diagnoses` and `diagnosis_comparisons`;
        // the bridge owns `report_progress`, created here with its columns.
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS report_progress(report_id INTEGER PRIMARY KEY,job_id INTEGER,state TEXT,updated_at INTEGER,viewer_copy_key TEXT,job_revision INTEGER NOT NULL DEFAULT 0);").execute(&self.intake_sql).await.unwrap();
        sqlx::query("INSERT INTO diagnoses(report_id,decided_at,decider_kind,decision,evidence_summary,resource,library_id,arr_instance_id,arr_item_id,arr_file_id,agent_file,agent_observed_at,access_observed_at,access_expires_at) VALUES(1,95,'rules',?,?,'series-a:episode-a','1','series',1,1,NULL,NULL,90,200)")
            .bind(json!({"decision":{"action":"replace"}}).to_string())
            .bind(json!({"decode":"failed"}).to_string())
            .execute(&self.intake_sql)
            .await
            .unwrap();
        sqlx::query("INSERT INTO diagnosis_comparisons(report_id,comparison) VALUES(1,?)")
            .bind(json!({"primary":"rules","entries":[]}).to_string())
            .execute(&self.intake_sql)
            .await
            .unwrap();
    }
    pub async fn job(&self, id: u64) {
        self.engine
            .create_job(
                &spec(id),
                &format!("job-{id}"),
                BudgetLimits {
                    global_hour: 1000,
                    global_day: 1000,
                    viewer_hour: 1000,
                    viewer_day: 1000,
                },
            )
            .await
            .unwrap();
    }
    pub async fn exclusion(&self) {
        use pitcrew_store::policy::*;
        self.engine
            .exclude_release(
                &NewExclusion {
                    client: "arr-a".into(),
                    release: 17,
                    identity: Default::default(),
                    kind: ExclusionKind::DeadRelease,
                    reason: "Decode failed".into(),
                    evidence: json!({"decode":"bad"}),
                    pinned: false,
                    created_at: 10,
                },
                "exclusion-a",
            )
            .await
            .unwrap();
    }
    pub async fn capabilities(&self) {
        self.engine
            .observe_client_version("arr-a", "1.0", 100)
            .await
            .unwrap();
    }
}
pub fn cookie(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
pub async fn call(
    app: &Arc<App>,
    method: &str,
    path: &str,
    cookie: &str,
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    request(
        app,
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, cookie)
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
}
pub async fn request(app: &Arc<App>, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().router().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}
pub fn encoded(fields: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in fields {
        serializer.append_pair(k, v);
    }
    serializer.finish()
}
pub fn action(csrf: &str, key: &str, command: &Command, note: &str) -> String {
    encoded(&[
        ("csrf", csrf),
        ("request_key", key),
        ("command", &serde_json::to_string(command).unwrap()),
        ("note", note),
    ])
}
pub fn get_hidden(body: &str, name: &str) -> String {
    body.split_once(&format!("name=\"{name}\" value=\""))
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap()
        .into()
}
pub async fn scalar(pool: &SqlitePool, query: &str) -> i64 {
    sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
}
pub fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens":input,"output_tokens":output,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"estimated_cost_microusd":999})
}
