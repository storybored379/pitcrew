# Changelog

This project is pre-alpha. Entries describe source-tree capabilities; they do not
imply that a release, package, or container image has been published.

## 0.1.0 — Unreleased pre-alpha

- Rust workspace with a durable repair lifecycle, explicit intent journal,
  reconciliation, access checks, budgets, and exact-file verification.
- Plex and Sonarr/Radarr adapters, a bounded local media agent, report intake,
  diagnosis, shadow comparisons, and optional rules or cloud deciders.
- Viewer web, Discord bot, and owner desk surfaces with synthetic offline tests.
- Setup command, Docker Compose and systemd packaging, and configuration and
  certification guides for starting in shadow mode.
- Separate slim control/web/admin/bot image and FFmpeg agent image, with digest-pinned
  bases, fixed Debian package snapshots and an isolated Compose smoke check in CI.
- Opt-in model scheduling by diagnosis kind with bounded requests, saved usage,
  rules fallback and a rules-owned set of permitted recommendations.
- Default-off agent timeout confirmation: repeated failures on the same byte range
  require prompt uncached control reads before the local verdict can be `bad`.
- Public scenario fixtures, a repository privacy gate, push/PR checks, scheduled
  mutation gates, contribution templates, and private security-reporting guidance.

Live repairs require explicit installation gates and certification. See
[the README](README.md) and [certification checklist](docs/live-certification.md)
for current runtime limits; no production-readiness claim is made.
