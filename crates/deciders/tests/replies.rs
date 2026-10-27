use pitcrew_deciders::{
    providers::{ClaudeDecider, ProviderConfig},
    reply::{ClaudeReplyWriter, RedactedReplyFacts, ReplyStage, ReplyWriter, TemplateReplyWriter},
};
use pitcrew_domain::*;
use pitcrew_upstream::{Error, Request, Response, ResponseFuture, Transport};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn templates_are_short_honest_and_owner_name_is_configurable() {
    let default = TemplateReplyWriter::default();
    for (facts, stage, expected) in [
        (
            RedactedReplyFacts::received(),
            ReplyStage::Received,
            "Got your report. We'll check what's happening.",
        ),
        (
            RedactedReplyFacts::checking(),
            ReplyStage::Checking,
            "We're checking this one. We'll update you when we know more.",
        ),
        (
            RedactedReplyFacts::escalated(),
            ReplyStage::Escalated,
            "This one needs the server owner to take a look. Your report has the details.",
        ),
        (
            RedactedReplyFacts::self_resolved(),
            ReplyStage::Closed,
            "Glad it's working again. Let us know if it comes back.",
        ),
    ] {
        assert_eq!(facts.stage(), stage);
        let result = default.write(&facts).await;
        assert_eq!(result.delivery_text, expected);
        assert!(result.draft.is_none());
        assert!(!result.draft_failed);
    }
    let custom = TemplateReplyWriter::new("Morgan".into()).unwrap();
    assert_eq!(
        custom
            .write(&RedactedReplyFacts::escalated())
            .await
            .delivery_text,
        "This one needs Morgan to take a look. Your report has the details."
    );
    for invalid in [
        "".into(),
        " \t".into(),
        "a\nb".into(),
        "a\0b".into(),
        "x".repeat(81),
    ] {
        assert!(TemplateReplyWriter::new(invalid).is_err());
    }
    assert!(TemplateReplyWriter::new("é".repeat(80)).is_ok());
}

fn job() -> FixJob {
    FixJob::new(
        1,
        7,
        FileIdentity {
            id: 10,
            revision: 1,
        },
        Constraints::default(),
        Policy {
            max_grabs: 2,
            stage_timeout: 100,
            total_timeout: 1000,
        },
        0,
    )
    .unwrap()
}
fn observe(job: FixJob, observation: Observation) -> FixJob {
    job.apply(
        Event::Observed {
            intent: job.expected_observation().unwrap(),
            observation,
        },
        2,
    )
    .unwrap()
    .job
}

#[tokio::test]
async fn ready_only_after_exact_imported_decode_and_target_verification() {
    let templates = TemplateReplyWriter::default();
    let mut current = job();
    let check_pending = |job: &FixJob| {
        let facts = RedactedReplyFacts::for_job(job);
        assert_eq!(facts.stage(), ReplyStage::Fixing);
        facts
    };
    assert_eq!(
        templates
            .write(&check_pending(&current))
            .await
            .delivery_text,
        "The fix is still in progress. We'll check the new copy before saying it's ready."
    );
    let access = Access::Granted {
        library: 7,
        expires_at: 100,
    };
    current = current
        .apply(
            Event::SelectRelease {
                release: 20,
                access,
            },
            1,
        )
        .unwrap()
        .job;
    current = observe(current, Observation::GrabAccepted { download: 30 });
    check_pending(&current);
    let downloaded = FileIdentity {
        id: 40,
        revision: 1,
    };
    current = observe(
        current,
        Observation::DownloadCompleted {
            download: 30,
            file: downloaded,
        },
    );
    current = observe(
        current,
        Observation::DownloadDecoded {
            file: downloaded,
            good: true,
        },
    );
    check_pending(&current);
    current = current
        .apply(Event::RequestImport { access }, 2)
        .unwrap()
        .job;
    current = observe(current, Observation::ImportAccepted);
    let imported = FileIdentity {
        id: 50,
        revision: 2,
    };
    current = observe(
        current,
        Observation::ImportCompleted {
            download: 30,
            file: imported,
        },
    );
    check_pending(&current);
    let bad = observe(
        current.clone(),
        Observation::ImportedDecoded {
            file: imported,
            good: false,
        },
    );
    check_pending(&bad);
    current = observe(
        current,
        Observation::ImportedDecoded {
            file: imported,
            good: true,
        },
    );
    // Decode alone does not prove Plex is looking at the replacement.
    check_pending(&current);
    current = observe(
        current,
        Observation::TargetChecked {
            file: imported,
            plex_file: imported,
            constraints_satisfied: true,
        },
    );
    for verified in [&current, &current.apply(Event::QueueReady, 2).unwrap().job] {
        let facts = RedactedReplyFacts::for_job(verified);
        assert_eq!(facts.stage(), ReplyStage::Ready);
        assert_eq!(
            serde_json::to_value(&facts).unwrap(),
            json!({"stage":"ready"})
        );
        assert_eq!(
            templates.write(&facts).await.delivery_text,
            "The new copy passed our playback check. Ready to watch!"
        );
    }
    let failed = job().apply(Event::Fail(Failure::Cancelled), 1).unwrap().job;
    assert_eq!(
        RedactedReplyFacts::for_job(&failed).stage(),
        ReplyStage::Escalated
    );
}

