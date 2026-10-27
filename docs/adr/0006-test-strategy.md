# ADR 0006: Prove behavior with fixtures, properties and fault injection

Status: accepted. Date: 2026-10-23. Public fixture scope clarified 2026-10-26.

Happy-path mock checks do not establish safety under ambiguous responses, stale
evidence, retries or process interruption. Use independent expected behavior,
explicit fault scripts and honest coverage boundaries.

Versioned JSON lifecycle fixtures execute the real Rust `domain` reducer. Tests
compare states, effects, payloads, budgets and active-grab counts. Engine tests
execute the same fixtures with the transactional SQLite store and worker. An
accepted grab with a lost response remains observable in upstream history; the
worker must reconcile it rather than submit another mutation. Never implement a
fake application by returning a fixture's expected answer.

Public HTTP contracts are synthetic examples of a validated schema, not measured
application behavior. Optional external suites require their own reviewed adapter
and independent observations. Exact expected failure IDs are required; setup
errors and unexpected failures cannot count as known failures. Missing
capabilities remain unsupported. A recorded result is distinct from execution.
No existing private application, raw incident history or deployment-specific
migration fixture is required by CI. [Coverage](../../scenarios/COVERAGE.md)
identifies the executable public tests and their limits.

Adapters run only against owned temporary state with synthetic clients. An
existing tool's HTTP GET route may perform mutations and is never assumed safe
merely because of its method. External drill adapters clear inherited credentials,
deny outbound I/O and restrict writes to scratch. The generic source export helper
exports committed regular files and directories only; dirty/untracked files and
unsafe links are rejected. Source extraction helpers do not execute unrelated
globals. These boundaries protect reviewed test source, not hostile native code.

The complete `cargo xtask ci` gate requires formatting, Clippy with warnings
denied, unit/integration/scenario/property tests, privacy checks and mutation
testing. Every generated mutant must be caught or unviable; no survivor is silently
waived. Fast hosted CI and scheduled/manual mutation jobs use the same gates.
See [contributing](../../CONTRIBUTING.md) for commands and prerequisites.

Fault tests cover exact import identity, ownership and budget races, slow/hung
decodes, and process interruption around committed intents. Outbox tests distinguish
one logical notification from physical delivery; exactly-once delivery cannot be
assumed without downstream idempotency. Agent tests use generated real media,
temporary symlinks, approved roots, explicit timezones, DST transitions and rotated
logs. Test-owned loopback listeners and scripted transports require no installed
media stack or service credentials.

Live shadow comparison, version-specific API certification and maintainer cutover
approval remain separate release gates. Local tests authorize no contact with or
changes to a media stack.
