# Live repair certification

P3 supplies an offline-tested effect router, durable policy APIs, and replay import.
**The shipped executable still refuses live mode.** No certificate is included, and
passing CI does not authorize a deployment or a real repair. The remaining runtime
integration and owner-run checks below must precede enabling any live executor.

## Implemented boundaries

- `engine::live::Router` executes committed grabs, queue blocklisting without
  redownload, ManualImport through the replacement arbiter, and exact-file agent
  decoding. Reconciliation uses correlated queue/history evidence. A missing queue
  row alone never proves a mutation absent. No library-file DELETE is routed.
- `engine::target::TargetVerifier` verifies the exact Plex item, unique media/part,
  path, fresh agent identity and configured language/SDR constraints. Ambiguous
  versions, missing facts and stale/future evidence hold the job. `NotifyReady`
  is created only after the reducer accepts imported decode and target evidence.
- `engine::delivery::Notifications` uses configured `Notifier` and `Channel`
  adapters with immutable store payloads and the existing intent outbox. A process
  death after send leaves delivery uncertain. No uncertain message automatically
  resends, including terminal admin alerts. Admin resend reuses the same logical
  key, destination and text and records the administrator's note.
- `store::policy` persists item cooldowns, quarantined/dead-release exclusions,
  file identities and per-client capabilities. Exclusions retain release, reason
  and evidence, expire after 30 days unless pinned, and require an audited admin
  note for early release. Identifier aliases exclude the same release across jobs.
- `store::create_job_with_cooldown` reserves cooldown, ownership and budget in one
  transaction. A failed reservation commits none of them. Replay uses the existing
  zero-duration wrapper; it still cannot bypass an existing nonzero cooldown.
- The file registry combines arr file ID, size, dateAdded, quality revision and the
  agent's device/inode/metadata identity. Changed tuples mint a new opaque revision;
  returning to an old tuple does not resurrect its old identity. Mutation checks
  require a fresh registered revision and re-read evidence for the current item.
- The external snapshot importer reads a standalone SQLite backup without executing its source app,
  opening its HTTP API, or resolving upstream credentials. See [shadow.md](shadow.md).
  It records explicit report/attempt ownership and timestamped audit/budget facts.
  The historical schema lacks access grants and exact byte revisions: those facts
  remain unknown and the Rust proposal remains Hold, rather than fabricated parity.

## Version contracts and certification files

Synthetic read contracts target **Sonarr 4.0.19.2979** and **Radarr 6.3.0.10514**:

- `crates/arr/tests/fixtures/sonarr-4.0.19.2979.json`
- `crates/arr/tests/fixtures/radarr-6.3.0.10514.json`

Their field shapes were checked against recorded read-only responses for those
versions. Every title, identifier and path in the committed fixtures is invented.
They cover status/version, current-file lookup, file metadata, queue, history and
blocklist reads. A Sonarr queue sample was empty; populated queue and ManualImport
shapes are synthetic contract assumptions tested separately, not recorded proof.
Sonarr failed-history fields may be null; imported file IDs and release indexer IDs
in history data are strings. Radarr queue byte counts may be JSON integers.

These fixtures do **not** prove live grab, blocklist, category or replacement
semantics. Certification is per configured client instance, including its exact
arr version and download-client settings. Replacing or upgrading a client requires
recertification; capabilities reset when the observed version changes.

`CertificationGate::from_files` loads a certification record and a separate owner
acknowledgement file. The configured certificate, record and acknowledgement must
agree in every field, explicit enablement must be true, and certification must be
current. Media writes also require a matching version read from arr. Notifications
use the local gate so an arr outage does not prevent a terminal failure alert.
The expected record shape is:

```json
{
  "id": "owner-assigned-certification-id",
  "client": "configured-client-id",
  "kind": "Sonarr",
  "version": "4.0.19.2979",
  "fixture_digest": "sha256-of-reviewed-contract-fixtures",
  "certified_at": 0,
  "expires_at": 1
}
```

