//! Synthetic fixture contracts and executable Rust lifecycle scenarios.
//!
//! HTTP fixtures are validated here, not claimed as Rust application passes. Domain
//! fixtures execute the real pure reducer, including every returned side-effect intent.
use pitcrew_domain::{Constraints, Event, FileIdentity, FixJob, Policy, State};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub step: String,
    #[serde(default)]
    pub pointer: String,
    pub r#where: Option<Value>,
    pub index: Option<usize>,
    pub after: Option<Value>,
    pub field: Option<String>,
    #[serde(default)]
    pub transforms: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Eq,
    Ne,
    Contains,
    ContainsAny,
    NotContains,
    LenEq,
    LenGe,
    Ge,
    Present,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Login {
        id: String,
        user: String,
        session: String,
    },
    Request {
        id: String,
        path: String,
        #[serde(default = "get_method")]
        method: String,
        session: Option<String>,
        headers: Option<Value>,
        query: Option<Value>,
        body: Option<Value>,
        csrf: Option<bool>,
        repeat: Option<usize>,
    },
    Capture {
        id: String,
        name: String,
        actual: Selection,
    },
    Assert {
        id: String,
        actual: Selection,
        predicate: Predicate,
        expected: Option<Value>,
    },
}
fn get_method() -> String {
    "GET".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpFixture {
    pub version: u32,
    pub id: String,
    pub adapter: String,
    pub description: String,
    pub checks: Vec<String>,
    #[serde(default, rename = "known_external_bug", alias = "known_python_bug")]
    pub known_python_bug: bool,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(
        default,
        rename = "expected_external_failures",
        alias = "expected_python_failures"
    )]
    pub expected_python_failures: Vec<String>,
    pub review: Option<ReviewContract>,
    pub variables: Option<Value>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewContract {
    pub finding: u8,
    pub requirement: String,
    pub fixture: Value,
    pub inputs: Vec<Value>,
    pub injected_behavior: Vec<String>,
    pub expected: Value,
    pub observability: String,
}

impl HttpFixture {
    pub fn validate(&self) -> Result<usize> {
        if self.version != 1
            || !["external_http", "python_http"].contains(&self.adapter.as_str())
            || self.id.is_empty()
            || self.description.is_empty()
        {
            return Err("invalid fixture version, adapter or identity".into());
        }
        if self.known_python_bug != self.review.is_some() {
            return Err("known external bugs require an explicit review contract".into());
        }
        if let Some(review) = &self.review
            && (review.finding == 0
                || review.inputs.is_empty()
                || review.injected_behavior.is_empty()
                || review.requirement.is_empty()
                || review.observability.is_empty()
                || review.expected.is_null())
        {
            return Err("incomplete review regression contract".into());
        }
        let mut ids = BTreeSet::new();
        let mut observations = BTreeSet::new();
        let mut assertions = 0;
        for step in &self.steps {
            let (id, selection) = match step {
                Step::Login { id, user, session } => {
                    if user.is_empty() || session.is_empty() {
                        return Err("empty login fixture".into());
                    }
                    observations.insert(id.clone());
                    (id, None)
                }
                Step::Request {
                    id,
                    path,
                    method,
                    repeat,
                    ..
                } => {
                    if !path.starts_with('/')
                        || path.starts_with("//")
                        || path.contains("://")
                        || path.contains('\\')
                    {
                        return Err("fixture requests must use relative paths".into());
                    }
                    if !["GET", "POST", "DELETE"].contains(&method.as_str())
                        || repeat.is_some_and(|n| n == 0 || n > 500)
                    {
                        return Err("invalid method or repetition bound".into());
                    }
                    observations.insert(id.clone());
                    (id, None)
                }
                Step::Capture { id, actual, .. } => (id, Some(actual)),
                Step::Assert {
                    id,
                    actual,
                    predicate,
                    expected,
                } => {
                    if !matches!(predicate, Predicate::Present) && expected.is_none() {
                        // JSON null is a valid expected value; Option maps it to None.
                        if !matches!(predicate, Predicate::Eq | Predicate::Ne) {
                            return Err(format!("{id}: missing expected value"));
                        }
                    }
                    assertions += 1;
                    (id, Some(actual))
                }
            };
            if !ids.insert(id) {
                return Err(format!("duplicate step id: {id}"));
            }
            if let Some(select) = selection {
                if select
                    .transforms
                    .iter()
                    .any(|t| !["length", "lower", "json"].contains(&t.as_str()))
                {
                    return Err(format!("{id}: invalid selection transform"));
                }
                if !observations.contains(&select.step) {
                    return Err(format!(
                        "{id}: selection refers to an unavailable observation"
                    ));
                }
                for pointer in [&select.pointer, select.field.as_deref().unwrap_or("")] {
                    if !pointer.is_empty() && !pointer.starts_with('/') {
                        return Err(format!("{id}: invalid JSON pointer"));
                    }
                }
            }
        }
        if self.known_python_bug
            && (self.expected_python_failures.is_empty()
                || self.expected_python_failures.iter().any(|id| {
                    !self
                        .steps
                        .iter()
                        .any(|step| matches!(step, Step::Assert {id: actual, ..} if actual == id))
                }))
        {
            return Err("known bug must name exact failing assertion IDs".into());
        }
        if assertions == 0 {
            return Err("fixture has no behavior assertions".into());
        }
        Ok(assertions)
    }
}

/// Recursive subset matching. Array patterns match prefixes, useful for call tuples.
pub fn subset(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => b
            .iter()
            .all(|(key, value)| a.get(key).is_some_and(|v| subset(v, value))),
        (Value::Array(a), Value::Array(b)) => {
            a.len() >= b.len() && a.iter().zip(b).all(|(a, b)| subset(a, b))
        }
        _ => actual == expected,
    }
}

