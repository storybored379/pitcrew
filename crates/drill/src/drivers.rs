//! Offline drivers. Each run owns temporary state; neither accepts a server URL.
use crate::{Corpus, DrillError, Observation, Target, label, observed_item, validate};
use pitcrew_bot::server::{TestConfig, TestMessage, TestPrincipal, TestServer};
use pitcrew_intake::{AuthorizedViewer, IntakeResponse};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Exercise the authenticated test channel's exact route entry and durable
/// journal. The supplied contexts are trusted fixtures, never answer labels.
pub async fn run_test_channel(corpus: &Corpus) -> Result<Vec<Observation>> {
    validate(corpus)?;
    let scratch = tempfile::tempdir()?;
    let mut observations = Vec::new();
    for (index, case) in corpus.cases.iter().enumerate() {
        let database = scratch.path().join(format!("case-{index}.sqlite"));
        let bearer = "synthetic-drill-bearer-with-no-live-authority";
        let viewer = case.viewer.clone().unwrap_or(AuthorizedViewer {
            // The test transport itself is authenticated even when the corpus
            // viewer is unlinked. A syntactically valid transport identity with
            // zero libraries reaches the real unauthorized intake response.
            account_id: "synthetic-unlinked-principal".into(),
            library_ids: Vec::new(),
        });
        let store = pitcrew_intake_store::Store::open(&database).await?;
        store
            .persist_intake(
                "test",
                "context-fixture",
                &viewer.account_id,
                &case.id,
                &serde_json::json!({"_context": case.context}),
                0,
            )
            .await?;
        let server = TestServer::open(TestConfig {
            bind: "127.0.0.1:0".parse()?,
            database,
            catalog: corpus.catalog.clone(),
            principals: vec![TestPrincipal {
                bearer_token: bearer.into(),
                viewer,
                expires_at: u64::MAX,
            }],
        })
        .await?;
        for (turn, row) in case.turns.iter().enumerate() {
            let response = server
                .message(
                    bearer,
                    TestMessage {
                        message_id: format!("message-{turn}"),
                        conversation_id: case.id.clone(),
                        text: row.text.clone(),
                    },
                    turn as u64,
                )
                .await
                .map_err(|_| DrillError::Dispatch)?;
            observations.push(observe(corpus, case, turn, response)?);
        }
    }
    Ok(observations)
}

fn observe(
    corpus: &Corpus,
    case: &crate::Case,
    turn: usize,
    response: IntakeResponse,
) -> std::result::Result<Observation, DrillError> {
    let targets = response
        .issues
        .iter()
        .map(|issue| {
            let item = observed_item(&corpus.catalog, case.viewer.as_ref(), &issue.item_id)?;
            Ok(Target {
                title: item.title.clone(),
                season: item.season,
                episode: item.episode,
                library: item.library_id.clone(),
                kinds: issue.kinds.iter().map(label).collect(),
            })
        })
        .collect::<std::result::Result<_, DrillError>>()?;
    Ok(Observation {
        case_id: case.id.clone(),
        turn,
        outcome: label(&response.outcome),
        targets,
        suspicious: response.suspicious_input,
        reply: response.reply,
        diagnosis: None,
        ready: None,
        unsafe_actions: Some(Vec::new()),
    })
}

/// Neutral entry point; the previous language-specific name remains compatible.
pub use run_python as run_external;

/// Launch a fresh, outbound-denied external adapter child for every case.
/// Only request/catalog/context fixtures cross stdin. Reference output stays in
/// scratch. Reply observations can contain reference-specific copy; keep them
/// private and publish only reviewed aggregate scores.
pub fn run_python(
    corpus: &Corpus,
    reference: &Path,
    interpreter: &Path,
) -> Result<Vec<Observation>> {
    validate(corpus)?;
    let reference = reference.canonicalize()?;
    if !reference.join("adapter.py").is_file() {
        return Err("external adapter directory must contain adapter.py".into());
    }
    let scratch = tempfile::tempdir()?;
    let requests = crate::python_requests(corpus)?;
    let input = serde_json::to_vec(&requests)?;
    let adapter = scratch.path().join("baseline.py");
    std::fs::write(&adapter, include_str!("../python/baseline.py"))?;
    let mut child = Command::new(interpreter)
        .args(["-I", "-B"])
        .arg(adapter)
        .arg(&reference)
        .arg(scratch.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("HOME", scratch.path())
        .env("TMPDIR", scratch.path())
        .current_dir(scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("child stdin unavailable")?
        .write_all(&input)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(
            "isolated external baseline failed; check the adapter contract and its dependencies"
                .into(),
        );
    }
    let observations: Vec<Observation> = serde_json::from_slice(&output.stdout)?;
    // Reject foreign/duplicate rows before they can be persisted as evidence.
    crate::score(corpus, &observations)?;
    Ok(observations)
}
