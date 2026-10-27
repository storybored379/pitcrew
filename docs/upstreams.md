# Upstream contracts (P2)

These clients have only been exercised against synthetic, in-process transports. They are not
certified against any running server. No deployment was contacted while developing them.
`pitcrew-upstream` supplies the common transport and the versioned P1-agent client;
`pitcrew-arr` and `pitcrew-plex` supply service-specific reads.

## Transport and write boundaries

Each configured instance owns an endpoint, header credential and timeout/response-size profile.
Defaults are 30 seconds and 4 MiB; agent decode needs the configured decode deadline plus transport
margin. Base URLs may have a reverse-proxy path prefix, but cannot contain URL credentials,
query strings or fragments. Query values are encoded. TLS certificates are verified. Redirects,
ambient HTTP proxies, automatic retries and cookies are disabled. Responses are read incrementally
under a size cap and total deadline. Errors contain categories/status codes, never the response,
URL, credential or underlying transport error. DTOs and requests deliberately lack `Debug`.
Facts containing paths/titles remain local sensitive data and must pass redaction before any cloud use.

`LivePermit::authorize(Mode::Live, true)` is an explicit runtime gate. It is only created by a caller
that has checked its startup config and opened its durable store. It is **not** proof of access,
reserved budget, verified replacement or persisted intent; the FixJob worker must enforce all those
requirements before calling a write. The default mode is shadow and cannot construct a permit.
Lost/failed write responses are ambiguous: reconcile queue, history and command state; never blindly
repeat a grab, import or delete. No API in these clients invokes `history/failed`.

## Covered paths and provenance