The times above are deliberately expired placeholders. The separate acknowledgement
contains `{"certification": <the exact record>, "acknowledge_live_mutations": true}`.
No code creates that acknowledgement on the owner's behalf. The digest identifies
reviewed evidence; the current library gate compares the three records, so runtime
bootstrap must compute and validate the digest against its installed fixtures.
Never copy credentials or personal activity into certification records committed
to the public repository. Store deployment records privately with owner-controlled
file permissions.

## Administrative API

The shadow service can expose local policy administration independently of effects.
Set a **separate** optional `engine.admin_secret` reference, for example:

```toml
[engine]
mode = "shadow"
admin_secret = { env = "PITCREW_ADMIN_SECRET" }
```

The normal webhook/replay secrets do not grant this role. The service authenticates
the admin credential before processing payloads. Roles/actors cannot be supplied in
request bodies; the configured admin principal and a nonempty note enter the audit.
The credential must be at least 32 visible ASCII characters, as with other control
secrets. Supply it as `Authorization: Bearer <secret>` over protected local ingress.

| Method / path | Body or result |
| --- | --- |
| GET `/v1/admin/exclusions/<client-id>` | Active exclusions, release identity, reason, evidence, expiry/pin state |
| POST `/v1/admin/exclusions/release` | `{"id":1,"note":"reason for release"}` |
| GET `/v1/admin/capabilities/<client-id>` | Current version-bound ledger, or null |
| POST `/v1/admin/capabilities/reset` | `{"client":"client-id","note":"reason for reset"}` |
| GET `/v1/admin/deliveries/<job-id>` | Notification intents and statuses, including `uncertain` |
| POST `/v1/admin/notifications/resend` | `{"job":1,"sequence":2,"request_key":"unique-request","note":"reason for resend"}` |

A repeated resend request key does not schedule a second retry. A conflicting key
returns 409. A pending, completed or unprepared notification cannot be rescheduled.
In the shipped shadow executable, scheduling a resend only changes durable state;
it cannot send a real message. The certified live worker must later consume it.
Use opaque client IDs without URL delimiters for these local routes. The
[owner desk](admin.md) exposes supported actions and saved evidence.

## Completion races and retry policy

Persist observed external queue interference in the version-bound capability ledger
before choosing an import policy. A clean subsequent download must not erase the
downgrade; only audited admin reset or a version change permits recertification.
A missing/cleared ledger blocks media writes. An external actor can remove completed
downloads within seconds, so never infer success or failure from queue disappearance.
The router checks ledger presence, version and revocation; the disabled runtime must
still select an import policy from the recorded external-actor/category capabilities.

Replacement proposals also reject sample/NCOP/NCED basenames and unknown or sub-70-MiB
source sizes. This conservative completeness check also holds otherwise valid tiny
movies/shorts until a richer completeness proof is implemented.

The implemented ManualImport route submits immediately after exact source verification
and final identity/access checks. It adds no delay or category round trip. It never
imports unverified bytes over a library copy. This is not a certified completion-to-import
deadline or proof of winning a race with an external remover. Independent arr auto-import observations
can enter the reducer's verify-after-import path and still require exact destination
decode before Ready. A first bad decode enters `VerifyRetry`; configure a 180-second
settling delay. The retry state and exact file identity survive restart. No extra grab,
blocklist or ready message is emitted during that delay. A repeated bad result excludes
the candidate within that job and allows only another budgeted attempt; the total deadline
still applies. Promoting that imported-decode failure to a cross-job exclusion remains
part of the deferred policy orchestration below.

Held-category submission and unattended-upgrade guards remain unsupported: neither is
certified by the recorded read responses, and the current release API does not choose
a per-submission category. Lower-quality upgrades therefore fail closed. A future
immediate-import route must prove preservation of the old copy until replacement
verification; racing an external remover is not permission to bypass that requirement.

## Remaining integration before certification

1. Compose `Router<Arr, Agent, TargetVerifier<Plex, Agent, Notifications>>` in a
   deliberately enabled runtime after validating all files/configuration. Keep the
   current executable refusal until this bootstrap is independently reviewed.
2. Connect authenticated intake to nonzero `create_job_with_cooldown`, access refresh,
   trusted per-intent `live::bind` / `target::bind`, imported registry updates, immutable
   notification preparation and continuous worker scheduling. The library APIs and
   fake integration tests exist; autonomous live discovery/enrichment is not wired.
   Raw webhooks and legacy rows must not manufacture missing access or byte identity.
