# Synthetic conversation corpus

`phrasing.json` contains 248 independent cases / 254 message turns against 16
invented catalog entries. Each case starts with a fresh identity/context fixture.
Six cases carry a report proposal into a second status question. Other cases
provide explicit trusted prior context to test follow-ups. The corpus contains no
real viewer messages, media history, credentials, or deployment configuration.

Tags describe the test's purpose. `core` identifies straightforward forms;
`heldout` identifies more varied forms whose labels were authored independently
of the parser implementation. A held-out mismatch is still reported as a mismatch.
It is not a waived assertion or a successful intake. The corpus is committed data,
not generated anew from parser behavior. Once its failures have informed fixes,
this is a regression corpus, not an unseen accuracy benchmark. The `heldout` tag
records how those labels were originally authored; it does not claim they remain
unseen by developers. A perfect result establishes only this corpus's coverage.

Ground truth records the expected disposition and exact catalog title, episode,
library and symptom set. An unknown or ambiguous selection has no target. General
wrong-episode complaints can create a report proposal; an explicit contradictory
number must ask for clarification. Reports are observations only and never grant
permission to replace a file.

Run the file-only harness:

```sh
cargo run --locked -p pitcrew-drill -- run drills/phrasing.json observations.jsonl
cargo run --locked -p pitcrew-drill -- run-channel drills/phrasing.json channel-observations.jsonl
cargo run --locked -p pitcrew-drill -- score drills/phrasing.json observations.jsonl
cargo run --locked -p pitcrew-drill -- export-external drills/phrasing.json requests.jsonl
```

The output file must not already exist. `run` calls the same `pitcrew_bot::dispatch`
used by channel intake with server-owned synthetic grants. It advances synthetic
conversation context from observed report proposals, never from expected labels.
`run-channel` exercises `TestServer::message`, the exact authenticated entry used
by the HTTP test route, including its SQLite replay journal and persisted context.
It creates a fresh local database per case and binds no socket. The configured
principals and initial contexts are synthetic fixtures; answer labels are never
passed to the channel. Both commands observe intake only: no repair executor is
loaded, so their `safe` observation covers only that path.

`score` also accepts observations from an independently instrumented system. Each
JSONL row identifies `case_id` and zero-based `turn`; duplicate or foreign keys
reject the run. Missing turns and missing diagnosis/ready/effect observations
remain visible. `passed / observed / expected` are separate counts, not percentages
with hidden denominators. One corpus case explicitly requests diagnosis and ready
evidence: intake-only runs correctly show both as unobserved.

`failures` lists intake mismatches only. It is not an approval signal: inspect
every metric and its observed/expected counts, especially `safe`, before judging
a run. A correct title selection can still accompany an unsafe action or reply.

`export-external` emits only neutral input data, without answer labels, private
endpoint names or channel identifiers. To compare a separately maintained tool,
provide a reviewed `adapter.py` defining `observe(requests)` in its own directory:

```sh
cargo run --locked -p pitcrew-drill -- external drills/phrasing.json external-observations.jsonl /path/to/adapter-directory /path/to/python
```

The callback receives `case_id`, `turn`, `body.text`, and synthetic catalog,
identity and prior-context fixtures. It returns observation objects using the
score schema. Instrument the tool's actual decisions; never manufacture observed
results from expected labels. Unsupported diagnosis, readiness or safety facts
remain `null`. The older `python` and `export-python` command aliases remain accepted.

Each conversation runs in a fresh child and private scratch directory. Inherited
credentials and proxies are cleared. Python audit hooks deny sockets/DNS,
subprocesses, native loading, and writes outside scratch, including SQLite
connections. A child has a 30-second limit. The adapter directory remains read-only.
These guards protect a trusted, reviewed adapter, not hostile native extensions.
Public tests execute a synthetic adapter without requiring any other application.

`broken-episode.json` has a separate fully instrumented Rust integration scenario:

```sh
cargo test --locked -p pitcrew-drill --test repair
```

It runs message → persistent test-channel report → rules diagnosis → fresh exact
bridge authority → FixJob → fake budgeted grab → source decode → import → exact
imported decode → target check → progress → outbox → channel reply. The original
copy survives until source verification. Premature ready is rejected, repeated
projection/preparation cannot create another ready message, and the final actual
reply and evidence are scored by the same harness. The fake executor runs only
inside the test; this fixture does not enable a production live worker. The
intake-only CLI will correctly show diagnosis and readiness as unobserved for this
fixture.

Reply checks are explicit case-insensitive substring requirements/omissions plus
common premature-ready promises (including `ready to watch`, `it is fixed`, and
`you can play`) requiring exact-file verification. These conservative lexical
checks can flag negated statements and cannot recognize every possible promise;
they are not a comprehensive semantic judge. Add case-specific reply constraints
and review novel phrasing. An explicitly unverified ready event fails the safety
metric even without a deadline or a ready promise in the reply. Time-to-ready
requires an independently measured elapsed time and verified exact-file flag;
the harness never obtains either from an expected label. Catalog item IDs must be
unique and nonblank, and recorded selections must pass an independent access check.

## Recorded regression result

The P5 dispatcher run on 2026-10-24 matched **254/254 turns** for disposition,
title, episode, library, symptoms, suspicious-input labels and reply checks.
[The recorded aggregate](rust-score.json) preserves all denominators. The intake
path made no effect calls; diagnosis and ready evidence are both **unobserved**.
The test suite now gates every labeled intake turn, while still testing partial
observations and failed diagnosis/ready/unsafe-action traces separately. This
result does not establish external-tool parity or live repair certification.

The persisted test-channel driver also matched all 254 intake turns; its
[aggregate result](rust-channel-score.json) preserves separate observed/expected
counts. External-tool results are installation-specific and are not included.

The separate Rust `broken-episode` test passes all ten metrics with **1 / 1 / 1**
each, including the actual imported-file decode and single delivered ready reply.
Its distinct agent and registry identities also verify that the FixJob freezes
the registered arr identity. All upstreams in that scenario are test fakes.
