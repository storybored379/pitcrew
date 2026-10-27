use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::Response,
    routing::any,
};
use pitcrew_bot::{channels::*, runtime::*};
use pitcrew_domain::{channels::*, delivery::*};
use pitcrew_intake::{CatalogItem, Resolution};
use pitcrew_intake_store::Store;
use pitcrew_web::auth::{Account, Auth, AuthError, AuthFuture, LoginPin};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
type HttpCall = (String, axum::http::HeaderMap, Value);
#[derive(Clone)]
struct FakeHttp {
    status: StatusCode,
    body: String,
    seen: Arc<Mutex<Vec<HttpCall>>>,
}
async fn serve(State(fake): State<FakeHttp>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, 1_100_000).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    fake.seen
        .lock()
        .unwrap()
        .push((parts.uri.to_string(), parts.headers, value));
    Response::builder()
        .status(fake.status)
        .body(Body::from(fake.body))
        .unwrap()
}
async fn fake_http(status: u16, body: String) -> (String, FakeHttp, tokio::task::JoinHandle<()>) {
    let fake = FakeHttp {
        status: StatusCode::from_u16(status).unwrap(),
        body,
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new().fallback(any(serve)).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, fake, task)
}
#[test]
fn receipts_never_turn_ambiguous_delivery_into_success() {
    for (platform, body) in [
        (Platform::Discord, json!({"id":"7"})),
        (
            Platform::Telegram,
            json!({"ok":true,"result":{"message_id":7}}),
        ),
        (Platform::Seerr, json!({"id":7})),
    ] {
        for status in [200, 201, 299] {
            assert_eq!(
                delivery_receipt(platform, status, &body),
                DeliveryOutcome::Confirmed
            );
        }
        for status in [0, 199, 300, 399, 408, 500, 599] {
            assert_eq!(
                delivery_receipt(platform, status, &body),
                DeliveryOutcome::Uncertain
            );
        }
        for status in [400, 401, 403, 404, 409, 499] {
            assert_eq!(
                delivery_receipt(platform, status, &body),
                DeliveryOutcome::PermanentFailure
            );
        }
        assert_eq!(
            delivery_receipt(platform, 429, &body),
            DeliveryOutcome::NotAccepted
        );
        for body in [
            Value::Null,
            json!({}),
            json!({"id":0,"ok":false,"result":{"message_id":0}}),
            json!({"id":"0","ok":true,"result":{"message_id":0}}),
        ] {
            assert_eq!(
                delivery_receipt(platform, 200, &body),
                DeliveryOutcome::Uncertain
            );
        }
    }
    for body in [
        json!({"ok":false,"result":{"message_id":7}}),
        json!({"ok":true}),
        json!({"ok":true,"result":{"message_id":-1}}),
    ] {
        assert_eq!(
            delivery_receipt(Platform::Telegram, 200, &body),
            DeliveryOutcome::Uncertain
        );
    }
    assert_eq!(
        delivery_receipt(Platform::Discord, 200, &json!({"id":"abc"})),
        DeliveryOutcome::Uncertain
    );
}
#[tokio::test]
async fn real_http_transport_records_contracts_and_stops_redirects() {
    for (platform, path, header, expected, body) in [
        (
            Platform::Discord,
            "/channels/30/messages",
            "authorization",
            "Bot synthetic",
            json!({"id":"7"}),
        ),
        (
            Platform::Telegram,
            "/sendMessage",
            "authorization",
            "",
            json!({"ok":true,"result":{"message_id":7}}),
        ),
        (
            Platform::Seerr,
            "/api/v1/issue/30/comment",
            "x-api-key",
            "synthetic",
            json!({"id":7}),
        ),
    ] {
        let (url, fake, task) = fake_http(200, body.to_string()).await;
        let transport = HttpTransport::new(platform, &format!("{url}/"), "synthetic").unwrap();
        assert_eq!(
            transport
                .send(SendRequest {
                    path: path.into(),
                    body: json!({"text":"literal"})
                })
                .await,
            Ok(DeliveryOutcome::Confirmed)
        );
        assert_eq!(transport.get("/poll?skip=3").await.unwrap(), body);
        let calls = fake.seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, path);
        assert_eq!(calls[0].2, json!({"text":"literal"}));
        assert_eq!(calls[1].0, "/poll?skip=3");
        assert_eq!(
            calls[0]
                .1
                .get(header)
                .map(|h| h.to_str().unwrap())
                .unwrap_or(""),
            expected
        );
        task.abort();
    }
    for (status, body, want) in [
        (302, "{}".into(), DeliveryOutcome::Uncertain),
        (500, "{}".into(), DeliveryOutcome::Uncertain),
        (429, "bad".into(), DeliveryOutcome::NotAccepted),
        (200, "bad".into(), DeliveryOutcome::Uncertain),
        (200, "x".repeat(1_048_577), DeliveryOutcome::Uncertain),
    ] {
        let (url, _, task) = fake_http(status, body).await;
        let transport = HttpTransport::new(Platform::Seerr, &url, "synthetic").unwrap();
        assert_eq!(
            transport
                .send(SendRequest {
                    path: "/send".into(),
                    body: json!({})
                })
                .await,
            Ok(want)
        );
        assert!(transport.get("/poll").await.is_err());
        task.abort();
    }
    for url in [
        "invalid",
        "file:///tmp/fixture",
        "http://name:noreply@example.invalid",
        "http://example.invalid/?secret=x",
        "http://example.invalid/#fragment",
    ] {
        assert!(HttpTransport::new(Platform::Seerr, url, "token").is_err());
    }
    let (url, _, task) = fake_http(200, "{}".into()).await;
    task.abort();
    let _ = task.await;
    let transport = HttpTransport::new(Platform::Seerr, &url, "synthetic").unwrap();
    assert_eq!(
        transport
            .send(SendRequest {
                path: "/send".into(),
                body: json!({})
            })
            .await,
        Ok(DeliveryOutcome::Uncertain)
    );
    assert!(transport.get("/poll").await.is_err());
}
#[derive(Default)]
struct FakeAuth {
    access: Mutex<Option<Result<Vec<String>, AuthError>>>,
}
impl Auth for FakeAuth {
    fn create_pin(&self) -> AuthFuture<'_, LoginPin> {
        Box::pin(async { unreachable!() })
    }
    fn poll(&self, _: u64) -> AuthFuture<'_, Option<Account>> {
        Box::pin(async { unreachable!() })
    }
    fn access<'a>(&'a self, token: &'a str) -> AuthFuture<'a, Vec<String>> {
        assert_eq!(token, "synthetic-plex-token");
        Box::pin(async {
            self.access
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(vec!["films".into()]))
        })
    }
}
#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<SendRequest>>,
    outcome: Mutex<Option<DeliveryOutcome>>,
}
impl Transport for Recording {
    fn send(&self, request: SendRequest) -> ChannelFuture<'_, DeliveryOutcome> {
        self.calls.lock().unwrap().push(request);
        Box::pin(async {
            Ok(self
                .outcome
                .lock()
                .unwrap()
                .unwrap_or(DeliveryOutcome::Confirmed))
        })
    }
}
fn incoming(id: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        sender: ChannelIdentity {
            channel_id: "tg".into(),
            platform_user_id: "10".into(),
        },
        message_id: id.into(),
        conversation_id: "30".into(),
        text: text.into(),
        received_at: 10,
    }
}
fn catalog() -> Vec<CatalogItem> {
    vec![CatalogItem {
        id: "glass".into(),
        title: "The Glass Harbor".into(),
        aliases: vec![],
        library_id: "films".into(),
        library_name: "Films".into(),
        resolution: Resolution::Hd,
        season: None,
        episode: None,
    }]
}
#[tokio::test]
async fn host_links_fresh_auth_and_outbox_claims_before_a_single_send() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.sqlite")).await.unwrap();
    let auth = Arc::new(FakeAuth::default());
    let host = BotHost {
        store: store.clone(),
        catalog: catalog(),
        auth: auth.clone(),
    };
    let recording = Arc::new(Recording::default());
    let channel = PlatformChannel::new(Platform::Telegram, "tg".into(), recording.clone());
    let unlinked = host.accept(&incoming("unlinked", "broken")).await.unwrap();
    assert!(unlinked.1.contains("Sign in"));
    let failed = host
        .accept(&incoming("badlink", "link invalid"))
        .await
        .unwrap();
    assert!(failed.1.contains("could not be used"));
    let state = store.begin_login(1, 10).await.unwrap();
    store
        .complete_login(&state, "account", "synthetic-plex-token", 10)
        .await
        .unwrap();
    let code = store.issue_link("account", 10).await.unwrap();
    let linked = host
        .accept(&incoming("link", &format!("link {code}")))
        .await
        .unwrap();
    assert_eq!(linked.0, "account");
    assert!(linked.1.contains("account is linked"));
    let report = incoming("report", "The Glass Harbor won't play");
    host.handle(&channel, report.clone()).await.unwrap();
    host.handle(&channel, report.clone()).await.unwrap();
    assert_eq!(recording.calls.lock().unwrap().len(), 1);
    let key = pitcrew_bot::server::message_key(&report);
    assert_eq!(
        store
            .delivery_state("tg", &key, "10")
            .await
            .unwrap()
            .as_deref(),
        Some("confirmed")
    );
    *auth.access.lock().unwrap() = Some(Err(AuthError::Unavailable));
    let denied = host
        .accept(&incoming("denied", "The Glass Harbor won't play"))
        .await
        .unwrap();
    assert!(!denied.1.contains("Glass"));
    *auth.access.lock().unwrap() = Some(Ok(vec![]));
    let denied = host.accept(&report).await.unwrap();
    assert!(!denied.1.contains("Glass"));
    *auth.access.lock().unwrap() = Some(Ok(vec!["films".into()]));
    for (id, outcome, state) in [
        ("uncertain", DeliveryOutcome::Uncertain, "uncertain"),
        ("rejected", DeliveryOutcome::NotAccepted, "not_accepted"),
        (
            "failed",
            DeliveryOutcome::PermanentFailure,
            "permanent_failure",
        ),
    ] {
        *recording.outcome.lock().unwrap() = Some(outcome);
        let message = incoming(id, "The Glass Harbor won't play");
        host.handle(&channel, message.clone()).await.unwrap();
        assert_eq!(
            store
                .delivery_state("tg", &pitcrew_bot::server::message_key(&message), "10")
                .await
                .unwrap()
                .as_deref(),
            Some(state)
        );
        host.handle(&channel, message).await.unwrap();
    }
    assert_eq!(recording.calls.lock().unwrap().len(), 4);
    let mut invalid = report;
    invalid.text = "".into();
    assert!(host.accept(&invalid).await.is_err());
    drop(host);
    let restarted = BotHost {
        store: Store::open(temp.path().join("host.sqlite")).await.unwrap(),
        catalog: catalog(),
        auth,
    };
    restarted
        .handle(
            &channel,
            incoming("uncertain", "The Glass Harbor won't play"),
        )
        .await
        .unwrap();
    assert_eq!(recording.calls.lock().unwrap().len(), 4);
}
type PollReply = (String, Result<Value, &'static str>);
struct PollFake(Mutex<std::collections::VecDeque<PollReply>>);
impl PollFake {
    fn new(values: Vec<(String, Value)>) -> Self {
        Self(Mutex::new(
            values.into_iter().map(|(p, v)| (p, Ok(v))).collect(),
        ))
    }
    fn done(&self) {
        assert!(self.0.lock().unwrap().is_empty());
    }
}
impl PollSource for PollFake {
    fn get<'a>(&'a self, path: &'a str) -> PollFuture<'a> {
        let (expected, result) = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected fake poll");
        assert_eq!(path, expected);
        Box::pin(async move { result })
    }
}
#[tokio::test]
async fn polling_pages_share_dispatch_and_bound_offsets_and_issue_pages() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("poll.sqlite")).await.unwrap();
    let host = BotHost {
        store,
        catalog: catalog(),
        auth: Arc::new(FakeAuth::default()),
    };
    let recording = Arc::new(Recording::default());
    let tg = PlatformChannel::new(Platform::Telegram, "tg".into(), recording.clone());
    let path = "/getUpdates?offset=7&timeout=25&allowed_updates=%5B%22message%22%5D";
    let page = json!({"ok":true,"result":[{"update_id":9,"message":{"message_id":1,"from":{"id":10,"is_bot":false},"chat":{"id":30,"type":"private"},"text":"broken"}},{"update_id":2}]});
    let fake = PollFake::new(vec![(path.into(), page)]);
    assert_eq!(
        telegram_page(&host, &tg, &fake, "tg", 7, 10).await.unwrap(),
        10
    );
    fake.done();
    assert_eq!(recording.calls.lock().unwrap().len(), 1);
    for (body, expected) in [
        (json!({"ok":true,"result":[]}), Ok(7)),
        (json!({"ok":true,"result":[{"update_id":1}]}), Ok(7)),
        (
            json!({"ok":false,"result":[]}),
            Err("Telegram polling rejected"),
        ),
        (json!({"ok":true}), Err("missing Telegram updates")),
        (
            json!({"ok":true,"result":null}),
            Err("invalid Telegram updates"),
        ),
        (
            json!({"ok":true,"result":[{"update_id":u64::MAX}]}),
            Err("Telegram update id overflow"),
        ),
        (
            json!({"ok":true,"result":vec![json!({"update_id":1});101]}),
            Err("oversized Telegram update page"),
        ),
    ] {
        let fake = PollFake::new(vec![(path.into(), body)]);
        assert_eq!(
            telegram_page(&host, &tg, &fake, "tg", 7, 10).await,
            expected
        );
        fake.done();
    }
    let fake = PollFake::new(vec![(
        path.into(),
        json!({"ok":true,"result":vec![json!({"update_id":7});100]}),
    )]);
    assert_eq!(
        telegram_page(&host, &tg, &fake, "tg", 7, 10).await.unwrap(),
        8
    );
    fake.done();
    let seerr = PlatformChannel::new(Platform::Seerr, "seerr".into(), recording.clone());
    let list = "/api/v1/issue?take=100&skip=0&filter=open";
    let fake = PollFake::new(vec![
        (list.into(), json!({"results":[{"id":30}]})),
        (
            "/api/v1/issue/30".into(),
            json!({"id":30,"status":1,"comments":[{"id":1,"message":"broken","user":{"id":10}},{"id":2,"message":"bot reply","user":{"id":99}}]}),
        ),
    ]);
    seerr_cycle(&host, &seerr, &fake, "seerr", 99, 10)
        .await
        .unwrap();
    fake.done();
    assert_eq!(recording.calls.lock().unwrap().len(), 2);
    for body in [
        json!({}),
        json!({"results":null}),
        json!({"results":vec![json!({"id":1});101]}),
        json!({"results":[{}]}),
    ] {
        let fake = PollFake::new(vec![(list.into(), body)]);
        assert!(
            seerr_cycle(&host, &seerr, &fake, "seerr", 99, 10)
                .await
                .is_err()
        );
        fake.done();
    }
    let fake = PollFake::new(vec![
        (list.into(), json!({"results":[{"id":1},{"id":1}]})),
        (
            "/api/v1/issue/1".into(),
            json!({"id":1,"status":2,"comments":[]}),
        ),
    ]);
    assert_eq!(
        seerr_cycle(&host, &seerr, &fake, "seerr", 99, 10).await,
        Err("Seerr returned a duplicate issue")
    );
    fake.done();
    // A full first page must advance skip, and ten full distinct pages hit the cap.
    for pages in [1, 10] {
        let mut script = Vec::new();
        for page in 0..pages {
            script.push((format!("/api/v1/issue?take=100&skip={}&filter=open",page*100),json!({"results":(page*100..page*100+100).map(|id|json!({"id":id})).collect::<Vec<_>>()})));
            for id in page * 100..page * 100 + 100 {
                script.push((
                    format!("/api/v1/issue/{id}"),
                    json!({"id":id,"status":2,"comments":[]}),
                ));
            }
        }
        if pages == 1 {
            script.push((
                "/api/v1/issue?take=100&skip=100&filter=open".into(),
                json!({"results":[]}),
            ));
        }
        let fake = PollFake::new(script);
        let result = seerr_cycle(&host, &seerr, &fake, "seerr", 99, 10).await;
        if pages == 1 {
            result.unwrap();
        } else {
            assert_eq!(result, Err("Seerr pagination limit reached"));
        }
        fake.done();
    }
}
#[tokio::test]
async fn bot_account_verification_and_seerr_metadata_are_explicit() {
    for (body, success) in [
        (json!({"bot":true}), true),
        (json!({"bot":false}), false),
        (json!({}), false),
    ] {
        let fake = PollFake::new(vec![("/users/@me".into(), body)]);
        assert_eq!(verify_discord_bot(&fake).await.is_ok(), success);
        fake.done();
    }
    let fake = PollFake::new(vec![]);
    assert_eq!(seerr_title(&fake, &json!({})).await.unwrap(), "");
    fake.done();
    for (kind, field) in [("movie", "title"), ("tv", "name")] {
        let fake = PollFake::new(vec![(
            format!("/api/v1/{kind}/7"),
            json!({field:"The Glass Harbor"}),
        )]);
        assert_eq!(
            seerr_title(
                &fake,
                &json!({"media":{"mediaType":kind,"tmdbId":7},"problemSeason":2,"problemEpisode":3})
            )
            .await
            .unwrap(),
            "The Glass Harbor season 2 episode 3 "
        );
        fake.done();
    }
    let fake = PollFake::new(vec![(
        "/api/v1/tv/7".into(),
        json!({"name":"The Glass Harbor"}),
    )]);
    assert_eq!(
        seerr_title(
            &fake,
            &json!({"media":{"mediaType":"tv","tmdbId":7},"problemSeason":0,"problemEpisode":0})
        )
        .await
        .unwrap(),
        "The Glass Harbor "
    );
    fake.done();
    for issue in [
        json!({"media":{}}),
        json!({"media":{"mediaType":"custom","tmdbId":7}}),
        json!({"media":{"mediaType":"tv"}}),
    ] {
        assert!(seerr_title(&PollFake::new(vec![]), &issue).await.is_err());
    }
    assert!(now().unwrap() > 1_000_000_000);
}
#[tokio::test]
async fn link_requires_fresh_access_and_does_not_consume_a_denied_code() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("link.sqlite")).await.unwrap();
    let auth = Arc::new(FakeAuth::default());
    let host = BotHost {
        store: store.clone(),
        catalog: catalog(),
        auth: auth.clone(),
    };
    let state = store.begin_login(1, 10).await.unwrap();
    store
        .complete_login(&state, "account", "synthetic-plex-token", 10)
        .await
        .unwrap();
    let code = store.issue_link("account", 10).await.unwrap();
    for access in [Err(AuthError::Unavailable), Ok(vec![])] {
        *auth.access.lock().unwrap() = Some(access);
        let result = host
            .accept(&incoming("link", &format!("link {code}")))
            .await
            .unwrap();
        assert!(result.1.contains("could not be used"));
        assert_eq!(store.linked_account("tg", "10").await.unwrap(), None);
        assert_eq!(
            store.link_account(&code, 10).await.unwrap().as_deref(),
            Some("account")
        );
    }
    *auth.access.lock().unwrap() = Some(Ok(vec!["films".into()]));
    assert!(
        host.accept(&incoming("link", &format!("link {code}")))
            .await
            .unwrap()
            .1
            .contains("account is linked")
    );
    assert_eq!(store.link_account(&code, 10).await.unwrap(), None);
}