3. Feed authoritative client interference and all dead-release observations into the
   policy APIs. The ledger and cross-job checks are durable, but polling-to-live-policy
   orchestration is part of the disabled bootstrap. Admin reset must trigger a new
   evidence review, not silently recertify a client.
4. Implement and separately certify held-category/preserve-old-copy and upgrade-guard
   transports if those policies are required. Until then, restrict canaries to supported
   exact single-file flows and conservative observation of independent auto-imports.
5. Supply the channel/notifier transports from the channel lane and verify physical
   delivery behavior. The engine already uses their traits/outbox and fakes. The admin
   API and policy/delivery UI are implemented. Runtime consumption and
   version-specific physical delivery checks remain certification prerequisites.
6. Certify evidence for requested constraints. The P1 agent may return unknown dynamic
   range; an SDR requirement stays blocked until a certified collector supplies it.
   Plex multi-version/multipart selection is intentionally held pending explicit support.

## Owner-run certification checklist

Do not run these steps merely because this file exists. They require an authorized
maintenance window and the owner's explicit live-test approval.

- Record exact versions/settings and fixture digests for each client. Preserve backups
  of the database/configuration and a known-good rollback binary; rehearse rollback.
- Run `cargo xtask ci` at the reviewed commit. It enforces formatting, all-target clippy,
  workspace/scenario/property tests and zero missed mutants across domain, agent,
  arbiter, engine, store, arr and the control-plane executable.
- Run at least seven days of shadow comparison. Review missing-evidence cases separately
  from agreement; require zero unsafe divergences. Enrich historical snapshots with
  authorized access and file evidence before claiming full external-tool/engine action parity.
- On disposable, owner-approved media, prove original-file preservation, exact source
  and destination checks, byte replacement at the same path, ambiguous/lost responses,
  external queue removal, cooldown/budget contention, restart and fresh-import settling.
- For each version, inspect the actual grab and ManualImport requests/results; confirm
  queue blocklisting uses `removeFromClient=false`, `blocklist=true`,
  `skipRedownload=true`, `changeCategory=false`. No mark-failed or unbudgeted search loop.
- Verify unknown access, stale revisions, expired certification, mismatched acknowledgement,
  version changes and reset capabilities all block mutation. Exercise admin release with
  a note and expiry/pinning across restart.
- Lose a ready receipt after acceptance. Confirm the job is visibly delivery-uncertain,
  no automatic duplicate occurs, and an explicitly audited resend uses the same payload.
  Confirm each terminal failure creates one admin notification intent.
- Review actual actor/settings evidence for held versus verify-after-import policy, then
  record the capability ledger. Start with the owner's own reports. Expand only after
  the owner reviews the canary results and writes the acknowledgement file.

Offline tests use fake transports, temporary databases and owned test processes only.
They do not contact or certify a real media stack, notifier or channel.

### Runtime integration update

The supervised **shadow** composition is now implemented: catalog/access refresh,
report diagnosis, independent bridge registry/authority, nonzero cooldown/budget
reservation, conservative opt-in same-quality candidate selection, continuous shadow
worker scheduling, viewer progress and saved admin health/comparison/usage producers.
The admin command consumer and durable working/late-join notification journal are
also wired; shadow delivery records would-send without contacting an adapter.
Exact-source dynamic-range parsing and the no-Dolby-Vision-profile-5 constraint now
cover source/import and destination verification. Multipart/multiversion stays held.
These facts supersede the corresponding shadow-only items in the list above.

The executable **still refuses live mode**. Config-only autonomous live bootstrap
is explicitly deferred: trusted per-intent release/download/import route binding,
crash-safe queue/history correlation, imported-file registry/Plex target enrichment,
access refresh on subsequent transitions, certified capability evidence loading and
notification transport composition are not yet complete. The read-only shadow
candidate selector is not authority to execute those missing steps. Existing fake
Router request-shape tests are library proofs, not evidence of a running live stack.
No acknowledgement, certificate or capability record is manufactured by bootstrap.