struct Capture {
    requests: Arc<Mutex<Vec<Request>>>,
    response: Result<Vec<u8>, Error>,
}
impl Transport for Capture {
    fn send(&self, request: Request) -> ResponseFuture<'_> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            self.response
                .clone()
                .map(|body| Response { status: 200, body })
        })
    }
}
fn writer(
    enabled: bool,
    result: Result<Vec<u8>, Error>,
) -> (ClaudeReplyWriter<Capture>, Arc<Mutex<Vec<Request>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let config = ProviderConfig {
        cloud_opt_in: true,
        ..ProviderConfig::default()
    };
    let provider = ClaudeDecider::new(
        config,
        "synthetic-key",
        Capture {
            requests: requests.clone(),
            response: result,
        },
    )
    .unwrap();
    (
        ClaudeReplyWriter {
            provider,
            enabled,
            templates: TemplateReplyWriter::new("Morgan".into()).unwrap(),
        },
        requests,
    )
}

#[tokio::test]
async fn disabled_drafts_make_no_request_and_failure_keeps_templates() {
    let facts = RedactedReplyFacts::escalated();
    let (disabled, requests) = writer(false, Err(Error::Timeout));
    let result = disabled.write(&facts).await;
    assert!(requests.lock().unwrap().is_empty());
    assert!(result.draft.is_none());
    assert!(!result.draft_failed);
    let (enabled, requests) = writer(true, Err(Error::Timeout));
    let failed = enabled.write(&facts).await;
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(failed.delivery_text, result.delivery_text);
    assert!(failed.draft.is_none());
    assert!(failed.draft_failed);
}

#[tokio::test]
async fn cloud_draft_never_replaces_delivery_or_receives_the_owner_name() {
    let body = json!({"type":"message", "role":"assistant", "stop_reason":"end_turn",
        "content":[{"type":"text", "text":json!({"text":"Untrusted model claims it is ready."}).to_string()}],
        "usage":{"input_tokens": 31,"output_tokens": 13}}).to_string().into_bytes();
    let (writer, requests) = writer(true, Ok(body));
    let facts = RedactedReplyFacts::escalated();
    let result = writer.write(&facts).await;
    assert!(!result.draft_failed);
    assert_eq!(
        result.delivery_text,
        "This one needs Morgan to take a look. Your report has the details."
    );
    let draft = result.draft.unwrap();
    assert_eq!(
        draft.text_for_review(),
        "Untrusted model claims it is ready."
    );
    assert_eq!(draft.usage.input_tokens, 31);
    assert_eq!(draft.usage.output_tokens, 13);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body = requests[0].body.as_ref().unwrap();
    assert!(!body.to_string().contains("Morgan"));
    assert!(body.to_string().contains("escalated"));
}
