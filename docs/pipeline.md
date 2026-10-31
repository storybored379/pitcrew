# Catalog, access and diagnosis pipeline

`pitcrew-sync` supplies read-only catalog snapshots. `pitcrew-intake::pipeline`
resolves an authenticated stored report, gathers evidence, runs rules and optional
shadow providers, and commits a diagnosis. Neither component has a library-write
or download capability. A diagnosis is input to the separately gated fix bridge;
it is not permission to mutate media or a claim that playback is fixed.

## Host integration

1. Configure an explicit `LibraryBinding` for each Plex section and arr instance.
   Server and arr scopes are opaque installation-local names. Library IDs must
   agree with the viewer authorization adapter; the existing Plex web adapter
   returns section keys as decimal strings. Preserve that representation when
   connecting to an engine that uses numeric library IDs.
2. Use the sync readers with the bounded upstream transport. Plex metadata is
   paged, with an optional update watermark. Sonarr series/episodes/files and
   Radarr movies/files are bounded reads. Apply pages through
   `CatalogSnapshot::merge_page`; only a complete successful full sweep may call
   `retain_seen`. An incomplete or incremental sweep must not delete unseen rows.
   Timestamp each batch from its oldest source observation, not export time.
   Serialize sweeps per library; persisted observation watermarks reject older
   pages, including after a completed sweep removes a record.
3. Obtain access snapshots from trusted Plex shared-library data. A failed read,
   missing viewer or expired snapshot grants nothing. The host refreshes snapshots
   before expiry. `AccessSnapshot` implements the pipeline's `AccessProvider`;
   a multi-viewer host dispatches to the appropriate account snapshot.
4. Persist authenticated intake using `Store::persist_intake` or
   `persist_web_report`. Read bounded pending rows with
   `pending_diagnoses(after_id, limit)`. `PendingReport.account_id` comes from the
   journal, never from a request to the diagnosis worker.
   The cursor is a report-ID page cursor, not a processing watermark: repeat
   bounded sweeps from the beginning so earlier reports awaiting refreshed
   access or evidence are retried.
5. Construct `Pipeline::new(store, catalog, access, evidence, clock)` and call
   `diagnose(report_id)`. The clock returns Unix **milliseconds**. Snapshot and
   database timestamps are Unix **seconds**. `pipeline::collectors::Collectors`
   composes the P1 agent, native Tautulli and optional Tracearr bridge clients.
   Configure their existing endpoint deadlines and response limits explicitly.
6. Rules are always primary. `with_shadows(providers, cloud_consent)` requires
   explicit consent for any nonempty shadow list. Provider construction also
   enforces its own consent and request limits. No provider is constructed or
   called by default. Shadow decisions never replace the rules decision.
7. Handle `Outcome` explicitly. Missing/stale catalog, unknown access and changed
   evidence are retryable after refreshing trusted inputs. Clarification requires
   viewer input. A report containing multiple selected items needs separate
   per-item reports before diagnosis; the current seam has one row per report.
   Do not silently choose the first item. The host owns polling/cursor scheduling
   and clarification delivery.

Catalog resolution requires one Plex version and part, an exact translated file
path, and an unambiguous arr item/file membership match. Titles are display text,
never join keys. Multipart and multiple-version media remain unavailable for
automatic diagnosis until exact selection is supported. An agent observation
supplies the byte identity; an arr ID is never substituted for an agent identity.

The pipeline checks access, catalog freshness and exact mapping before collection
and again before committing. Media evidence older than five minutes, in the future,
with a zero identity or a mismatched expected revision is unknown. Access expiry
also applies while an optional shadow provider is running. Missing telemetry is
unknown, not a playback failure. Plex log counts provide context but cannot prove
broken bytes. Audio language facts require a successful exact-file stream probe;
untagged audio streams do not prove that a requested language is absent.

## Durable seam

`pitcrew_intake_store::diagnoses::Diagnosis` is the typed record. SQL columns in
`diagnoses` are:

| Column | Representation |
| --- | --- |
| `report_id` | INTEGER primary key, references `web_intakes(id)` |
| `decided_at` | INTEGER Unix seconds |
| `decider_kind` | TEXT serialized enum name, `rules` for this pipeline |
| `decision` | TEXT JSON, the P4 `DecisionReport` including `decision` and `usage` |
| `evidence_summary` | TEXT JSON, P4 `EvidenceFacts` |
| `resource` | TEXT scoped arr media identity |
| `library_id` | TEXT, matches the access adapter's library ID |
| `arr_instance_id` | TEXT opaque configured instance ID |
| `arr_item_id`, `arr_file_id` | INTEGER exact movie/episode and current file IDs |
| `agent_file` | nullable TEXT JSON `FileIdentity {id, revision}` |
| `agent_observed_at` | nullable INTEGER Unix seconds, paired with `agent_file` |
| `access_observed_at`, `access_expires_at` | INTEGER Unix seconds |

`diagnosis_comparisons(report_id, comparison)` holds the serialized P4 comparison
for the owner UI. The pipeline writes the diagnosis and comparison in one SQLite
transaction. A failed comparison insert rolls back the diagnosis. Records are
immutable: identical retries are no-ops, conflicting retries are errors. Reads
are bounded to 100 rows per page and ordered by report ID. Existing intake
databases gain the tables through idempotent startup schema creation.

