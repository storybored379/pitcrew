use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pitcrew_domain::delivery::{DeliveryKey, DeliveryOutcome, DeliveryPurpose};
use pitcrew_notify::{
    AdminAlert, Notifier, NotifyError,
    http::{HttpNotifier, Protocol},
};
use pitcrew_upstream::Limits;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn alert() -> AdminAlert {
    AdminAlert {
        key: DeliveryKey {
            job: 7,
            purpose: DeliveryPurpose::AdminFailure,
            recipient: "admin-a".into(),
        },
        title: "Repair needs review".into(),
        body: "Review the repair evidence.".into(),
    }
}
fn limits() -> Limits {
    Limits {
        timeout: Duration::from_millis(500),
        max_response_bytes: 4096,
    }
}

#[derive(Debug)]
struct Captured {
    head: String,
    body: Vec<u8>,
}
enum Reply {
    Echo,
    Json(Value),
    Raw(&'static [u8]),
    Lost,
    StallHeaders,
    StallBody,
}
async fn exchange(
    protocol: Protocol,
    status: u16,
    reply: Reply,
    limits: Limits,
) -> (DeliveryOutcome, Captured) {
    exchange_alert(protocol, status, reply, limits, alert()).await
}
async fn exchange_alert(
    protocol: Protocol,
    status: u16,
    reply: Reply,
    limits: Limits,
    alert: AdminAlert,
) -> (DeliveryOutcome, Captured) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/prefix/admin-topic",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let split = loop {
            let mut chunk = [0; 1024];
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(offset) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        while bytes.len() < split + length {
            let mut chunk = [0; 1024];
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        let captured = Captured {
            head,
            body: bytes[split..].to_vec(),
        };
        let body = match reply {
            Reply::Echo => {
                let sent: Value = serde_json::from_slice(&captured.body).unwrap();
                serde_json::to_vec(&json!({"accepted":true,"key":sent["key"]})).unwrap()
            }
            Reply::Json(value) => serde_json::to_vec(&value).unwrap(),
            Reply::Raw(value) => value.to_vec(),
            Reply::Lost => return captured,
            Reply::StallHeaders => {
                tokio::time::sleep(Duration::from_millis(750)).await;
                return captured;
            }
            Reply::StallBody => {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(750)).await;
                return captured;
            }
        };
        let head = format!(
            "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(&body).await.unwrap();
        captured
    });
    let notifier = HttpNotifier::new(protocol, &endpoint, Some("test-credential"), limits).unwrap();
    assert!(!notifier.idempotent_delivery());
    let result = notifier.send(&alert).await.unwrap();
    (result, server.await.unwrap())
}

#[tokio::test]
async fn webhook_sends_exact_immutable_body_key_and_bearer_to_complete_configured_path() {
    let (outcome, sent) = exchange(Protocol::Webhook, 200, Reply::Echo, limits()).await;
    assert_eq!(outcome, DeliveryOutcome::Confirmed);
    assert!(
        sent.head
            .starts_with("POST /prefix/admin-topic HTTP/1.1\r\n")
    );
    assert!(
        sent.head
            .to_lowercase()
            .contains("authorization: bearer test-credential\r\n")
    );
    assert!(
        sent.head
            .to_lowercase()
            .contains("content-type: application/json\r\n")
    );
    let expected = alert();
    let sent_body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        sent_body,
        json!({"key":expected.key,"title":expected.title,"body":expected.body})
    );
    let key_header = sent
        .head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("x-pitcrew-delivery-key")
                .then(|| value.trim())
        })
        .unwrap();
    let decoded: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(key_header).unwrap()).unwrap();
    assert_eq!(decoded, serde_json::to_value(expected.key).unwrap());
}

#[tokio::test]
async fn native_ntfy_sends_literal_text_and_requires_a_topic_message_receipt() {
    let (outcome, sent) = exchange(
        Protocol::Ntfy,
        201,
        Reply::Json(json!({"id":"receipt-a","event":"message","topic":"admin-topic"})),
        limits(),
    )
    .await;
    assert_eq!(outcome, DeliveryOutcome::Confirmed);
    assert_eq!(sent.body, alert().body.as_bytes());
    assert!(
        sent.head
            .to_lowercase()
            .contains("title: repair needs review\r\n")
    );
    assert!(
        sent.head
            .to_lowercase()
            .contains("content-type: text/plain; charset=utf-8\r\n")
    );
    for receipt in [
        json!({"id":"receipt-a","event":"open","topic":"admin-topic"}),
        json!({"id":"receipt-a","event":"message","topic":"other"}),
        json!({"id":"","event":"message","topic":"admin-topic"}),
        json!({"event":"message","topic":"admin-topic"}),
        json!({"id":7,"event":"message","topic":"admin-topic"}),
        json!({"id":"receipt-a","topic":"admin-topic"}),
        json!({"id":"receipt-a","event":"message"}),
    ] {
        assert_eq!(
            exchange(Protocol::Ntfy, 200, Reply::Json(receipt), limits())
                .await
                .0,
            DeliveryOutcome::Uncertain
        );
    }
}