pub fn evaluate(actual: &Value, predicate: &Predicate, expected: &Value) -> Result<bool> {
    let length = || {
        actual
            .as_array()
            .map(Vec::len)
            .or_else(|| actual.as_str().map(|s| s.chars().count()))
            .or_else(|| actual.as_object().map(serde_json::Map::len))
            .ok_or_else(|| "length assertion needs collection".to_owned())
    };
    let contains = || match actual {
        Value::Array(a) => Ok(a.iter().any(|v| subset(v, expected))),
        Value::Object(a) => expected
            .as_str()
            .map(|e| a.contains_key(e))
            .ok_or_else(|| "key match needs string".to_owned()),
        Value::String(s) => expected
            .as_str()
            .map(|e| s.contains(e))
            .ok_or_else(|| "string match needs string".to_owned()),
        _ => Err("contains assertion needs string or array".into()),
    };
    Ok(match predicate {
        Predicate::Eq => actual == expected,
        Predicate::Ne => actual != expected,
        Predicate::Contains => contains()?,
        Predicate::ContainsAny => actual
            .as_str()
            .zip(expected.as_array())
            .is_some_and(|(a, e)| {
                e.iter()
                    .any(|part| part.as_str().is_some_and(|part| a.contains(part)))
            }),
        Predicate::NotContains => !contains()?,
        Predicate::LenEq => u64::try_from(length()?).ok() == expected.as_u64(),
        Predicate::LenGe => {
            let actual = length()?;
            expected
                .as_u64()
                .is_some_and(|e| u64::try_from(actual).is_ok_and(|a| a >= e))
        }
        Predicate::Ge => actual
            .as_f64()
            .zip(expected.as_f64())
            .is_some_and(|(a, e)| a >= e),
        Predicate::Present => !actual.is_null(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainFixture {
    pub version: u32,
    pub id: String,
    pub adapter: String,
    pub description: String,
    pub initial: InitialJob,
    pub steps: Vec<DomainStep>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialJob {
    pub id: u64,
    pub library: u64,
    pub original: FileIdentity,
    pub constraints: Constraints,
    pub policy: Policy,
    pub now: u64,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainStep {
    pub at: u64,
    pub event: Event,
    pub state: State,
    #[serde(default)]
    pub rejected: bool,
    pub effects: Vec<String>,
    pub effect_payloads: Option<Vec<Value>>,
    pub reserved_budget: u32,
    pub active_grabs: u8,
}
impl DomainFixture {
    pub fn run(self) -> Result<usize> {
        if self.version != 1
            || self.adapter != "domain"
            || self.id.is_empty()
            || self.description.is_empty()
            || self.steps.is_empty()
        {
            return Err("invalid domain fixture".into());
        }
        let i = self.initial;
        let mut job = FixJob::new(i.id, i.library, i.original, i.constraints, i.policy, i.now)
            .map_err(|e| e.to_string())?;
        let count = self.steps.len();
        for (index, step) in self.steps.into_iter().enumerate() {
            let result = job.apply(step.event.clone(), step.at);
            let mut payloads = vec![];
            let effects = match result {
                Ok(transition) if !step.rejected => {
                    job = transition.job;
                    transition
                        .intents
                        .into_iter()
                        .map(|intent| {
                            let value =
                                serde_json::to_value(intent.effect).expect("effects serialize");
                            payloads.push(value.clone());
                            value.as_str().map(str::to_owned).unwrap_or_else(|| {
                                value
                                    .as_object()
                                    .expect("enum object")
                                    .keys()
                                    .next()
                                    .expect("variant")
                                    .clone()
                            })
                        })
                        .collect::<Vec<_>>()
                }
                Err(_) if step.rejected => vec![],
                other => {
                    return Err(format!(
                        "{} step {index}: expected rejection {}, got {other:?}",
                        self.id, step.rejected
                    ));
                }
            };
            if job.state() != step.state
                || effects != step.effects
                || step
                    .effect_payloads
                    .as_ref()
                    .is_some_and(|expected| *expected != payloads)
                || job.reserved_budget() != step.reserved_budget
                || job.active_grabs() != step.active_grabs
            {
                return Err(format!(
                    "{} step {index}: observed state {:?}, effects {effects:?}, budget {}, grabs {}; expected {step:?}",
                    self.id,
                    job.state(),
                    job.reserved_budget(),
                    job.active_grabs()
                ));
            }
        }
        Ok(count)
    }
}

pub fn fixture_paths(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![];
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            paths.extend(fixture_paths(&path)?);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn check_suite(directory: &Path) -> Result<String> {
    let (mut http, mut assertions, mut domain, mut transitions) = (0, 0, 0, 0);
    let mut ids = BTreeSet::new();
    let mut contracts = Vec::new();
    for path in fixture_paths(directory)? {
        let contents = fs::read(&path).map_err(|e| e.to_string())?;
        let raw: Value =
            serde_json::from_slice(&contents).map_err(|e| format!("{}: {e}", path.display()))?;
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .ok_or("missing fixture id")?;
        if !ids.insert(id.to_owned()) {
            return Err(format!("duplicate scenario {id}"));
        }
        match raw.get("adapter").and_then(Value::as_str) {
            Some("external_http" | "python_http") => {
                let fixture: HttpFixture =
                    serde_json::from_value(raw).map_err(|e| format!("{}: {e}", path.display()))?;
                assertions += fixture
                    .validate()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                http += 1;
                contracts.push(fixture);
            }
            Some("domain") => {
                let fixture: DomainFixture =
                    serde_json::from_value(raw).map_err(|e| e.to_string())?;
                transitions += fixture.run()?;
                domain += 1;
            }
            other => return Err(format!("{}: unknown adapter {other:?}", path.display())),
        }
    }
    if http == 0 || domain == 0 {
        return Err("suite must contain HTTP contracts and executable domain scenarios".into());
    }
    let checks = if directory.join("source-checks.txt").exists()
        || directory.join("python-results.ndjson").exists()
    {
        validate_parity_evidence(directory, &contracts)?
    } else {
        0
    };
    Ok(format!(
        "{domain} Rust domain scenarios executed ({transitions} transitions); {http} external HTTP contracts validated ({assertions} assertions, {checks} optional baseline checks; HTTP behavior was not executed)"
    ))
}

#[derive(Deserialize)]
struct ReplayResult {
    id: String,
    status: String,
    #[serde(default)]
    passed: Vec<String>,
    #[serde(default)]
    failed: Vec<String>,
}

/// Enforce the complete source inventory and exact assertion IDs in recorded
/// baseline evidence when supplied separately. Validation does not execute an external app.
fn validate_parity_evidence(directory: &Path, contracts: &[HttpFixture]) -> Result<usize> {
    let source =
        fs::read_to_string(directory.join("source-checks.txt")).map_err(|e| e.to_string())?;
    let evidence =
        fs::read_to_string(directory.join("python-results.ndjson")).map_err(|e| e.to_string())?;
    validate_parity_data(&source, &evidence, contracts)
}

fn validate_parity_data(source: &str, evidence: &str, contracts: &[HttpFixture]) -> Result<usize> {
    let checks: Vec<_> = source
        .lines()
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .collect();
    let expected: BTreeSet<_> = checks.iter().copied().collect();
    if checks.is_empty() || expected.len() != checks.len() {
        return Err("source inventory must contain distinct baseline checks".into());
    }
    let mut results = BTreeMap::new();
    for line in evidence.lines().filter(|l| !l.is_empty()) {
        let result: ReplayResult = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if results.insert(result.id.clone(), result).is_some() {
            return Err("duplicate baseline result".into());
        }
    }
    if results.len() != contracts.len() {
        return Err("baseline evidence does not match the current fixture inventory".into());
    }
    let mut mapped = BTreeSet::new();
    for fixture in contracts {
        let result = results
            .get(&fixture.id)
            .ok_or_else(|| format!("{} has no baseline result", fixture.id))?;
        if !fixture.checks.is_empty() {
            if result.status != "pass" || fixture.known_python_bug {
                return Err(format!(
                    "{} cannot claim passing source coverage",
                    fixture.id
                ));
            }
            mapped.extend(fixture.checks.iter().map(String::as_str));
        }
        if result.status == "unsupported" && fixture.known_python_bug {
            if !result.passed.is_empty() || !result.failed.is_empty() {
                return Err("unsupported case must not claim executed assertions".into());
            }
            continue;
        }
        let assertions: BTreeSet<_> = fixture
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Assert { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let observed: BTreeSet<_> = result
            .passed
            .iter()
            .chain(&result.failed)
            .map(String::as_str)
            .collect();
        if assertions != observed || observed.len() != result.passed.len() + result.failed.len() {
            return Err(format!(
                "{} has stale or incomplete assertion evidence",
                fixture.id
            ));
        }
        let failed: BTreeSet<_> = result.failed.iter().map(String::as_str).collect();
        let permitted: BTreeSet<_> = fixture
            .expected_python_failures
            .iter()
            .map(String::as_str)
            .collect();
        if fixture.known_python_bug {
            if !["known_external_bug", "known_python_bug"].contains(&result.status.as_str())
                || failed != permitted
            {
                return Err(format!(
                    "{} does not match its exact known failure",
                    fixture.id
                ));
            }
        } else if result.status != "pass" || !failed.is_empty() {
            return Err(format!("{} did not pass", fixture.id));
        }
    }
    if mapped != expected {
        return Err("source check coverage is incomplete or contains unknown IDs".into());
    }
    Ok(expected.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitcrew_domain::{Access, GrabResolution, IntentKey, Observation};
    use pitcrew_fakes::{Reply, ScriptedCall, ScriptedUpstream};
    use serde_json::json;

    #[test]
    fn assertions_catch_wrong_instance_extra_mutations_and_missing_notification() {
        let calls = json!([["uhd", "grab", 8], ["hd", "delete", 2]]);
        assert!(!evaluate(&calls, &Predicate::Contains, &json!(["hd", "grab", 8])).unwrap());
        assert!(!evaluate(&calls, &Predicate::NotContains, &json!(["hd", "delete"])).unwrap());
        assert!(evaluate(&calls, &Predicate::Contains, &json!(["uhd", "grab", 8])).unwrap());
        assert!(!evaluate(&json!([]), &Predicate::LenEq, &json!(1)).unwrap());
        assert!(evaluate(&Value::Null, &Predicate::LenGe, &json!(0)).is_err());
        assert!(
            evaluate(
                &json!({"reason":"banter"}),
                &Predicate::NotContains,
                &json!("report_id")
            )
            .unwrap()
        );
        assert!(
            !evaluate(
                &json!({"report_id":7}),
                &Predicate::NotContains,
                &json!("report_id")
            )
            .unwrap()
        );
        assert!(evaluate(&json!({"report_id":7}), &Predicate::Contains, &json!(7)).is_err());

        assert!(!subset(
            &json!({"status":"ready"}),
            &json!({"status":"ready","decoded":true})
        ));
        assert!(evaluate(&Value::Null, &Predicate::Contains, &json!("ready")).is_err());
        assert!(
            evaluate(
                &json!("a 🫶 response"),
                &Predicate::ContainsAny,
                &json!(["❤", "🫶"])
            )
            .unwrap()
        );
        assert!(
            !evaluate(
                &json!("no heart"),
                &Predicate::ContainsAny,
                &json!(["❤", "🫶"])
            )
            .unwrap()
        );
        assert!(!evaluate(&json!(1), &Predicate::Ge, &json!(2)).unwrap());
        assert!(evaluate(&json!(2), &Predicate::Ge, &json!(2)).unwrap());
    }

    #[test]
    fn coverage_rejects_a_missing_check_and_skipped_or_duplicate_assertions() {
        let source = "auth\nready\n";
        let evidence = r#"{"id":"synthetic-auth","status":"pass","passed":["denied"]}
{"id":"synthetic-ready","status":"pass","passed":["verified"]}
"#;
        let fixture = |id: &str, check: &str, assertion: &str| {
            serde_json::from_value(json!({
                "version":1,"id":id,"adapter":"external_http","description":"Synthetic contract",
                "checks":[check],"steps":[
                    {"op":"request","id":"request","path":"/example"},
                    {"op":"assert","id":assertion,"actual":{"step":"request"},"predicate":"present"}
                ]
            }))
            .unwrap()
        };
        let mut fixtures: Vec<HttpFixture> = vec![
            fixture("synthetic-auth", "auth", "denied"),
            fixture("synthetic-ready", "ready", "verified"),
        ];
        assert_eq!(validate_parity_data(source, evidence, &fixtures), Ok(2));
        assert!(validate_parity_data("", evidence, &fixtures).is_err());
        assert!(validate_parity_data("auth\nauth\n", evidence, &fixtures).is_err());
        fixtures[0].checks.clear();
        assert!(validate_parity_data(source, evidence, &fixtures).is_err());
        fixtures[0].checks.push("auth".into());
        let mut records: Vec<Value> = evidence
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let pass = records.iter_mut().find(|r| r["status"] == "pass").unwrap();
        pass["passed"].as_array_mut().unwrap().pop();
        let skipped = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(validate_parity_data(source, &skipped, &fixtures).is_err());
        assert!(
            validate_parity_data(
                source,
                &(evidence.to_owned() + evidence.lines().next().unwrap()),
                &fixtures
            )
            .is_err()
        );
    }

    #[test]
    fn optional_external_scenarios_validate_when_supplied() {
        let Some(directory) = std::env::var_os("PITCREW_EXTERNAL_SCENARIOS") else {
            eprintln!("optional external scenarios skipped: PITCREW_EXTERNAL_SCENARIOS is unset");
            return;
        };
        check_suite(Path::new(&directory)).unwrap();
    }

    #[test]
    fn synthetic_external_regressions_keep_known_failures_distinct_from_skips() {
        let fixture = |id: &str| {
            json!({
                "version":1,"id":id,"adapter":"external_http","description":"Synthetic regression",
                "checks":[],"steps":[
                    {"op":"request","id":"request","path":"/example"},
                    {"op":"assert","id":"assertion","actual":{"step":"request"},"predicate":"present"}
                ]
            })
        };
        let mut normal = fixture("normal");
        normal["checks"] = json!(["check"]);
        let mut regression = fixture("regression");
        regression["known_external_bug"] = json!(true);
        regression["expected_external_failures"] = json!(["assertion"]);
        regression["review"] = json!({
            "finding":1,"requirement":"Missing evidence cannot prove ready",
            "fixture":{},"inputs":[{}],"injected_behavior":["missing evidence"],
            "expected":{"ready":false},"observability":"independent response"
        });
        let normal: HttpFixture = serde_json::from_value(normal).unwrap();
        let known: HttpFixture = serde_json::from_value(regression.clone()).unwrap();
        assert_eq!(known.validate(), Ok(1));
        let fixtures = [normal, known];
        let evidence = |status: &str, passed: Value, failed: Value| {
            format!(
                "{}\n{}",
                json!({"id":"normal","status":"pass","passed":["assertion"]}),
                json!({"id":"regression","status":status,"passed":passed,"failed":failed})
            )
        };
        assert_eq!(
            validate_parity_data(
                "check",
                &evidence("known_external_bug", json!([]), json!(["assertion"])),
                &fixtures
            ),
            Ok(1)
        );
        assert_eq!(
            validate_parity_data(
                "check",
                &evidence("unsupported", json!([]), json!([])),
                &fixtures
            ),
            Ok(1)
        );
        for (status, passed, failed) in [
            ("pass", json!(["assertion"]), json!([])),
            ("known_external_bug", json!([]), json!(["different"])),
            ("unsupported", json!(["assertion"]), json!([])),
        ] {
            assert!(
                validate_parity_data("check", &evidence(status, passed, failed), &fixtures)
                    .is_err()
            );
        }
        for (field, value) in [
            ("known_external_bug", json!(false)),
            ("expected_external_failures", json!([])),
            ("expected_external_failures", json!(["missing-assertion"])),
            ("review", Value::Null),
        ] {
            let mut invalid = regression.clone();
            invalid[field] = value;
            let invalid: HttpFixture = serde_json::from_value(invalid).unwrap();
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn scripted_accepted_then_lost_response_reconciles_without_second_grab() {
        let mut upstream = ScriptedUpstream::new([
            ScriptedCall {
                operation: "grab".into(),
                reply: Reply::AcceptedThenLost(9),
            },
            ScriptedCall {
                operation: "queue-and-history".into(),
                reply: Reply::Success(9),
            },
        ]);
        let job = FixJob::new(
            1,
            2,
            FileIdentity { id: 3, revision: 1 },
            Constraints::default(),
            Policy {
                max_grabs: 2,
                stage_timeout: 10,
                total_timeout: 100,
            },
            0,
        )
        .unwrap();
        let job = job
            .apply(
                Event::SelectRelease {
                    release: 4,
                    access: Access::Granted {
                        library: 2,
                        expires_at: 100,
                    },
                },
                1,
            )
            .unwrap()
            .job;
        assert!(upstream.call("grab").is_err());
        let job = job
            .apply(
                Event::Observed {
                    intent: IntentKey {
                        job: 1,
                        sequence: 1,
                    },
                    observation: Observation::GrabAmbiguous,
                },
                2,
            )
            .unwrap()
            .job;
        let download = upstream.call("queue-and-history").unwrap();
        let transition = job
            .apply(
                Event::Observed {
                    intent: IntentKey {
                        job: 1,
                        sequence: 2,
                    },
                    observation: Observation::GrabReconciled(GrabResolution::Accepted { download }),
                },
                3,
            )
            .unwrap();
        assert_eq!(transition.job.state(), State::Downloading);
        assert_eq!(transition.job.reserved_budget(), 1);
        assert_eq!(transition.job.active_grabs(), 1);
        assert!(transition.intents.is_empty());
        assert!(upstream.is_exhausted());
        assert_eq!(upstream.calls, ["grab", "queue-and-history"]);
    }

    #[test]
    fn checked_in_scenarios_validate_and_domain_scenarios_execute() {
        check_suite(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios")).unwrap();
    }
}
