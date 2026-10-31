# Live repair certification

The repository includes offline-tested adapter composition, an effect router,
durable policy APIs and replay import. **The shipped executable still refuses live
mode, including with valid certificates.** Trusted grab ownership and subsequent
live orchestration remain missing. No certificate is included, and passing CI does
not authorize a deployment or a real repair. The remaining integration and
owner-run checks below must precede enabling any live executor.

## Implemented boundaries

- `pitcrew::live` validates local certification inputs and composes
  `Router<Arr, Agent, TargetVerifier<Plex, Agent, Notifications>>` without contacting
  an upstream. It returns no live worker or scheduler. Startup reports a specific
  missing prerequisite; after all local prerequisites pass, it logs
  `pitcrew_live_refused` with `effects_enabled=false` and refuses the still-missing
  ownership, access, binding and policy orchestration.
- `engine::live::Router` executes committed grabs, queue blocklisting without
  redownload, ManualImport through the replacement arbiter, and exact-file agent
  decoding. Reconciliation uses correlated queue/history evidence. A missing queue
  row alone never proves a mutation absent. No library-file DELETE is routed.
  A grab rechecks the installed original after release search. Import reconciliation
  matches history to one file record, then rechecks its path, full metadata tuple and
  installed-file ID before updating the registry; a changed file cannot inherit the
  earlier record's import evidence.
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