#[tokio::test]
async fn configuration_requires_unique_channel_names_without_starting_platforms() {
    let temp = tempfile::tempdir().unwrap();
    let base = json!({"database":temp.path().join("run.sqlite"),"catalog":[],"plex_auth_url":"https://auth.example.invalid/","plex_server_url":"https://server.example.invalid/","plex_server_id":"synthetic","client_id":"test","discord":null,"telegram":null,"seerr":null});
    assert_eq!(
        run(serde_json::from_value(base.clone()).unwrap()).await,
        Err("configure at least one channel with unique channel ids")
    );
    let mut config = base.clone();
    config["discord"] = json!({"channel_id":"same","bot_token":"synthetic"});
    config["telegram"] = json!({"channel_id":"same","bot_token":"synthetic"});
    assert_eq!(
        validate_channels(&serde_json::from_value(config.clone()).unwrap()),
        Err("configure at least one channel with unique channel ids")
    );
    config["telegram"]["channel_id"] = json!("different");
    assert_eq!(
        validate_channels(&serde_json::from_value(config).unwrap()),
        Ok(())
    );
}
struct Events(std::collections::VecDeque<twilight_gateway::Event>);
impl DiscordEvents for Events {
    fn next(&mut self) -> EventFuture<'_> {
        let event = self.0.pop_front();
        Box::pin(async move { event })
    }
}
#[tokio::test]
async fn platform_runners_use_injected_fake_endpoints() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(BotHost {
        store: Store::open(temp.path().join("runtime.sqlite"))
            .await
            .unwrap(),
        catalog: catalog(),
        auth: Arc::new(FakeAuth::default()),
    });
    let (url, fake, task) = fake_http(200, json!({"bot":true,"id":"7"}).to_string()).await;
    let transport = Arc::new(HttpTransport::new(Platform::Discord, &url, "synthetic").unwrap());
    let event = json!({"id":"20","channel_id":"30","author":{"id":"10","username":"synthetic","discriminator":"0","avatar":null,"bot":false},"content":"hello","timestamp":"2026-01-01T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0});
    let mut events = Events(std::collections::VecDeque::from([
        twilight_gateway::Event::MessageCreate(Box::new(
            serde_json::from_value(event.clone()).unwrap(),
        )),
        twilight_gateway::Event::MessageCreate(Box::new(serde_json::from_value(event).unwrap())),
    ]));
    assert_eq!(
        run_discord(host.clone(), "discord", transport, &mut events).await,
        Err("Discord gateway stopped")
    );
    {
        let calls = fake.seen.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "/users/@me");
        assert_eq!(calls[1].0, "/channels/30/messages");
        assert!(calls[1].2["content"].as_str().unwrap().contains("Sign in"));
    }
    task.abort();
    let (url, _, task) = fake_http(200, json!({"ok":false}).to_string()).await;
    let transport = Arc::new(HttpTransport::new(Platform::Telegram, &url, "synthetic").unwrap());
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_telegram(host.clone(), "tg".into(), transport)
        )
        .await
        .unwrap(),
        Err("Telegram polling rejected")
    );
    task.abort();
    let (url, _, task) = fake_http(200, json!({}).to_string()).await;
    let transport = Arc::new(HttpTransport::new(Platform::Seerr, &url, "synthetic").unwrap());
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_seerr(host, "seerr".into(), 99, transport)
        )
        .await
        .unwrap(),
        Err("invalid Seerr issue page")
    );
    task.abort();
}

