# Pit Crew — agent instructions

Pit Crew is an open-source, self-hosted help desk for a Plex server: viewers report a broken title (web page,
Discord DM, Seerr issue), Pit Crew diagnoses it with evidence, swaps in a verified working copy through
Sonarr/Radarr, and tells the viewer when it's fixed. Rust workspace, dual-licensed MIT OR Apache-2.0.

This file is public. Never put private deployment details, hostnames, IPs, API keys, tokens, Discord ids, real
viewer names or viewing history in it, or anywhere else that gets committed.

## Ground rules

- **Safety rails are product requirements, not suggestions.** A library file is never removed before a verified
  replacement exists. No arr "mark failed" auto-redownload loops. Every re-download spends a budget that is checked
  and reserved before the request. Every terminal failure notifies the admin exactly once. "Ready" is only ever
  said after the exact imported file passed a decode test. Unknown access never authorizes a mutation.
- **Every fix is a `FixJob` with an explicit state.** Write the intended transition before the side effect;
  reconcile ambiguous upstream results instead of guessing. No hidden state in ad-hoc key/value flags.
- **Every integration is optional.** The minimum install (Plex + one Sonarr or Radarr + rules, no AI key) must work.
- **Privacy by default.** Anything sent to a cloud model goes through redaction (no usernames, tokens, ids, IPs,
  file paths). Viewer text is untrusted input.
- **Tests first for bugs.** Reproduce as a failing scenario or test, then fix.

## Test bar (CI must enforce all of it)

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (unit + scenarios + property tests),
and mutation testing (`cargo-mutants`) on the engine and arbiter crates. Upstreams are faked in the `fakes` crate;
tests never need a real media stack, network, or secrets. Fault injection (timeouts, 5xx, success-then-lost-response,
process kill and resume) belongs in the engine tests.

## Repo conventions

- Commits as `storybored379 <storybored379@users.noreply.github.com>` (set repo-locally). Small, focused commits.
- Architecture decisions go in `docs/adr/NNNN-title.md`.
- Mock/fake code lives only in `fakes` and test code, never in production crates.
- Don't push, publish crates, or publish images unless the maintainer asks.
