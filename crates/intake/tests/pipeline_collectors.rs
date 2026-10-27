use pitcrew_diagnosis::collectors::*;
use pitcrew_domain::FileIdentity;
use pitcrew_intake::{
    CatalogItem, Resolution,
    pipeline::{collectors::Collectors, *},
};
use pitcrew_upstream::Error;
use pitcrew_upstream::*;
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};

struct Fake {
    replies: Mutex<VecDeque<Result<Value, Error>>>,
    requests: Mutex<Vec<Request>>,
}
impl Fake {
    fn new(replies: Vec<Result<Value, Error>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(vec![]),
        }
    }
}
impl Transport for &Fake {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        assert_eq!(request.method, Method::Get);
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected read")
                .map(|v| Response {
                    status: 200,
                    body: serde_json::to_vec(&v).unwrap(),
                })
        })
    }
}
fn endpoint() -> Endpoint {
    Endpoint::new(
        "https://synthetic.invalid",
        Auth::bearer("synthetic-secret").unwrap(),
        Limits::default(),
    )
    .unwrap()
}
fn target() -> ResolvedTarget {
    ResolvedTarget {
        item: CatalogItem {
            id: "scope".into(),
            title: "Synthetic".into(),
            aliases: vec![],
            library_id: "library".into(),
            library_name: "Films".into(),
            resolution: Resolution::Hd,
            season: None,
            episode: None,
        },
        resource: "scope".into(),
        arr_instance_id: "arr".into(),
        arr_item_id: 1,
        arr_file_id: 2,
        agent_path: "/media/synthetic.mkv".into(),
        agent_file: Some(FileIdentity { id: 5, revision: 6 }),
        plex_rating_key: "72".into(),
        observed_at: 1,
        expires_at: 2000,
    }
}
fn agent() -> Value {
    json!({"file":{"id":5,"revision":6},"observed_at_unix_ms":1_000_000,"symlinks":null,"mount":null,"reads":[],"header":null,"decode":null,"plex_failures":[]})
}
fn logs() -> Value {
    json!({"failures":[],"truncated":false,"rejected_timestamps":0})
}
fn history() -> Value {
    json!({"response":{"result":"success","data":{"data":[{"rating_key":"72","date":999,"transcode_decision":"transcode"}]}}})
}
fn tracearr() -> Value {
    json!({"sessions":[{"rating_key":"72","minutes_ago":0,"video_decision":"directplay","product":"test","player_name":"test"}],"user_known":true})
}

#[tokio::test]
async fn collector_uses_agent_exact_path_identity_window_and_optional_correlated_telemetry() {
    let a = Fake::new(vec![Ok(agent()), Ok(logs())]);
    let t = Fake::new(vec![
        Ok(history()),
        Ok(json!({"response":{"result":"success","data":{"stream_count":1}}})),
    ]);
    let r = Fake::new(vec![
        Ok(json!({"sessions_24h":1,"latest_min_ago":0,"active_now":1})),
        Ok(tracearr()),
    ]);
    let source = Collectors {
        agent: Some(AgentCollector::new(endpoint(), &a)),
        tautulli: Some(TautulliClient::new(endpoint(), &t, "private-api-key".into()).unwrap()),
        tracearr: Some(TracearrBridgeClient::new(endpoint(), &r)),
    };
    let result = source.collect(&target(), "17", &|| 1_000_000).await;
    let Observation::Available(media) = result.agent.media else {
        panic!("missing agent")
    };
    assert_eq!(media.file, FileIdentity { id: 5, revision: 6 });
    assert!(matches!(result.agent.plex, Observation::Available(_)));
    let Observation::Available(s) = result.sessions else {
        panic!("missing sessions")
    };
    assert_eq!(s.direct_plays, 1);
    assert_eq!(s.transcodes, 0);
    let requests = a.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url.path(), "/v1/evidence");
    assert!(
        requests[0]
            .url
            .query_pairs()
            .any(|(k, v)| k == "path" && v == "/media/synthetic.mkv")
    );
    assert!(
        requests[0]
            .url
            .query_pairs()
            .any(|(k, v)| k == "decode" && v == "true")
    );
    assert!(
        requests[1]
            .url
            .query_pairs()
            .any(|(k, v)| k == "minutes" && v == "5")
    );
    let requests = t.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .url
            .query_pairs()
            .any(|(k, v)| k == "user_id" && v == "17")
    );
    let requests = r.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .url
            .query_pairs()
            .any(|(k, v)| k == "hours" && v == "24")
    );
}