#[test]
fn binary_requires_an_explicit_mode_and_readable_configuration() {
    for (args, expected) in [
        (vec![], "usage: pitcrew-bot (test-channel|run) CONFIG.json"),
        (
            vec!["unknown", "missing-fixture.json"],
            "unknown bot mode; use test-channel or run",
        ),
        (
            vec!["run", "missing-fixture.json"],
            "could not read bot config",
        ),
        (
            vec!["test-channel", "missing-fixture.json"],
            "could not read test config",
        ),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_pitcrew-bot"))
            .env("TOKIO_WORKER_THREADS", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), expected);
    }
}

#[tokio::test]
async fn seerr_link_command_survives_metadata_enrichment_then_report_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("seerr-link.sqlite"))
        .await
        .unwrap();
    let state = store.begin_login(1, 10).await.unwrap();
    store
        .complete_login(&state, "account", "synthetic-plex-token", 10)
        .await
        .unwrap();
    let code = store.issue_link("account", 10).await.unwrap();
    let host = BotHost {
        store: store.clone(),
        catalog: catalog(),
        auth: Arc::new(FakeAuth::default()),
    };
    let recording = Arc::new(Recording::default());
    let channel = PlatformChannel::new(Platform::Seerr, "seerr".into(), recording.clone());
    let fake = PollFake::new(vec![
        (
            "/api/v1/issue?take=100&skip=0&filter=open".into(),
            json!({"results":[{"id":30}]}),
        ),
        (
            "/api/v1/issue/30".into(),
            json!({"id":30,"status":1,"media":{"mediaType":"movie","tmdbId":7},"comments":[{"id":1,"message":format!("link {code}"),"user":{"id":10}},{"id":2,"message":"won't play","user":{"id":10}}]}),
        ),
        (
            "/api/v1/movie/7".into(),
            json!({"title":"The Glass Harbor"}),
        ),
    ]);
    seerr_cycle(&host, &channel, &fake, "seerr", 99, 10)
        .await
        .unwrap();
    fake.done();
    assert_eq!(
        store
            .linked_account("seerr", "10")
            .await
            .unwrap()
            .as_deref(),
        Some("account")
    );
    let rows = store.reports("account").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].response["issues"][0]["item_id"], "glass");
    let calls = recording.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0].body["message"]
            .as_str()
            .unwrap()
            .contains("account is linked")
    );
}

