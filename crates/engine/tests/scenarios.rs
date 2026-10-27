//! Every checked-in domain scenario also executes through SQLite and Worker.
use pitcrew_domain::{Intent, StageTimeouts};
use pitcrew_engine::{CallFuture, Options, Upstream, Worker};
use pitcrew_scenarios::DomainFixture;
use pitcrew_store::*;
struct NoEffects;
impl Upstream for NoEffects {
    fn read<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        panic!("shadow read effect");
    }
    fn write<'a>(&'a self, _: &'a Intent) -> CallFuture<'a> {
        panic!("shadow write effect");
    }
}
#[tokio::test]
async fn every_domain_scenario_runs_through_store_and_shadow_worker() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/domain");
    let mut count = 0;
    for path in std::fs::read_dir(root).unwrap() {
        let fixture: DomainFixture =
            serde_json::from_slice(&std::fs::read(path.unwrap().path()).unwrap()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("db");
        let store = Store::open(&db).await.unwrap();
        let i = fixture.initial;
        let timeout = i.policy.stage_timeout;
        let spec = NewJob {
            id: i.id,
            viewer: "fixture-viewer".into(),
            resource: "fixture-media".into(),
            library: i.library,
            original: i.original,
            constraints: i.constraints,
            policy: i.policy,
            timeouts: StageTimeouts {
                download: timeout,
                import: timeout,
                decode: timeout,
                verify_retry: timeout,
            },
            created_at: i.now,
        };
        store
            .create_job(
                &spec,
                "fixture",
                BudgetLimits {
                    global_hour: 100,
                    global_day: 100,
                    viewer_hour: 100,
                    viewer_day: 100,
                },
            )
            .await
            .unwrap();
        let lease = store
            .acquire_lease(i.id, "fixture-worker", i.now, 1_000_000)
            .await
            .unwrap();
        let worker = Worker::new(Some(store.clone()), NoEffects, Options::default()).unwrap();
        for (index, step) in fixture.steps.into_iter().enumerate() {
            let result = worker
                .apply(&lease, &format!("step:{index}"), step.event, step.at)
                .await;
            let intents = if step.rejected {
                assert!(result.is_err(), "{} step {index}", fixture.id);
                vec![]
            } else {
                result.unwrap().intents
            };
            let effects: Vec<_> = intents
                .iter()
                .map(|intent| serde_json::to_value(&intent.effect).unwrap())
                .collect();
            let names: Vec<_> = effects
                .iter()
                .map(|value| value.as_object().unwrap().keys().next().unwrap().clone())
                .collect();
            assert_eq!(names, step.effects, "{} step {index}", fixture.id);
            if let Some(expected) = step.effect_payloads {
                assert_eq!(effects, expected);
            }
            let actual = store.load_job(i.id).await.unwrap().job;
            assert_eq!(actual.state(), step.state, "{} step {index}", fixture.id);
            assert_eq!(actual.reserved_budget(), step.reserved_budget);
            assert_eq!(actual.active_grabs(), step.active_grabs);
            // Rejected backwards-clock events must not be used as a worker clock.
            if !step.rejected {
                worker.drive(&lease, step.at).await.unwrap();
            }
        }
        assert_eq!(
            store.load_job(i.id).await.unwrap(),
            Store::open(&db)
                .await
                .unwrap()
                .load_job(i.id)
                .await
                .unwrap()
        );
        count += 1;
    }
    assert_eq!(count, 12);
}
