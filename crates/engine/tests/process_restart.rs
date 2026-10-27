//! Abrupt process death at every *occurrence* of the worker's await boundaries.
//! All durable upstream facts live in this test's temporary directory.
use pitcrew_domain::{
    Access, Constraints, Effect, Event, Failure, FileIdentity, GrabResolution, Intent, Observation,
    Policy, StageTimeouts, State,
};
use pitcrew_engine::{Boundary, CallError, CallFuture, Mode, Options, Outcome, Upstream, Worker};
use pitcrew_store::{BudgetLimits, NewJob, OutboxStatus, Store};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const CHILD_MARKER: &str = "pitcrew-owned-process-crash-fixture-v1";
const EXIT_AT_BOUNDARY: i32 = 77;
const ROUTES: [&str; 4] = ["success", "lost-response", "shadow", "terminal-deadline"];

struct DiskFake {
    root: PathBuf,
    route: String,
}
impl DiskFake {
    fn record(&self, value: &str) {
        assert_ne!(self.route, "shadow", "shadow dispatched an upstream effect");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("calls"))
            .unwrap();
        writeln!(file, "{value}").unwrap();
        file.sync_all().unwrap();
    }
}
impl Upstream for DiskFake {
    fn read<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(async move {
            assert!(matches!(
                intent.effect,
                Effect::ReconcileGrab { release: 8 }
            ));
            self.record("reconcile");
            let resolution = if self.root.join("grab-accepted").exists() {
                GrabResolution::Accepted { download: 51 }
            } else {
                GrabResolution::Absent
            };
            Ok(Outcome::Observed(Observation::GrabReconciled(resolution)))
        })
    }
    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(async move {
            match intent.effect {
                Effect::Grab {
                    release: 8,
                    reservation: 1,
                } => {
                    self.record("grab");
                    // Do not deduplicate a second dispatch in the fake: this
                    // create-new marker makes an accidental duplicate fail.
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(self.root.join("grab-accepted"))
                        .expect("second grab reached the upstream");
                    file.sync_all().unwrap();
                    if self.route == "lost-response" {
                        Err(CallError::Timeout)
                    } else {
                        Ok(Outcome::Observed(Observation::GrabAccepted {
                            download: 51,
                        }))
                    }
                }
                Effect::NotifyAdmin {
                    failure: Failure::Deadline,
                } => {
                    self.record("admin");
                    Ok(Outcome::Delivered)
                }
                Effect::NotifyViewerFailure {
                    failure: Failure::Deadline,
                } => {
                    self.record("viewer");
                    Ok(Outcome::Delivered)
                }
                _ => panic!("unexpected effect in process fixture: {:?}", intent.effect),
            }
        })
    }
}

fn spec() -> NewJob {
    NewJob {
        id: 1,
        viewer: "process-viewer".into(),
        resource: "process-media".into(),
        library: 3,
        original: FileIdentity {
            id: 10,
            revision: 1,
        },
        constraints: Constraints::default(),
        policy: Policy {
            max_grabs: 2,
            stage_timeout: 100,
            total_timeout: 1000,
        },
        timeouts: StageTimeouts {
            download: 500,
            import: 200,
            decode: 100,
            verify_retry: 100,
        },
        created_at: 0,
    }
}
fn options(route: &str) -> Options {
    if route == "shadow" {
        Options::default()
    } else {
        Options {
            mode: Mode::Live,
            acknowledge_live_mutations: true,
            call_timeout: Duration::from_secs(1),
        }
    }
}
fn expiry(route: &str) -> u64 {
    if route == "terminal-deadline" {
        120
    } else {
        20
    }
}
async fn prepare(root: &Path, route: &str) {
    fs::write(root.join("owned.marker"), CHILD_MARKER).unwrap();
    let store = Store::open(root.join("db")).await.unwrap();
    store
        .create_job(
            &spec(),
            "create",
            BudgetLimits {
                global_hour: 10,
                global_day: 20,
                viewer_hour: 4,
                viewer_day: 8,
            },
        )
        .await
        .unwrap();
    let lease = store.acquire_lease(1, "prepare", 0, 1).await.unwrap();
    store
        .apply_event(
            &lease,
            0,
            "select",
            Event::SelectRelease {
                release: 8,
                access: Access::Granted {
                    library: 3,
                    expires_at: 900,
                },
            },
            0,
        )
        .await
        .unwrap();
    store.release_lease(&lease, 0).await.unwrap();
    assert!(ROUTES.contains(&route));
}