All request/response examples in the contract tests are invented. Field and route names were checked
on 2026-10-23 against the official [Sonarr v3 schema](https://raw.githubusercontent.com/Sonarr/Sonarr/develop/src/Sonarr.Api.V3/openapi.json)
and [Radarr v3 schema](https://raw.githubusercontent.com/Radarr/Radarr/develop/src/Radarr.Api.V3/openapi.json),
with isolated adapter contracts used to confirm calling conventions. These `develop` sources
are mutable, so the first certified server versions still need to be recorded by the maintainer.

| Client | Reads | Live-gated writes |
| --- | --- | --- |
| Sonarr/Radarr | paged queue/history/blocklist; episodefile/moviefile by ID; interactive releases by episode/movie ID; manualimport candidates by download ID | POST release; POST command `ManualImport`; DELETE episodefile/moviefile; DELETE queue with blocklist |
| Plex | library sections, metadata by numeric rating key, active sessions | none |
| P1 agent | `/v1/health`, `/v1/evidence`, `/v1/probe`, `/v1/decodecheck`, `/v1/file`, `/v1/plexfails` | none |

Arr page callers must walk all pages; one page never establishes global absence. History is sorted
newest first, but arrivals can be late: retain stable instance/history-ID deduplication and poll an
overlap window. Unknown optional facts remain `None`. An unknown history event string survives as
unknown; a structurally incompatible field is an error, not an empty success.

Interactive release search is an HTTP GET but contacts indexers and consumes quota. Ordinary shadow
polling must not call it implicitly. Manualimport candidates are only proposals: their presence is
not proof of a completed, playable or correctly matched file. Queue blocklisting sets
`removeFromClient=false`, `blocklist=true`, `skipRedownload=true`, `changeCategory=false` to avoid
unbudgeted replacement searches and preserve the download for reconciliation. There is no supported
standalone v3 blocklist-add API in the schemas; non-queue exclusions stay in Pit Crew's own ledger.

Plex uses `X-Plex-Token` and `Accept: application/json`; the read envelope is `MediaContainer` with
`Directory` or `Metadata`. Sources: [Plex official API](https://developer.plex.tv/pms/) and
[Plex server URL commands](https://support.plex.tv/articles/201638786-plex-media-server-url-commands/).
An absent/mismatched sessions count is unknown and cannot authorize maintenance. A positively
reported size zero permits an empty list. No refresh/analyze routes are exposed, including GET routes
that would cause Plex to mutate state. P1-agent contracts come from this repository's HTTP handler
and runtime; `evidence` returns the typed domain `MediaEvidence`.

## Webhook support

Official [Sonarr settings](https://raw.githubusercontent.com/Sonarr/Sonarr/develop/src/NzbDrone.Core/Notifications/Webhook/WebhookSettings.cs),
[Sonarr proxy](https://raw.githubusercontent.com/Sonarr/Sonarr/develop/src/NzbDrone.Core/Notifications/Webhook/WebhookProxy.cs),
and [Radarr settings](https://raw.githubusercontent.com/Radarr/Radarr/develop/src/NzbDrone.Core/Notifications/Webhook/WebhookSettings.cs)
provide username/password Basic authentication and advanced custom headers. Configure either the
intake's Basic credentials or its Bearer header, with a distinct secret per instance. Test each
installed version; do not assume a webhook signature or sender timestamp is available. A replayed
identical payload is deduplicated locally; missing webhooks require paged queue/history fallback.

The official [Sonarr payload builder](https://raw.githubusercontent.com/Sonarr/Sonarr/develop/src/NzbDrone.Core/Notifications/Webhook/WebhookBase.cs)
and [Radarr payload builder](https://raw.githubusercontent.com/Radarr/Radarr/develop/src/NzbDrone.Core/Notifications/Webhook/WebhookBase.cs)
use `Download` for an import. Sonarr can provide either one `episodeFile` or an `episodeFiles` array
for import completion. These sources expose no `DownloadFailed` callback: accepting that event in
intake is a compatibility extension, and installed versions need history polling to observe failures.

## Unverified behavior and intentional limits

- Hold-category import prevention, upgrade guards, recycling semantics, queue blocklisting without
  re-download, `ManualImport` optional fields and success/error responses require version-specific
  certification. A returned command/release object is acknowledgment only, never ready evidence.
- `quality`, `languages`, Plex media/session details and import rejections preserve a limited opaque
  JSON shape. Their semantics must be explicitly interpreted before authorizing any action.
- The exact relation between API file IDs and filesystem identity must be established by fresh
  agent evidence. Numeric IDs alone cannot prove a source/destination is unchanged.
- Per-instance read capability does not establish a viewer's library access.
- An existing tool's status route may perform writes; HTTP GET alone does not establish
  read-only behavior. Shadow observes authenticated arr history; when actor attribution is
  unavailable it reports unknown rather than claiming a particular tool acted.
- No live write transport is wired by this milestone; the executable's read-only rollout and any
  later cutover remain subject to the engine's startup gate and version certification.

## Import arbiter shadow

`pitcrew_arr::shadow::observe_queue` converts the same queue/manualimport DTOs into a conservative
`ImportObservation`. It never invents viewer access, ownership, a legacy actor, title/language
agreement or decode success. A worker can enrich those facts from authenticated observations;
missing evidence yields `Hold` and contradictory evidence yields `Deny`.

`pitcrew_arbiter::shadow::evaluate` mirrors the legacy imports module's clear rule path (title
exact/prefix; year, language and episode agreement; no samples or arr rejections; no existing file;
one video for movies), then additionally requires current access, exact download ownership and an
exact source identity matching its decode. Each proposal covers one file; multi-file episode packs
remain held until split into separately verified proposals. `Allow` means a proposal for a durable FixJob, not an
executed command. The model-confidence path is deliberately deferred: this phase holds uncertain
cases rather than allowing a model to authorize a replacement.

`compare` returns a versioned JSON-serializable report with local correlation IDs, `Allow`/`Deny`/
`Hold`, an enum reason, observed legacy action and divergence class. Serialize one per JSONL line.
Legacy `blocklist_and_search` is `UnbudgetedReplacement`; an observed import without a safe proposal
is `ImportWithoutSafeProposal`; other disagreements are `DifferentDecision`. When arr history does
not identify the legacy actor, use `UnknownLegacyAction` rather than claim attribution.
P1 `QueueFailureEvidence` is passed through the existing `QueueInterference` policy: authoritative
remove+blocklist+marked-failed history and a complete local mutation audit are required before
attributing interference. Timing alone never identifies a tool. Degrading the route to immediate
import/verification does not bypass the arbiter's verified-source requirements.

The arbiter report's `interference` describes only the supplied observation; it is not a stored
per-client policy. The engine integration must retain a confirmed route downgrade across later
observations with no new evidence. That persistent policy enrichment, viewer-access enrichment,
exact-source mapping and authenticated legacy actor correlation are not supplied by raw queue
polling in P2; those queue reports therefore remain conservative holds/unknown comparisons.
