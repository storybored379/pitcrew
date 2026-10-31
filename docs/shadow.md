# Engine and import arbiter shadow (P2)

`pitcrew` runs a local SQLite journal, authenticated arr webhook intake, optional
read-only arr polling, and shadow report generation. It can replay a trusted,
recorded fix through the real reducer and durable worker without executing its
effects. The executable **refuses `engine.mode = "live"`**, even with the explicit
live acknowledgment: no certified real effect router is wired into this binary.
The engine library's live path is exercised with fakes.

This is pre-alpha observation infrastructure. Raw webhooks and queue/history reads
do not establish viewer access, exact-file decode, job ownership, or which tool
acted. Their reports retain unknown evidence and propose `Hold`. They cannot count
toward safe parity or authorize a repair. See [upstream contracts](upstreams.md)
and [local agent setup](agent.md) for the separate transport and media boundaries.

## Configuration and startup

Start with [config.shadow.example.toml](../config.shadow.example.toml). The example
contains five independent instances: two Sonarr and three Radarr. Those counts are
examples, not installation limits. Replace its placeholder URLs and Plex library
keys, choose stable instance IDs, and provide these local values:

| Setting | Owner supplies |
| --- | --- |
| `database` | A writable SQLite file path; create its parent directory with private permissions |
| `engine.listen` | Loopback socket, default `127.0.0.1:8791`; a non-loopback bind is rejected |
| Each `arr.url`, `arr.kind`, `arr.libraries` | The actual API base URL, product, and mapped Plex section keys; a library mapping is not an access grant |
| Each `arr.webhook_secret` | A distinct env/file reference holding 32–4096 visible ASCII bytes; use a fresh random secret, not an API key |
| Each `arr.api_key` | An env/file reference for that arr's API credential; it is resolved only when polling is enabled |
| `engine.control_secret` | Optional separate env/file secret with the same length/character bounds; omit it to disable recorded replay/import submission |
| `engine.poll_enabled` | Default `false`; deliberately enable it only when upstream read-only observation is authorized and credentials are ready |
| `engine.poll_seconds` | Poll/projection interval, 1–3600 seconds, default 30 |
| `download_clients` and `budgets` | Profile deadlines and replay job/global/viewer budget envelopes; see the limits below |

A trailing newline in a secret file is trimmed. Config parsing does not resolve
secrets or contact an upstream. A configured webhook/control secret is resolved
at startup, including export-only startup. With polling disabled, API credentials
remain unresolved and the process makes no upstream polling calls.

```sh
cargo build --locked -p pitcrew
./target/debug/pitcrew --config /etc/pitcrew/shadow.toml
```

Startup logs identify shadow mode, disabled effects, the listening address, and
whether polling is enabled. `GET /healthz` is an unauthenticated local health check
of the process's database access; it does not assert upstream health or parity.

For remote arr senders, terminate TLS at a trusted ingress that can reach the
loopback listener. Use a URL such as `https://shadow.example.invalid`; supply your
own hostname and certificate. Keep the service's loopback bind. A container needs
an ingress in the same network namespace or an appropriate host-network design;
ordinary bridge port publishing cannot reach a container's loopback-only socket.
No native TLS/mTLS listener is included.

## Arr webhooks and polling

Create one webhook connection in each arr instance, using its configured ID in
the path. The five-example mapping is:

| Instance ID | Example webhook URL |
| --- | --- |
| `series-main` | `https://shadow.example.invalid/v1/webhooks/series-main` |
| `series-alt` | `https://shadow.example.invalid/v1/webhooks/series-alt` |
| `movies-main` | `https://shadow.example.invalid/v1/webhooks/movies-main` |
| `movies-alt` | `https://shadow.example.invalid/v1/webhooks/movies-alt` |
| `movies-extra` | `https://shadow.example.invalid/v1/webhooks/movies-extra` |

