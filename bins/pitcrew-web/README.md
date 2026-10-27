# Pit Crew web

See [the web intake guide](../../docs/web.md) for configuration, browser flows,
security boundaries, and the explicit Python parity and engine handoff gaps.

## Synced picker

Set `catalog_snapshot_file` in the web JSON configuration to the local JSON
serialization of `pitcrew_sync::CatalogSnapshot`. The web app rereads that file
for each authenticated request, so a sync host can atomically replace the export
without restarting the viewer UI. Use a private local file and keep catalog
library IDs equal to the configured Plex server's section keys. The legacy
`catalog` array remains available when no export path is configured.

A configured export is authoritative: missing, invalid, or oversized exports show
a temporary catalog error. There is no fallback to static entries. Reads are
bounded to 16 MiB; expired records do not appear. Fresh Plex access is still
checked on every request. Recent plays and watchlist rows come only from the
current account's synced activity, intersected with fresh catalog rows and its
currently shared libraries. Missing activity leaves search available. Neither
feed starts an upstream request in the web process.

Whole-show selections open an episode picker; they cannot submit a whole-series
repair. Synced picks use stable series identities, so separate shows with the
same title stay separate. The legacy static catalog groups episodes by title and
library because it has no series identity; each submission still requires an
explicit episode ID. Shared-catalog search returns at most 100 matches with a
refine-search hint; episode choices have 100 entries per page, with previous and
next links so every episode remains reachable.

## Wizard intent

Saved reports include a server-derived `wizard` object with `issue`, `answer`,
`details`, and `disposition`. The disposition is `diagnostic`, `self_resolved`,
`advice`, or `review`; a browser cannot choose it. Buffering that improved is
self-resolved; checks not yet tried and ads receive guidance; general feedback
is saved for review. Ads and general feedback retain their selected item but
have an empty diagnostic-kind list. Other details require 1–140 characters;
optional diagnostic details remain bounded to 2,000 characters.

The new video-broken branch records a playback symptom for evidence collection.
No choice proves file damage or authorizes replacement. Copy describes only
confirmed actions: saving a report or providing guidance. My reports distinguishes
advice and self-resolved entries from reports awaiting diagnosis. When the bridge
has saved `report_progress`, the account-scoped page renders received, needs
review, advice, working, fixed, or could not fix. Only the engine projection can
show fixed; working explicitly says verification is still pending. Missing or
unknown states keep the received message, and a revoked library hides its reports.
Raw copy keys and internal job identifiers are never displayed.