fn spawn_and_wait(root: &Path, route: &str, cut: usize) -> ExitStatus {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "engine_crash_child", "--nocapture"])
        .env("PITCREW_ENGINE_PROCESS_TEST", CHILD_MARKER)
        .env("PITCREW_ENGINE_PROCESS_TEST_ROOT", root)
        .env("PITCREW_ENGINE_PROCESS_TEST_ROUTE", route)
        .env("PITCREW_ENGINE_PROCESS_TEST_CUT", cut.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("process fixture hung: route={route}, occurrence={cut}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// With no explicit test-only marker this is an ordinary no-op test. In child
/// mode both the environment marker and parent-created fixture marker must match.
#[test]
fn engine_crash_child() {
    if std::env::var("PITCREW_ENGINE_PROCESS_TEST").as_deref() != Ok(CHILD_MARKER) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("PITCREW_ENGINE_PROCESS_TEST_ROOT").unwrap());
    assert_eq!(
        fs::read_to_string(root.join("owned.marker")).unwrap(),
        CHILD_MARKER
    );
    let route = std::env::var("PITCREW_ENGINE_PROCESS_TEST_ROUTE").unwrap();
    assert!(ROUTES.contains(&route.as_str()));
    let cut: usize = std::env::var("PITCREW_ENGINE_PROCESS_TEST_CUT")
        .unwrap()
        .parse()
        .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open(root.join("db")).await.unwrap();
            let lease = store
                .acquire_lease(1, "child", 0, expiry(&route))
                .await
                .unwrap();
            let worker = Worker::new(
                Some(store),
                DiskFake {
                    root: root.clone(),
                    route: route.clone(),
                },
                options(&route),
            )
            .unwrap();
            let mut trace = vec![];
            let first = if route == "terminal-deadline" { 100 } else { 1 };
            for now in [first, first + 1] {
                worker
                    .drive_with(&lease, now, |boundary| {
                        trace.push(format!("{boundary:?}"));
                        if cut != 0 && trace.len() == cut {
                            // Save the exact reached prefix before exiting without any
                            // Rust destructor, transaction drop or task cancellation.
                            fs::write(root.join("trace.json"), serde_json::to_vec(&trace).unwrap())
                                .unwrap();
                            std::process::exit(EXIT_AT_BOUNDARY);
                        }
                        std::future::ready(())
                    })
                    .await
                    .unwrap();
            }
            fs::write(root.join("trace.json"), serde_json::to_vec(&trace).unwrap()).unwrap();
        });
}