Those retry rules describe the store methods. `Pipeline::diagnose` reconciles a
competing valid first writer and returns `AlreadyDecided` without replacing its
record. The bridge reads `Store::diagnoses(after_id, limit)`. Report-ID pagination
does not track commit order: an older pending report can receive a diagnosis
after a newer report. Reset the cursor for repeated bounded sweeps, or select
unprocessed diagnoses against bridge progress; a permanent increasing report-ID
watermark would lose late diagnoses.

Evidence and comparison JSON contain only closed typed facts, decisions and usage
measurements. Viewer text, titles, usernames, paths, tokens and raw upstream errors
never enter those JSON fields. Local identity columns stay private to the control
plane; they must not be projected into cloud requests or public viewer responses.

The bridge must independently require fresh authorization, current exact identity,
positive broken-byte evidence, budgets and its engine safety gates. An advice or
review diagnosis, nullable agent identity, or provider opinion cannot create a
replacement job by itself. The existing engine still owns verified import and
the only valid transition to a fixed/ready notification.

## Viewer wizard

See [the web README](../bins/pitcrew-web/README.md) for snapshot configuration,
account-scoped recent-play/watchlist pickers and wizard fields. Whole-show
selection leads to an episode picker. Ads use play-origin advice, other requests
go to review, and self-resolved buffering has its own receipt. These routes stay
non-replacement even if an unrelated bad decode was observed.

## Verification and remaining runtime work

Tests use synthetic Plex/arr fixtures, in-process transport fakes, temporary
SQLite files, expiry properties and report-to-diagnosis scenarios. They cover
identity mismatches, unavailable optional telemetry, access revocation during
collection, concurrent immutable persistence, restart and redacted shadow output.
`cargo xtask ci` includes the new sync package in its mutation gate.

No live endpoint, filesystem mount, shared account or cloud provider is certified
by these tests. The host must schedule refreshes and diagnosis work, persist
private snapshots atomically, configure authenticated read clients and pass
diagnoses to the fix bridge. The library does not automatically discover servers,
schedule requests, write cloud credentials or enable live effects.

The first pipeline supplies probe/decode results, correlated log/session facts,
resolution, complaint and inspected audio languages. Dynamic-range metadata,
alternate-HD availability, provider throughput, Plex analysis state and positive
server-health evidence remain unknown until exact source observations are wired
in. Their existing P4 rules therefore do not activate from this pipeline yet.
Unknown health never becomes a proven outage. Plex cloud watchlist discovery and
external-GUID matching are also deferred: pickers accept a trusted normalized
feed of exact local catalog IDs and never match watchlist titles by name.

## Control-plane integration

The `pitcrew` control plane now calls the sync readers and `Pipeline` for bounded,
rotating report batches when the shared stack is enabled. It exports private catalog
snapshots atomically for the web launcher, refreshes account access independently,
checks current arr identity before/after collection, registers the full fingerprint,
and records bridge authority from collected agent evidence. Neither viewer text nor
raw webhooks can provide that authority. Resweeps recover committed diagnoses and
saved comparison/usage projections after restart; observation timestamps are never
refreshed merely by replaying a saved diagnosis.

Clean, single-video ffprobe facts can now establish SDR, HDR10, HLG or supported
Dolby Vision profiles. Unknown, conflicting or incomplete metadata remains unknown.
The no-profile-5 invariant is checked against exact replacement source bytes before
import and exact destination bytes before Ready. Alternate-HD availability, provider
throughput, Plex analysis and positive server-health diagnosis facts remain unknown
without their respective certified collectors. The runtime currently uses rules;
optional model/telemetry scheduling remains unwired.

## Action attribution in shadow comparisons

Arr history and webhooks describe events, not their actors. The comparison joins
an exact, case-sensitive download ID **within its configured arr instance** to a
trusted action receipt. Titles, timestamps, media IDs, matching release GUIDs and
queue disappearance never establish attribution. Routine RSS, upgrade, new-item
and interactive grabs without a receipt stay `unknown`.

The engine records accepted dispatches against the exact started outbox intent.
An explicit grab-response `downloadId` can identify that grab; HTTP success without
one stays ambiguous. ManualImport and queue-removal receipts use their independently
bound exact download IDs. Pending, suppressed and shadowed intents cannot establish
an observed engine action. A lost response without a recorded receipt remains
unknown until trusted evidence arrives. These receipts do not authorize effects
or establish completion, decode success, access or budgets.

An administrator-prepared `--import-legacy` snapshot can supply machine-recorded
download IDs through an optional `fix_attempts.download_id` column, including
proactive attempts without report links. A separate optional `arr_actions` table
supports exact external import/removal receipts; see [shadow exports](shadow.md).
Existing native `finish`/`lmfinish` KV completion markers also supply exact accepted
ManualImport receipts scoped through reports or landmine records. These establish
only an external-tool import actor; counters, waits and unrelated values are ignored.
An external tool's grab never establishes that the same tool imported the download.
Conflicting action claims remain unknown regardless of insertion order.

`pitcrew --export` re-joins original observations with the current trusted receipts,
so late imports and conflicts are reflected after restart. It retains immutable
first-projection journal rows for audit. The human summary separates unattributed
routine/unknown observations, counts each actor and lists observed/proposed action
pairs. Counts are comparison rows, not unique repairs or independent safety proofs.
Queue exports distinguish download-grab ownership from the import actor; attribution
alone never populates the arbiter's missing access, ownership or exact-byte evidence.

My reports reads the account-scoped `report_progress` projection: received, review,
advice, working, fixed or could not fix. Web reports have no guessed channel route.
Bot routes require an exact stored message reservation and current authenticated
account link; a Plex account number alone never becomes a notification destination.
