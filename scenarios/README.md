# Acceptance scenarios

Run the public, offline suite:

```sh
cargo run --locked -p pitcrew-scenarios -- check
```

`domain/*.json` executes the real Rust lifecycle reducer, checking every returned
intent, state transition, budget and active-grab count. Engine integration tests
also run these same scenarios through a SQLite store and worker. They require no
external application, network access, or credentials.

`contracts/*.json` illustrates the optional external-tool HTTP contract format.
These files are **validated, not executed** by this command. Validation cannot
establish HTTP parity, a successful repair, or a measured external baseline.
See [coverage](COVERAGE.md) for executable Rust tests.

## External contracts

A version-1 contract has `id`, `adapter: "external_http"`, `description`, `checks`,
and ordered `steps`. Steps can be `login`, `request`, `capture`, or `assert`.
Requests require relative paths and bound repetition to 500. Selections refer to
an earlier observation with a JSON pointer; dangling observations, duplicate IDs,
unknown fields and transforms fail validation. Predicates include exact equality,
subset inclusion, length, numeric comparison and presence. Array subsets match
prefixes. No assertion executes code from fixture data.

Known external failures require an explicit review contract and exact assertion
IDs in `expected_external_failures`. A separately supplied inventory and result
file are checked together if present; missing, duplicate, skipped or unexpected
assertions fail the evidence check for executed cases. A known-failure case may
instead report `unsupported` with no executed assertions; it cannot claim source
coverage. Recorded evidence is distinct from execution.

To validate a separately maintained suite, use `check DIRECTORY`. To run a
reviewed local adapter, use `external --adapter PATH` followed by its arguments.
Without an adapter, optional replay prints a skip message and exits successfully.
An adapter is executable code: review its isolation before running it. Pit Crew
does not ship integration assumptions about another help desk's private API.

The Python helper tests in `crates/scenarios/python` retain generic isolation
coverage: committed-source export excludes dirty/untracked files and unsafe links;
AST extraction selects one named class without executing unrelated globals.
These helpers are for trusted source, not a sandbox for hostile native code.