Configure **Basic authentication** with username `pitcrew` and that instance's
webhook secret as the password, or an advanced custom header
`Authorization: Bearer <instance-webhook-secret>`. Choose one method. This
credential is independent of the arr API key used for polling. Verify support on
the installed version; upstream authentication provenance is linked in
[upstream contracts](upstreams.md#webhook-support).

Intake accepts `Grab`, `Download`/`Import`, `DownloadFailed`,
`ManualInteractionRequired`, `Rename`, `EpisodeFileDelete`, `MovieFileDelete`,
`Health`/`HealthRestored`, and `Test`. Sonarr import completion supports both
`episodeFile` and `episodeFiles`. `DownloadFailed` is a compatibility extension;
the examined arr webhook builders do not emit it, so history polling is needed
for those failures. `Test` confirms delivery/authentication, not permission to fix.

A valid POST returns `202` with `accepted: true` and a `duplicate` flag.
Unauthorized, invalid, and oversized requests return `401`, `400`, and `413`;
store failures return `503`. Bodies are limited to 64 KiB, headers to 8 KiB and
32 fields, connections to 32, and request read/write phases to ten seconds each.
Duplicate Authorization/Content-Length headers and transfer encoding are rejected.
Connections close after one request.

Replay protection hashes canonical JSON within each instance: whitespace/key
ordering changes do not bypass lifetime duplicate suppression, including after
restart. There is no signed sender timestamp or freshness proof. Identical bodies
from distinct instances remain distinct observations. A different body can be a
new observation; this does not grant mutation authority. Only normalized event
kind, download identifier, and available media/file IDs are retained from a
webhook, rather than arbitrary titles, paths, or payload credentials.

When enabled, polling reads up to ten 100-record history pages and the first
100 queue rows per instance, under one 30-second per-instance deadline. Stable
instance/history IDs deduplicate overlapping history windows. Import-blocked queue
rows may trigger at most ten manualimport-candidate reads per instance per pass.
Missed timer ticks are skipped; a configured interval is not a promise that all
instances finish within it. It does not run interactive release searches, which can contact indexers.
A page cap, incomplete response, or read failure logs `poll_incomplete`; absence
from a partial window never proves a download failed or disappeared. These reads
produce observations and conservative arbiter reports, not FixJobs or imports.

## Trusted recorded comparisons

Two control endpoints accept evidence prepared by an administrator or trusted
recording adapter. Authenticate with the separate control secret using Bearer or
Basic (`pitcrew`) credentials. Both have the same 64 KiB body bound. These APIs
are not viewer intake: a caller is responsible for trustworthy attribution,
access facts, and consistent opaque identities.

`POST /v1/shadow/replay` accepts this structure, defined by
[`Replay`](../bins/pitcrew/src/lib.rs) and [`NewJob`](../crates/store/src/lib.rs):

| Field | Meaning |
| --- | --- |
| `key` | Stable nonempty replay idempotency key |
| `job` | `id`, opaque `viewer`/`resource`, `library`, exact `original` identity, `constraints`, `policy`, stage `timeouts`, and `created_at` |
| `client` | Optional configured download-client ID; its deadline profile replaces `job.timeouts` |
| `events` | 1–512 ordered `{at, key, event}` records using the domain's serialized `Event` representation; keys must be unique and times valid |
| `observed` | Optional map from decimal outbox sequence to `Activity`, describing the authenticated legacy action and safety evidence for that intent |

Job IDs and resource identities must be stable across retries. `policy.max_grabs`
cannot exceed configured `budgets.per_job`; creating the replay reserves an
installation/viewer envelope. The full transcript is checked before journal/job
creation. Committed events are replayed through the reducer, effects are recorded
as `shadowed`, and comparisons are projected from the durable outbox. Successful
submission returns `200` with `reports` and a human `summary`. Invalid/conflicting
transcripts return `409`; replaying an unchanged accepted transcript returns its
durable reports. The selected client profile is frozen into the effective job;
recovery does not reinterpret an edited profile.

The background projector resumes durable replay inputs after an interrupted
worker's 30-second lease expires. Revisiting completed inputs is idempotent. A
crash between job creation and persisting the replay input occurs before any event
or effect; resubmit that unacknowledged request to supply the missing input.
Rejected job specifications/budgets do not publish a doomed recovery input.
Fixtures in [service tests](../bins/pitcrew/tests/service.rs)
show exact JSON shapes without real credentials or media.

Omitted observed actions remain unknown. Proposed safety facts describe the
reducer's validated proposal at the recorded event time; they are not evidence
that a remote command completed or that a file remains playable now.

`POST /v1/shadow/imports` accepts `{key, observation}`, where `key` is a stable
1–256-byte observation key and `observation` is
[`ImportObservation`](../crates/arbiter/src/shadow.rs). It includes opaque job,
library and download IDs, owned-download identity, observation time, media kind,
import-blocked status, access, source/decoded-source identities, matching/rejection
facts, optional external-failure evidence, and an authenticated `legacy_action`.
This supplies the missing evidence for comparing an external importer's actual behavior
with an arbiter proposal. Success returns `200` with `duplicate` and `report`;
conflicting reuse returns `409`. An `Allow` decision is still only a proposal.

## Offline external ownership importer

This optional adapter accepts an administrator-prepared **standalone SQLite
backup** with the schema described below. It is not needed for a new installation.
Existing tools must export this schema before their history can be compared:

```sh
./target/debug/pitcrew --config /etc/pitcrew/shadow.toml \
  --import-legacy /var/lib/pitcrew/legacy-backup.sqlite --source-key legacy-one
```

The source key is a stable installation namespace of 1–64 ASCII letters, digits,
hyphens or underscores. Reuse it for later backups of the same external database;
choose a different key for a different installation or reset database. This
command exits after importing and projecting local observations. It does not
resolve credentials, create upstream clients, poll, or listen, regardless of the
polling configuration. Live mode remains rejected.

Prepare the backup outside this command with the database owner's SQLite backup
procedure. Do not point the importer at the active database or copy only its main
file while WAL is active. The importer rejects all `-wal`, `-shm`, and `-journal`
sidecars, even empty ones; it never checkpoints or removes them. It reads at most
256 MiB through a read-only file handle, checks source identity/metadata before
and after capture, and queries a private frozen temporary copy using SQLite
read-only, immutable, query-only mode in one transaction. Source content and
sidecars are never written. The writable engine destination may not alias the
source or its sidecars. The copy is deleted on return; source snapshots and the
engine journal still need administrator-controlled private storage. A backup with
more than 50,000 rows across all recognized tables, including optional receipt
tables, is rejected rather than silently truncated.

The importer understands `reports`, `fix_attempts`, `audit`, `item_fixes` (including
its `kind` migration), and `rate_events`. For report-linked historical replay, a
successful named-release attempt must have an explicit `report_id` and matching
item, instance, media kind, and media ID. Failed searches and unlinked attempts do
not establish a report-linked grab; an explicit download receipt can independently
attribute a grab as described under [trusted action logs](#trusted-action-logs-and-actor-counts).
Exact structured
report prefixes in import/decode/ready audit entries, combined with a proven
attempt owner, establish historical external actions. Import command acceptance
is recorded separately from exact-file verification. Source table primary keys
and the installation namespace deduplicate repeated imports and restart
projection; changing an already imported audit record produces a conflict.

Private observation metadata retains timestamped budget/retry entries, their row
counts, and unattributed record counts. It does not infer an atomic reservation or viewer authorization
from a global research bucket. Viewer identifiers/text, release titles, instance
names, paths, and audit details are discarded. Historical arr file IDs and decode
verdicts are retained for local review; they cannot supply an immutable agent file
revision or prove that a later ready message concerned unchanged bytes.

These `LegacyReplay` observations identify the external historical actor, but
compare against `Hold` with insufficient evidence. They create no FixJobs,
reservations, or effect intents. To obtain an action-comparable reducer replay,
a trusted recorder must additionally supply scoped access grants, immutable
source/destination identities, complete budget reservations and correlated
upstream results through the recorded replay boundary above. Timing, title
similarity, and arr IDs alone never supply that evidence. Export the resulting
comparisons with the existing `--export` command.

## JSONL exports and interpretation

Export from the configured local journal without starting the listener or polling:

```sh
./target/debug/pitcrew --config /etc/pitcrew/shadow.toml --export /var/lib/pitcrew/review-001.jsonl
```

This projects pending local observations and creates three new private files
(mode `0600`): `review-001.jsonl`, `review-001.jsonl.summary.txt`, and
`review-001.jsonl.arbiter.jsonl`. Existing files are not overwritten. Choose an
unused output prefix; a write failure can leave partial outputs. Projection/export
fails closed if a recorded replay still has an active lease; retry after that work
finishes or its lease expires. The summary is
also printed to stdout. Reports and opaque local IDs are sensitive operational
data; redact them before any cloud use.

Each fix JSONL line uses schema `version: 1`:

| Field | Meaning |
| --- | --- |
| `fix` | Correlation/grouping key; several intent comparisons may share it, so it is not a unique line ID |
| `observed`, `proposed` | `Activity` with `action`, `actor`, `correlated`, and `safety` |
| `agreement` | `match`, `different`, or `insufficient_evidence` |
| `observed_unsafe`, `proposed_unsafe` | Independently evaluated safety violations |
| `missing_evidence` | Missing required safety fact, unknown actor, or absent trusted correlation on either side |

Actions include `grab`, `import`, `delete_library`, `remove_download`, `decode`,
`check_target`, `upgrade_guard`, `ready`, `admin_failure`, `viewer_failure`,
`reconcile`, and `hold`. Actors are `external_tool`, `external_importer`, `engine`, or `unknown`.
The `python` wire value remains supported for existing version-1 snapshot records;
unrecognized actor values become `unknown` and cannot establish safe parity.
Safety facts are tri-state booleans (`true`, `false`, or `null`): `access`,
`budget`, `replacement_verified`, `exact_import_decoded`, `notification_unique`,
`no_auto_redownload`, and `reconciled`.

### Trusted action logs and actor counts

The summary adds per-actor counts and observed/proposed action pairs for attributed
rows. Unattributed routine/unknown activity is counted separately. Attribution does
not fill missing safety facts: an identified external grab can still compare against
`Hold` with insufficient evidence. Engine receipts show the exact saved proposal
alongside the observed event. Counts describe comparison rows (history, webhooks and
legacy reports may overlap), not unique repairs or completed effects.

Older standalone legacy snapshots still work unchanged. If a `fix_attempts` table
has a nullable `download_id` column, a successful named-release attempt's explicit
ID is an action-specific grab receipt, even without a report link. Missing IDs are
never guessed from the existing `target` field, titles, media IDs or timing. A
terminal attempt verdict does not establish who imported the download.

Existing snapshots can also supply their native accepted-ManualImport markers in
the optional `kv(k,v)` table. An exact `finish:<report_id>:<download_id>` key with a
positive timestamp uses that report's `arr_instance`; an exact
`lmfinish:<landmine_id>:<download_id>` key with a positive integer count uses the
optional `landmines` table's `instance`. These markers attribute only `import` to
`external_tool`. They establish neither a grab owner nor safe or completed import.
Title-based matches that the older tool used before dispatch are not reused.

Counter and wait suffixes, predecode markers, unrelated keys, missing references
or instance IDs, and missing/`None` download IDs cannot attribute an action. Only
resolved receipt values are selected. The importer hashes native keys and
instance/download correlations; it stores no raw KV key or value. Timestamp/count
updates deduplicate the same receipt; changing its instance is a conflict.
Optional tables must be real tables with the required columns. Malformed relevant
receipts fail closed, and all rows in `kv`, `landmines` and `arr_actions` count
toward the shared snapshot limit, including rows ignored for attribution.

For other accepted actions, an administrator can include this optional normalized
export of the tool's **own machine-recorded dispatch log** in the standalone backup:

```sql
CREATE TABLE arr_actions (
  id INTEGER PRIMARY KEY,
  instance TEXT NOT NULL,
  download_id TEXT NOT NULL,
  actor TEXT NOT NULL,
  action TEXT NOT NULL
);
```

`instance` must equal the configured arr instance ID, and `download_id` must be the
exact ID recorded by the actor for that accepted action. `actor` is `external_tool`
or `external_importer`; `action` is `grab`, `import`, `remove_download` or
`delete_library`. Do not create these rows by matching arr history or pending tool
intentions. The import rejects views, invalid rows, unsupported actors/actions and
snapshots exceeding the existing total row bound. Keys remain scoped by `--source-key`;
changing an imported row is a conflict. Correlation projections store a hash of the
instance/download pair, not raw identifiers. No imported row creates a FixJob or
grants access or mutation authority.

Export recomputes attribution from original observations and all available receipts.
A later receipt can identify an earlier unknown event; contradictory claims remove
attribution. The immutable first `diff` row remains an audit of what was known then.
Queue JSONL adds `download_actor`, `import_actor`, and `observed_import` next to the
arbiter decision. A recorded grab owner is independent of import ownership. Old
queue rows without exact download correlation remain unknown. Neither queue status
nor an actor receipt supplies the arbiter's missing safety evidence.

| Unsafe class | Evidence that causes it |
| --- | --- |
| `access` | A grab, import, library deletion, download removal, or upgrade guard lacks allowed access |
| `budget` | A grab has no valid budget reservation |
| `unverified_replacement` | Import/library deletion proceeds without a verified replacement |
| `unverified_ready` | Ready is said without the exact imported file having decoded successfully |
| `duplicate_notification` | Ready/admin/viewer failure notification is not unique |
| `auto_redownload` | Download removal allows an uncontrolled re-download |
| `unreconciled_mutation` | A mutation proceeds without reconciling an ambiguous prior result |

An explicit `false` produces a violation; `null` produces insufficient evidence.
`match` compares actions only and can coexist with unsafe flags. Safe parity
requires complete trustworthy evidence and no unsafe flags in addition to action
agreement. The human summary reports those counts separately; neither identical
`Hold` actions with unknown facts nor a high matching-action count proves safety.

The arbiter export wraps each report in an `ObservationRecord` containing `id`,
`source`, `key`, `payload`, and `at`. Its payload has `schema_version: 1`, job and
download IDs, observation time, `legacy_action`, `decision: {action, reason}`,
optional `divergence`, and `interference`. Arbiter enum values use their Rust
spellings: `Allow`, `Deny`, `Hold`; divergences are `UnbudgetedReplacement`,
`ImportWithoutSafeProposal`, `DifferentDecision`, or `UnknownLegacyAction`.
Unknown access/ownership/source verification holds the proposal. Contradictory
facts can deny it. Multi-file episode packs need separately verified proposals.

## Persistence, remaining work, and deployment status

SQLx embedded migrations initialize SQLite with WAL, synchronous FULL, and foreign
keys. Job rows, append-only events, outbox intents, revision checks, worker leases
and fencing tokens, observation idempotency, and installation/viewer reservations
are implemented. Accepted transitions and their intents commit atomically; jobs
are reconstructed through reducer replay. Ambiguous effects stay unresolved until
reconciled. Uncertain notifications are not automatically resent. Terminal jobs
with unresolved mutations can retain ownership and budget quarantine.

The current store also enforces item cooldowns, exact-file registry revisions,
cross-job exclusions and audited release. The library effect router and immutable
notification preparation are implemented with fake transport and restart tests.
Viewer intake, owner review UI, rules and redacted optional model comparisons are
available; their configuration is indexed in [the documentation guide](README.md).

Remaining live-repair requirements include runtime composition of those boundaries,
independent fresh authority for each report, authoritative external-actor correlation,
and version-specific certification. Held categories, upgrade guards and exact
source-to-destination identity need reviewed evidence. A timing coincidence never
proves an external actor removed a download. See the maintained
[certification checklist](live-certification.md) for current runtime gaps and
[agent limits](agent.md) for operational executors and unimplemented transports.

[The systemd unit](../packaging/pitcrew.service) and [Dockerfile](../Dockerfile)
are supplied for review. The unit expects an administrator-created `pitcrew`
user/group and readable config/secrets, with writable state under `/var/lib/pitcrew`.
The container uses UID/GID 10001; provide private writable state and read-only
config/secret mounts with matching permissions. The container image has not been built, and neither package has been deployed
in this phase. No live shadow run or upstream
certification was performed.

Offline tests cover local fake webhook intake/export, all 12 existing reducer
scenarios through engine plus SQLite, fault/restart boundaries, replay, budgets,
leases, and mutation-sensitive safety decisions. Public synthetic contracts test
the external observation format; optional external replay does not claim Rust
HTTP endpoint parity. Full `cargo xtask ci` is the release gate; this document
does not claim that a particular checkout has passed it. See the
[coverage table](../scenarios/COVERAGE.md) for the evidence boundaries.