async fn verify_and_resume(root: &Path, route: &str) {
    let store = Store::open(root.join("db")).await.unwrap();
    let lease = store
        .acquire_lease(1, "restart", expiry(route), 1000)
        .await
        .unwrap();
    let worker = Worker::new(
        Some(store.clone()),
        DiskFake {
            root: root.into(),
            route: route.into(),
        },
        options(route),
    )
    .unwrap();
    // A recovered mutation first emits reconciliation; the next pass executes
    // its read. An extra pass proves the completed state is stable.
    for at in expiry(route)..expiry(route) + 3 {
        worker.drive(&lease, at).await.unwrap();
    }
    let calls = fs::read_to_string(root.join("calls")).unwrap_or_default();
    for operation in ["grab", "admin", "viewer"] {
        assert!(
            calls.lines().filter(|line| *line == operation).count() <= 1,
            "duplicate {operation} for {route}: {calls}"
        );
    }
    let job = store.load_job(1).await.unwrap().job;
    let rows = store.outbox(1).await.unwrap();
    assert_eq!(job.reserved_budget(), 1);
    match route {
        "shadow" => {
            assert!(calls.is_empty());
            assert_eq!(job.state(), State::GrabIntent);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].status, OutboxStatus::Shadowed);
            let observations = store.observations().await.unwrap();
            assert_eq!(observations.len(), 1);
            assert_eq!(observations[0].source, "engine_decision");
            assert_eq!(observations[0].key, "1:1");
        }
        "terminal-deadline" => {
            assert_eq!(job.failure(), Some(Failure::Deadline));
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].status, OutboxStatus::Suppressed);
            assert_eq!(
                rows.iter()
                    .filter(|row| matches!(row.intent.effect, Effect::NotifyAdmin { .. }))
                    .count(),
                1
            );
            assert_eq!(
                rows.iter()
                    .filter(|row| matches!(row.intent.effect, Effect::NotifyViewerFailure { .. }))
                    .count(),
                1
            );
            assert!(rows[1..].iter().all(|row| matches!(
                row.status,
                OutboxStatus::Completed | OutboxStatus::Uncertain
            )));
            assert!(!calls.lines().any(|line| line == "grab"));
        }
        "success" | "lost-response" => {
            if root.join("grab-accepted").exists() {
                assert_eq!(job.state(), State::Downloading);
                assert_eq!(calls.lines().filter(|line| *line == "grab").count(), 1);
            } else {
                assert_eq!(job.state(), State::Rejected);
                assert!(!calls.lines().any(|line| line == "grab"));
            }
            assert!(rows.iter().all(|row| row.status == OutboxStatus::Completed));
            assert!(job.unresolved_mutation().is_none());
        }
        _ => unreachable!(),
    }
}

#[test]
fn every_encountered_await_occurrence_survives_real_process_death() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut all_boundaries = BTreeSet::new();
    let mut total_cuts = 0;
    for route in ROUTES {
        let baseline = tempfile::tempdir().unwrap();
        runtime.block_on(prepare(baseline.path(), route));
        assert!(
            spawn_and_wait(baseline.path(), route, 0).success(),
            "baseline {route}"
        );
        let trace: Vec<String> =
            serde_json::from_slice(&fs::read(baseline.path().join("trace.json")).unwrap()).unwrap();
        assert!(!trace.is_empty());
        assert!(
            trace
                .iter()
                .filter(|name| name.as_str() == "LoadBefore")
                .count()
                >= 2
        );
        all_boundaries.extend(trace.iter().cloned());
        runtime.block_on(verify_and_resume(baseline.path(), route));
        for cut in 1..=trace.len() {
            let temp = tempfile::tempdir().unwrap();
            runtime.block_on(prepare(temp.path(), route));
            assert_eq!(
                spawn_and_wait(temp.path(), route, cut).code(),
                Some(EXIT_AT_BOUNDARY),
                "route {route}, occurrence {cut}"
            );
            let reached: Vec<String> =
                serde_json::from_slice(&fs::read(temp.path().join("trace.json")).unwrap()).unwrap();
            assert_eq!(reached, trace[..cut], "route {route}, occurrence {cut}");
            runtime.block_on(verify_and_resume(temp.path(), route));
            total_cuts += 1;
        }
    }
    let expected = [
        Boundary::LoadBefore,
        Boundary::LoadAfter,
        Boundary::TickBefore,
        Boundary::TickAfter,
        Boundary::OutboxBefore,
        Boundary::OutboxAfter,
        Boundary::ShadowBefore,
        Boundary::ShadowAfter,
        Boundary::StartBefore,
        Boundary::StartAfter,
        Boundary::CallBefore,
        Boundary::CallAfter,
        Boundary::ResultBefore,
        Boundary::ResultAfter,
        Boundary::FinishBefore,
        Boundary::FinishAfter,
    ]
    .map(|boundary| format!("{boundary:?}"))
    .into_iter()
    .collect();
    assert_eq!(
        all_boundaries, expected,
        "a worker await kind was never exercised"
    );
    assert!(
        total_cuts > all_boundaries.len(),
        "repeated occurrences were not enumerated"
    );
    eprintln!("verified {total_cuts} abrupt process-death cuts over all 16 hook kinds");
}