#[tokio::test]
async fn only_explicit_matching_webhook_receipt_confirms_and_no_ambiguous_result_retries() {
    for receipt in [
        json!({}),
        json!({"accepted":false,"key":alert().key}),
        json!({"accepted":true}),
        json!({"accepted":true,"key":{"job":8}}),
        json!({"accepted":"true","key":alert().key}),
        json!({"key":alert().key}),
    ] {
        assert_eq!(
            exchange(Protocol::Webhook, 200, Reply::Json(receipt), limits())
                .await
                .0,
            DeliveryOutcome::Uncertain
        );
    }
    for reply in [
        Reply::Raw(b"not-json"),
        Reply::Raw(b""),
        Reply::Lost,
        Reply::StallHeaders,
        Reply::StallBody,
    ] {
        assert_eq!(
            exchange(Protocol::Webhook, 200, reply, limits()).await.0,
            DeliveryOutcome::Uncertain
        );
    }
    // A parsed acknowledgement after an error or redirect is never acceptance.
    for status in [199, 300, 301, 307, 399, 408, 500, 599] {
        // HTTP 1xx is interim, then the peer closes: still uncertain.
        assert_eq!(
            exchange(Protocol::Webhook, status, Reply::Echo, limits())
                .await
                .0,
            DeliveryOutcome::Uncertain
        );
    }
    for status in [400, 401, 403, 404, 407, 409, 428, 430, 499] {
        assert_eq!(
            exchange(Protocol::Webhook, status, Reply::Echo, limits())
                .await
                .0,
            DeliveryOutcome::PermanentFailure
        );
    }
    assert_eq!(
        exchange(Protocol::Webhook, 429, Reply::Echo, limits())
            .await
            .0,
        DeliveryOutcome::NotAccepted
    );
    assert_eq!(
        exchange(Protocol::Webhook, 299, Reply::Echo, limits())
            .await
            .0,
        DeliveryOutcome::Confirmed
    );
}

#[tokio::test]
async fn response_bound_is_inclusive_and_oversized_receipt_is_uncertain() {
    let body = serde_json::to_vec(&json!({"accepted":true,"key":alert().key})).unwrap();
    let exact = Limits {
        max_response_bytes: body.len(),
        ..limits()
    };
    assert_eq!(
        exchange(Protocol::Webhook, 200, Reply::Echo, exact).await.0,
        DeliveryOutcome::Confirmed
    );
    let short = Limits {
        max_response_bytes: body.len() - 1,
        ..limits()
    };
    assert_eq!(
        exchange(Protocol::Webhook, 200, Reply::Echo, short).await.0,
        DeliveryOutcome::Uncertain
    );
}

#[test]
fn validates_private_configuration_without_network_or_value_disclosure() {
    for url in [
        "garbage",
        "file:///tmp/secret",
        "ftp://example.invalid/topic",
        "https://user@localhost/topic",
        "https://:private@localhost/topic",
        "https://example.invalid/topic?private=yes",
        "https://example.invalid/topic#private",
    ] {
        assert!(HttpNotifier::new(Protocol::Webhook, url, None, limits()).is_err());
    }
    for token in [
        "",
        "two words",
        "bad\nsecret",
        "non-ascii-é",
        &"a".repeat(4097),
    ] {
        assert!(
            HttpNotifier::new(
                Protocol::Webhook,
                "https://example.invalid/notify",
                Some(token),
                limits()
            )
            .is_err()
        );
    }
    for limit in [
        Limits {
            timeout: Duration::ZERO,
            ..limits()
        },
        Limits {
            timeout: Duration::from_secs(3600) + Duration::from_nanos(1),
            ..limits()
        },
        Limits {
            max_response_bytes: 0,
            ..limits()
        },
        Limits {
            max_response_bytes: 64 * 1024 * 1024 + 1,
            ..limits()
        },
    ] {
        assert!(
            HttpNotifier::new(
                Protocol::Webhook,
                "https://example.invalid/notify",
                None,
                limit
            )
            .is_err()
        );
    }
    for url in [
        "https://example.invalid",
        "https://example.invalid/topic/",
        "https://example.invalid/topic.name",
        "https://example.invalid/topic%20name",
    ] {
        assert!(HttpNotifier::new(Protocol::Ntfy, url, None, limits()).is_err());
    }
    for (protocol, url) in [
        (Protocol::Ntfy, "https://example.invalid/proxy/topic_123-A"),
        (Protocol::Webhook, "https://example.invalid/"),
    ] {
        assert!(
            HttpNotifier::new(
                protocol,
                url,
                Some(&"a".repeat(4096)),
                Limits {
                    timeout: Duration::from_secs(3600),
                    max_response_bytes: 64 * 1024 * 1024
                }
            )
            .is_ok()
        );
        assert!(
            HttpNotifier::new(
                protocol,
                url,
                None,
                Limits {
                    timeout: Duration::from_nanos(1),
                    max_response_bytes: 1
                }
            )
            .is_ok()
        );
    }
}

#[tokio::test]
async fn invalid_alerts_never_open_a_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/topic", listener.local_addr().unwrap());
    for (protocol, title, body) in [
        (Protocol::Webhook, "".into(), "body".into()),
        (Protocol::Webhook, "a".repeat(1025), "body".into()),
        (Protocol::Webhook, "title".into(), "".into()),
        (Protocol::Webhook, "title".into(), "a".repeat(65537)),
        (Protocol::Ntfy, "unsafe\r\nheader".into(), "body".into()),
        (Protocol::Ntfy, "non-ascii-é".into(), "body".into()),
    ] {
        let notifier = HttpNotifier::new(protocol, &endpoint, None, limits()).unwrap();
        assert_eq!(
            notifier
                .send(&AdminAlert {
                    title,
                    body,
                    ..alert()
                })
                .await,
            Err(NotifyError::Unavailable)
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn alert_size_limits_accept_exact_boundary_and_webhook_unicode() {
    for (title, body) in [
        ("a".repeat(1024), "b".repeat(65536)),
        ("Repair é".into(), "Evidence é".into()),
    ] {
        let alert = AdminAlert {
            title,
            body,
            ..alert()
        };
        assert_eq!(
            exchange_alert(Protocol::Webhook, 200, Reply::Echo, limits(), alert)
                .await
                .0,
            DeliveryOutcome::Confirmed
        );
    }
}
