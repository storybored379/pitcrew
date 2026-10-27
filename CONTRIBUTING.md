# Contributing

Pit Crew is pre-alpha. Start with a small, reproducible change and keep the safety
rails in [AGENTS.md](AGENTS.md) intact. Contributions are dual licensed
MIT OR Apache-2.0. Do not publish packages or images without the maintainer's
approval.

## Development setup

Install rustup, Git, Python 3, and ffmpeg (including ffprobe). The checked-in
`rust-toolchain.toml` selects the Rust version, rustfmt, and clippy. Keep
`Cargo.lock` committed. Tests generate their own media fixtures and use temporary
SQLite databases or owned loopback fake servers; no media stack or real
credentials are required.

```sh
cargo fetch --locked
cargo xtask check
```

`check` runs the privacy gate, formatting, all-target clippy with warnings denied,
the complete workspace test suite, synthetic scenario checks, and Python helper
tests. For focused iteration:

```sh
cargo test --locked -p pitcrew-engine
cargo run --locked -p pitcrew-scenarios -- check
cargo xtask privacy
```

After dependencies are cached, set `CARGO_NET_OFFLINE=true` to prohibit Cargo
network access. `CARGO_BUILD_JOBS=4`, `CARGO_PROFILE_DEV_DEBUG=0`, and
`CARGO_PROFILE_TEST_DEBUG=0` reduce shared-host resource use.

## Mutation gates and CI

Install the pinned runner and run the complete gate before declaring a release
candidate ready:

```sh
cargo install cargo-mutants --version 27.1.0 --locked
cargo xtask ci
```

`ci` runs `check`, then all mutation gates. Every generated mutant must be caught
or unviable; missed mutants, timeouts, missing tools, and failed baselines fail
the command. The domain gate includes scenario coverage. Other gates cover the
agent, arbiter, engine, store, arr adapter, control plane, diagnosis, deciders,
intake, intake store, web, bot, drill, sync, bridge, admin, and setup packages.
Source-specific groups use their dedicated contract suites; the remaining source
still runs through the package gate. There are no silently omitted sources.

Run a focused package gate without changing the complete gate:

```sh
cargo xtask mutants pitcrew-engine
```

GitHub push and pull-request workflows run `check`. Weekly and manually dispatched
workflows also run every mutation package, with separate evidence artifacts.
A green pull-request check alone is not proof that mutation gates passed. The
workflow matrix and xtask package list must stay in sync when adding a gate.

## Tests and safety

Begin bug fixes with a failing test or scenario, fix the smallest relevant
boundary, then rerun the reproduction. Add property or fault tests when ordering,
retries, crashes, budgets, or exact identity matter. Test fakes belong in `fakes`
or test code, never in production branches. A pure reducer test does not prove
HTTP behavior or durable database commits: preserve the integration, crash/resume,
concurrency, contract, and outbox tests at those boundaries.

Write effects behind committed intents. Revalidate access, budgets, constraints,
and exact media identity across retries. Provider suggestions cannot override
rails. Never delete a library file to force an upgrade, and never claim ready
before the exact imported bytes pass verification. Record architectural decisions
in `docs/adr/` and distinguish implemented behavior from planned integration.

## Public-tree privacy

Use synthetic titles, account IDs, endpoints, and fixtures. Never commit private
captures, credentials, local deployment names, or viewer history. Run
`cargo xtask privacy` before committing. It scans tracked files and nonignored
new files, including binary contents and symlink text. It does not follow symlinks.
Deleted working-tree files are skipped; review the staged diff before committing.

Built-in rules reject home-directory paths, non-noreply emails, common credential
formats and long literal credential assignments, and non-documentation IP
addresses. Credential assignments include quoted configuration values and common
unquoted env/YAML tokens, including service-prefixed keys. Shell/template
references, `env:`/`file:` references, and obvious placeholders are accepted.
Use RFC 5737 IPv4 or `2001:db8::/32` examples. Loopback and unspecified
bind addresses are accepted. Tests that must exercise rejected private values
should construct synthetic values at runtime. Do not add a blanket scanner
exception for a test directory.

For deployment-specific names, create an untracked, ignored `.privacy-denylist`
at the repository root. Put one literal substring per line; blank lines and lines
starting with `#` are ignored. Matching is case-sensitive. The scanner refuses a
tracked or unignored denylist and reports only file, line, and rule names, never
the matched text. The generic gate runs without this file in public CI. Pattern
checks cannot recognize every secret format or personal name, so review the diff
as well. Report security problems through [SECURITY.md](SECURITY.md).
