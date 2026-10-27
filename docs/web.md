# Web intake

`pitcrew-web --config web.json` serves the Plex sign-in and report flow. The default
listener is `127.0.0.1:7401`. `--check` validates configuration without opening the
database, listener, or any upstream connection. Nothing in this process repairs a
file, sends a download request, or announces that a file is ready.

```json
{
  "database": "intake.sqlite",
  "public_origin": "http://127.0.0.1:7401",
  "plex_server_url": "https://media.example.invalid/",
  "plex_server_id": "configured-machine-identifier",
  "client_id": "stable-installation-identifier",
  "catalog": [
    {
      "id": "sample-episode-hd",
      "title": "Signal Garden",
      "library_id": "2",
      "library_name": "Series",
      "resolution": "hd",
      "season": 2,
      "episode": 3
    }
  ]
}
```

Use a stable, installation-specific `client_id`. The default Plex account API is
`https://plex.tv/`; `plex_auth_url` exists for an explicitly configured endpoint
and local contract tests. A configured media server URL is never inferred from
viewer input. TLS and a reverse proxy are required for an external public origin;
HTTP public origins are accepted only for loopback. Forwarded headers do not
change the origin or authorization decision.

The browser receives opaque, HttpOnly, SameSite cookies. HTTPS origins also set
Secure. The sign-in challenge expires after ten minutes and is consumed once;
sessions have a seven-day absolute expiry. Account tokens remain on the server in
the SQLite database, which is created with private permissions on Unix; an
existing database readable by other users is rejected. Protect the whole data
directory and its backups. Sensitive responses are no-store and a restrictive CSP
allows only the stylesheet and same-origin forms.

Every authenticated page and report/link mutation revalidates membership by
Plex machine identifier and obtains the accessible library section keys using
the viewer's own token. Failed, malformed, or unknown checks deny access. Mutations
also require the exact configured Origin and a session-bound CSRF value. A
browser-provided item ID cannot grant access. The report issue and follow-up
answer are validated server-side; arbitrary details are only recorded as
untrusted text. They are not instructions to the engine.

The report flow is title/library/episode selection, problem selection, a guided
follow-up question, then receipt and My reports. Buffering, subtitles, audio
language, no sound, picture quality, wrong episode, and playback failures have
separate choices. Follow-up answers and details are durably stored alongside the
same parser result that channels use. Retries with the same browser submission ID
are deduplicated. The five-report daily quota and report insert commit together;
invalid input and retries do not spend that quota. Reports are scoped to their
account and current library access, with the selected episode and symptom shown.

The SQLite database has a private `<database>.link-key` companion file holding a
random 32-byte HMAC key. Preserve both files together in private backups. Missing
or corrupt keys on an existing database fail closed; they are never silently
regenerated. Link codes are 12 hexadecimal characters; only HMAC-SHA256 digests
are stored. Link lookup and redemption have durable attempt limits.

Generic chat linking creates an expiring one-use code after fresh Plex access
validation. Send `link CODE` privately to the configured bot. Creating another
code cancels the old one. Linking is identity proof only; the bot must check fresh
library access for every intake. Existing links are not silently reassigned.

## Current boundaries

The catalog is a typed static or atomically exported snapshot. Its library IDs
must match Plex section keys; each movie/version or individual episode/version
needs a unique item ID. The sync library supplies exact catalog/access mappings;
continuous refresh depends on the [runtime integration](stack.md#current-runtime-boundaries).
Trusted normalized recent-play/watchlist feeds, whole-show episode selection,
ads/other advice, broken-video reports and self-resolved buffering are implemented.
Cloud watchlist discovery and external-GUID matching remain unavailable.

Accepted reports are durable intake records. A receipt alone does not start a
repair or claim ready. Diagnosis, bridge authority and lifecycle projection have
separate trust boundaries; see [pipeline](pipeline.md) and [bridge](bridge.md).
Delivery reservations preserve ambiguous bot sends for audited reconciliation.
The [owner desk](admin.md) displays recorded requests and exposes the supported
policy/delivery actions; its runtime consumer limitations are documented there.

Tests use temporary private databases, injected auth traits, or HTTP servers
bound to process-owned ephemeral loopback ports. They never contact Plex or an
installed media stack.
