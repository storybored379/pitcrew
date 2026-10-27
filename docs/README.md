# Documentation

Start with the [README quickstart](../README.md). These guides expand on the same
shared TOML configuration; optional processes stay disabled until configured.

## Install and operate

- [Setup wizard and scripted installs](setup.md): private files, flags and doctor.
- [Stack](stack.md): Compose, systemd, media paths and operational limits.
- [Agent](agent.md): bounded media reads, decode tests and optional operations.
- [Live certification](live-certification.md): required evidence, gates and unsupported flows.
- [Shadow comparisons](shadow.md): observed activity, conservative attribution and exports.

## Viewers and owners

- [Viewer web](web.md): Plex sign-in, report wizard and access boundaries.
- [Channels](bot.md): Discord, Telegram, Seerr and the local test channel.
- [Owner desk](admin.md): saved evidence, audited requests and delivery uncertainty.
- [Intake and drills](intake-drills.md): synthetic conversations and scoring.

## Integrations and design

- [Configuration reference](../config.example.toml): all optional integrations.
- [Upstreams](upstreams.md): API contracts and version assumptions.
- [Catalog and diagnosis pipeline](pipeline.md): identity, access and evidence collection.
- [Diagnosis](diagnosis.md): rules, optional providers and redaction.
- [Report-to-fix bridge](bridge.md): trusted authority, budgets and projected progress.
- [Architecture](../ARCHITECTURE.md) and [architectural decisions](adr/0001-architecture.md).
- [Contributing and checks](../CONTRIBUTING.md), [scenario coverage](../scenarios/COVERAGE.md),
  [security reporting](../SECURITY.md), and [changelog](../CHANGELOG.md).

Guides distinguish offline contract tests from live certification. Passing a
synthetic test never authorizes a real repair or proves another server's version
and download-client behavior.
