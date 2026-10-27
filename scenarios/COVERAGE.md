# Executable coverage and its limits

Public CI does not require another help desk application or a historical deployment.
All fixtures are synthetic. Validating the optional external HTTP contract format
does not execute an HTTP application or establish parity with one.

| Behavior | Executable coverage |
| --- | --- |
| Lifecycle state, budget reservations, replacement-before-removal, exact-file readiness, retries and deadlines | `scenarios/domain/*.json`, `crates/domain`, `crates/engine/tests` |
| Accepted request followed by lost response and reconciliation | `crates/scenarios/src/lib.rs`, engine fault and restart tests |
| Assertion matching; missing/duplicate baseline checks and skipped assertion evidence | Synthetic unit tests in `crates/scenarios/src/lib.rs` |
| External source export and selected-class isolation | Four standard-library Python tests in `crates/scenarios/python` |
| Ten dynamic-range/library playback cases and two wizard routes | `crates/diagnosis/tests/scenario_parity.rs`, with standalone synthetic inputs in `tests/fixtures` |
| Authenticated intake, report persistence and viewer progress | `bins/pitcrew-web/tests`, `crates/intake-store/tests`, `crates/bridge/tests` |
| Offline conversation corpus and complete fake repair | `crates/drill/tests`, [drill corpus](../drills/README.md) |
| Actor correlation, unsafe actions and incomplete evidence | `crates/engine/tests/observer.rs`, `bins/pitcrew/tests/review.rs` |
| Read-only external SQLite snapshot, immutable source and restart deduplication | `bins/pitcrew/tests/python.rs` |

Optional separately supplied contracts can be validated with `check DIRECTORY` or
by setting `PITCREW_EXTERNAL_SCENARIOS` for the optional scenario test. An unset
variable skips only that optional suite; the public reducer, assertions, adapters,
and integration tests always run. Live API compatibility and real media repair
still require the [certification checklist](../docs/live-certification.md).