#[tokio::test]
async fn http_response_limit_accepts_exact_boundary_and_rejects_valid_oversize_json() {
    for length in [1_048_576_usize, 1_048_577, 1_048_578] {
        let expected = json!("x".repeat(length - 2));
        let (url, _, task) = fake_http(200, expected.to_string()).await;
        let transport = HttpTransport::new(Platform::Seerr, &url, "synthetic").unwrap();
        let result = transport.get("/response").await;
        if length == 1_048_576 {
            assert!(result.unwrap() == expected);
        } else {
            assert_eq!(result, Err("channel response too large"));
        }
        task.abort();
    }
}

#[tokio::test]
async fn channel_redirect_never_forwards_credentials_to_another_origin() {
    let (destination, fake, destination_task) = fake_http(200, json!({"id":7}).to_string()).await;
    let app = Router::new().fallback(any(move || {
        let destination = destination.clone();
        async move {
            Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header("location", destination)
                .body(Body::empty())
                .unwrap()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let source_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let transport = HttpTransport::new(Platform::Seerr, &url, "synthetic").unwrap();
    assert_eq!(transport.get("/poll").await, Err("channel poll rejected"));
    assert_eq!(
        transport
            .send(SendRequest {
                path: "/send".into(),
                body: json!({})
            })
            .await,
        Ok(DeliveryOutcome::Uncertain)
    );
    assert!(fake.seen.lock().unwrap().is_empty());
    source_task.abort();
    destination_task.abort();
}

#[tokio::test]
async fn duplicate_link_events_do_not_exhaust_fresh_link_lookup_quota() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("duplicate-link.sqlite"))
        .await
        .unwrap();
    let state = store.begin_login(1, 10).await.unwrap();
    store
        .complete_login(&state, "account", "synthetic-plex-token", 10)
        .await
        .unwrap();
    let code = store.issue_link("account", 10).await.unwrap();
    let host = BotHost {
        store: store.clone(),
        catalog: catalog(),
        auth: Arc::new(FakeAuth::default()),
    };
    let recording = Arc::new(Recording::default());
    let channel = PlatformChannel::new(Platform::Telegram, "tg".into(), recording.clone());
    // Leave two real lookups available: the original event and one fresh code.
    for _ in 0..98 {
        assert!(
            store
                .reserve_rate(
                    "link_lookup",
                    "global",
                    100,
                    pitcrew_intake_store::LINK_TTL,
                    10
                )
                .await
                .unwrap()
        );
    }
    let event = incoming("link", &format!("link {code}"));
    host.handle(&channel, event.clone()).await.unwrap();
    host.handle(&channel, event).await.unwrap();
    assert_eq!(recording.calls.lock().unwrap().len(), 1);
    let fresh = store.issue_link("account", 10).await.unwrap();
    assert_eq!(
        store.link_account(&fresh, 10).await.unwrap().as_deref(),
        Some("account")
    );
}