The pinned grab endpoints do not supply an ownership receipt: both await the
internal download service and return the submitted release object. See the
[Sonarr controller at 4.0.19.2979](https://github.com/Sonarr/Sonarr/blob/v4.0.19.2979/src/Sonarr.Api.V3/Indexers/ReleaseController.cs#L144-L152)
and [Radarr controller at 6.3.0.10514](https://github.com/Radarr/Radarr/blob/v6.3.0.10514/src/Radarr.Api.V3/Indexers/ReleaseController.cs#L105-L113).
A history row with the same release GUID/indexer does not establish which actor
submitted it; another actor may have grabbed that release. Media IDs, titles and
timing also cannot establish ownership. A configuration flag or certificate cannot
make these stock endpoints return a trusted download receipt. The router therefore
continues to report `GrabAmbiguous` after HTTP success and requires independently
trusted correlation before accepting a download as its own.

`[live]` defaults to `enabled=false`, alongside the existing shadow engine default.
Local validation requires `engine.mode="live"`,
`engine.acknowledge_live_mutations=true`, `live.enabled=true`, an available durable
engine store, `stack.enabled=true`, Plex, an agent, at least one arr client and
`runtime.search_enabled=true`. There must be exactly one certification for each
configured arr client. These prerequisites allow offline composition only; the
executable still refuses dispatch.

This illustrative configuration stays disabled and uses an expired certificate and
an invalid placeholder digest:

```toml
[live]
enabled = false

[[live.certifications]]
client = "sonarr-example"
id = "example-expired"
version = "4.0.19.2979"
fixture_digest = "0000000000000000000000000000000000000000000000000000000000000000"
certified_at = 0
expires_at = 1
certificate_file = "/config/live/sonarr-certificate.json"
acknowledgement_file = "/config/live/sonarr-acknowledgement.json"
capabilities_file = "/config/live/sonarr-capabilities.json"
```

The three files must be regular JSON files of at most 64 KiB; symlinks and special
files are refused. The certificate uses the configured arr kind and this shape:

```json
{
  "id": "example-expired",
  "client": "sonarr-example",
  "kind": "Sonarr",
  "version": "4.0.19.2979",
  "fixture_digest": "0000000000000000000000000000000000000000000000000000000000000000",
  "certified_at": 0,
  "expires_at": 1
}
```

The times above are deliberately expired placeholders. The separate acknowledgement
has `certification` containing the exact record and `acknowledge_live_mutations`
set to `true`. The capability file has the same `certification`, a `capabilities`
object containing `external_queue_actor` and `held_category`, and non-null `evidence`.
A non-null held category is refused because its transport is not implemented or
certified. Both capability values and evidence must match a pre-existing reviewed
ledger for the exact client version; a future-dated ledger is refused. Bootstrap
reads that ledger; it never creates it or restores a reset capability from a file.
A reset requires a new evidence review.

The configured certificate, record and acknowledgement must agree in every field,
and certification must be current. `fixture_digest` is the lowercase hexadecimal
SHA-256 of the exact bundled fixture file for the client's kind, without a prefix;
bootstrap computes and compares it. This binds the read-contract artifact, not proof
of write safety. Use the fixtures from the reviewed build when preparing evidence.
`CertificationGate::from_records` accepts these already-loaded records;
`from_files` remains available to library callers. Media writes also require a
matching version freshly read from arr. Notifications use the local gate so an arr
outage does not prevent a terminal failure alert.

No code creates an owner acknowledgement or certification. Never copy credentials
or personal activity into records committed to the public repository. Keep deployment
records private with owner-controlled file permissions.

## Delivery transports

Offline composition connects `engine::delivery::Notifications` to the real HTTP
transports. It requires exactly one admin notifier and at least one authenticated
viewer channel. Discord, Telegram and Seerr channel adapters are supported; the
web adapter continues to serve progress rather than an outbound viewer transport.
Email and Matrix channels, the email notifier and Pushover without a certified relay
receipt protocol produce explicit construction refusals.

The webhook notifier POSTs JSON `{key,title,body}`. A successful HTTP status alone
is insufficient: confirmation requires JSON with `accepted:true` and the exact
submitted `key`. The native ntfy notifier POSTs plain text to its configured topic
URL with a `Title` header; confirmation requires a message event, the same topic
and a nonempty message ID. Both send `X-PitCrew-Delivery-Key` containing URL-safe,
unpadded base64 of the serialized delivery key. Optional bearer credentials come
from secret references. Neither protocol promises physical deduplication.

The webhook and ntfy notifier transports disable redirects, ambient proxies and
automatic retries, and bound response sizes and timeouts. Channel adapters retain
their existing transport behavior. A timeout, lost response or invalid acknowledgement remains
uncertain; it cannot trigger an automatic duplicate. Loopback fakes exercise actual
HTTP request/receipt behavior and the immutable outbox. They do not certify any
installed platform or prove that a person received or read a message. Live bridge
preparation, lifecycle scheduling and platform-specific delivery certification remain
prerequisites below.

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

1. Implement a trusted grab ownership receipt producer and crash-safe correlation to
   the exact dispatched intent. The pinned stock release endpoints return no receipt;
   queue/history matching alone cannot fill that gap. Do not enable a worker that
   cannot safely reconcile its successful or lost-response grab.
2. Complete autonomous live scheduling from authenticated intake: refresh current
   access before every subsequent transition and at the final mutation boundary,
   bind each download/import intent through `live::bind`, update the imported-file
   registry and bind the exact Plex target through `target::bind`. The router currently checks
   the persisted grant's expiry, not a new access-provider response. Raw webhooks and
   legacy rows must not manufacture missing grants, ownership or byte identity.
3. Feed authoritative client interference and all dead-release observations into the
   policy APIs, including cross-job exclusion after repeated imported-decode failure.
   Select the import policy from the reviewed ledger. Bootstrap now validates the
   ledger without restoring reset capabilities; continuous policy polling is missing.
4. Implement and separately certify held-category/preserve-old-copy and upgrade-guard
   transports where required. They remain deferred and refused. Any future exact
   single-file canary must prove original-file preservation and safe observation of
   independent auto-imports before the live gate can open.
5. Wire immutable bridge notification preparation and lifecycle delivery into the live
   scheduler, then certify physical delivery with the selected platform versions.
   Adapter composition and loopback delivery tests exist; an enabled report → diagnosis
   → job → grab → verified import → Plex target → notification runtime proof does not.
   Its dead-release, import-mismatch, access-revocation and notifier-down branches must
   be exercised before enabling dispatch. Unsupported transports remain refused.
6. Certify evidence for requested constraints. Dynamic range may remain unknown; an
   SDR requirement stays blocked until a certified collector supplies it. Dynamic-range
   collector certification and explicit Plex multi-version/multipart selection remain
   deferred.

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

The executable **still refuses live mode**. Offline local validation, capability
evidence loading and the full router/target/notification adapter composition are
implemented. Trusted grab ownership, per-transition access refresh, autonomous
per-intent routing and target enrichment, live policy polling and worker/lifecycle
scheduling remain deferred for the reasons above. No configuration change alone can
complete that orchestration. Router and loopback delivery tests are component proofs;
they do not establish a running live repair pipeline. Bootstrap manufactures no
acknowledgement, certificate or capability record.