#[tokio::test]
async fn absent_telemetry_and_non_numeric_accounts_remain_unknown() {
    let f = Fake::new(vec![]);
    let source = Collectors {
        agent: None,
        tautulli: Some(TautulliClient::new(endpoint(), &f, "key".into()).unwrap()),
        tracearr: None,
    };
    let result = source
        .collect(&target(), "local-account", &|| 1_000_000)
        .await;
    assert_eq!(result.agent, AgentEvidence::default());
    assert_eq!(result.sessions, Observation::default());
    assert!(f.requests.lock().unwrap().is_empty());
    let source: Collectors<&Fake> = Collectors {
        agent: None,
        tautulli: None,
        tracearr: None,
    };
    let result = source.collect(&target(), "17", &|| 1_000_000).await;
    assert_eq!(result.agent, AgentEvidence::default());
    assert_eq!(result.sessions, Observation::default());
}

#[tokio::test]
async fn tracearr_failure_retains_tautulli_and_agent_failure_never_fabricates_broken_bytes() {
    let a = Fake::new(vec![Err(Error::Timeout), Err(Error::Status(503))]);
    let t = Fake::new(vec![Ok(history()), Err(Error::Timeout)]);
    let r = Fake::new(vec![Err(Error::Timeout), Err(Error::Timeout)]);
    let source = Collectors {
        agent: Some(AgentCollector::new(endpoint(), &a)),
        tautulli: Some(TautulliClient::new(endpoint(), &t, "key".into()).unwrap()),
        tracearr: Some(TracearrBridgeClient::new(endpoint(), &r)),
    };
    let result = source.collect(&target(), "17", &|| 1_000_000).await;
    assert_eq!(
        result.agent.media,
        Observation::Unavailable(Unavailable::Timeout)
    );
    assert_eq!(
        result.agent.plex,
        Observation::Unavailable(Unavailable::Upstream)
    );
    let Observation::Available(s) = result.sessions else {
        panic!("missing fallback")
    };
    assert_eq!(s.transcodes, 1);
}

#[tokio::test]
async fn tracearr_alone_is_usable_without_tautulli() {
    let r = Fake::new(vec![
        Ok(json!({"sessions_24h":1,"latest_min_ago":0,"active_now":1})),
        Ok(tracearr()),
    ]);
    let source = Collectors {
        agent: None,
        tautulli: None,
        tracearr: Some(TracearrBridgeClient::new(endpoint(), &r)),
    };
    let result = source.collect(&target(), "17", &|| 1_000_000).await;
    let Observation::Available(s) = result.sessions else {
        panic!("missing sessions")
    };
    assert_eq!(s.direct_plays, 1);
}

#[tokio::test]
async fn collector_checks_expected_identity_and_clock_units() {
    for case in 0..3 {
        let a = Fake::new(vec![Ok(agent()), Ok(logs())]);
        let source = Collectors {
            agent: Some(AgentCollector::new(endpoint(), &a)),
            tautulli: None,
            tracearr: None,
        };
        let mut t = target();
        let now = match case {
            0 => {
                t.agent_file = Some(FileIdentity { id: 6, revision: 6 });
                1_000_000
            }
            1 => 1_300_001,
            _ => 999_999,
        };
        let result = source.collect(&t, "17", &move || now).await;
        assert_eq!(
            result.agent.media,
            Observation::Unavailable(if case == 0 {
                Unavailable::IdentityChanged
            } else {
                Unavailable::Stale
            })
        );
    }
}
