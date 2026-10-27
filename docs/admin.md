# Owner desk

`pitcrew-admin` is a server-rendered owner interface with no JavaScript or remote
assets. It reads the engine and intake journals and writes a separate admin
journal. See [ADR 0010](adr/0010-owner-admin-interface.md) for the trust boundary.

## Configuration and startup

Add the `[admin]` section from
[the example](../bins/pitcrew-admin/config.example.toml) to the stack TOML file.
The shared configuration parser accepts this optional section; other binaries do
not start an admin listener implicitly.

```sh
cargo build --locked -p pitcrew-admin
target/debug/pitcrew-admin --config stack.toml --check
target/debug/pitcrew-admin --config stack.toml
```

`--check` validates configuration without opening databases, resolving secrets or
contacting any endpoint. Startup requires existing engine and intake databases,
a separate writable admin database, and a parent directory with private
permissions. Back up all three databases using SQLite-aware backups. A source
database or sidecar may not alias an admin database or sidecar, including through
symlinks and hardlinks.

The example binds loopback. Use HTTPS at trusted ingress for remote access.
`public_origin` must be an exact origin without a trailing slash. Authentication
and API endpoints require HTTPS, except explicit loopback HTTP for local ingress
and tests. Redirects are not followed. Configure your Plex account ID in
`admin_account_ids`; membership in a shared Plex library does not grant access.
`client_id` is an installation-specific Plex client identifier. The owner UI
never needs a Plex server-owner token or an arr API key.

The optional `admin.api` uses a distinct admin secret from an environment or file
reference. It connects only to the P3 control-plane admin API. Omitting it disables
immediate policy and delivery actions while retaining read access. TLS, ingress
and filesystem permissions are deployment responsibilities; this binary does not
deploy or configure them.

## Views and actions

| View | Saved source | Actions |
| --- | --- | --- |
| Help-desk queue | Report outcomes in `web_intakes`, optional `diagnoses` and `report_progress` | Request fix approval/denial, closure, or a fixed template message |
| Fix jobs | Replayed `jobs` + `events`, effect `outbox`, reservation envelopes | Request lower-quality approval for the exact pending revision |
| Quarantine | `release_exclusions`, including release and effective pin state | Release through API; request pin |
| Client capabilities | Version-bound `client_capabilities` ledger | Audited API reset to unknown |
| Deliveries | Prepared immutable notification payloads and outbox status | Explicit API resend of an uncertain notification |
| Shadow parity | Engine/arbiter and optional agent/decider observations | Read only |
| Decider accounting | Optional recorded usage observations; local explicit rates | Read only; no provider-account settings |
| System health | Optional last-observed reachability and mount/watchdog facts | Read only; no live probes |
| Action audit | Independent admin request journal and append-only audit | Read only |

List views are bounded to 100 records per source. Exact report and job links work
outside that list window. Job event replay is authoritative: missing/corrupt
events fail closed instead of installing a snapshot. Budget rows are whole
reservation envelopes, including active ownership older than the rolling window;
the page does not claim a remaining-budget total.

Release, reset and resend submit to the existing API. An `accepted` request means
the API acknowledged it; a resend still requires its worker to deliver. A lost
response becomes `uncertain`. The UI never automatically retries it. Check the
control-plane audit and actual saved state before deciding on another action.

Approval, denial, closure, template message and pin requests are consumed by
the configured control plane, with results in Action audit. This owner binary
only saves and audits requests. `controller_applied`, `controller_rejected`,
`controller_deferred` and `controller_uncertain` distinguish results from pending
requests. Lower-quality requests are explicitly deferred until the controller
can collect fresh release and upgrade-guard proof; a stale job revision is
rejected. Only replacement recommendations
offer fix approval. Advice-only reports cannot create approval requests. Templates
can acknowledge receipt, ask for details or say a report is under review; no
template claims a fix is ready.

The consumer rechecks the configured administrator allowlist and claims a request
in the same transaction as its audit entry. It never automatically repeats a
claimed request. After five minutes an interrupted claim becomes uncertain for
review, with a fenced result update. Approval revalidates saved diagnosis version,
independent current access and exact-byte registry evidence, and engine budgets.
Denial and closure prevent future automatic repair for an uncommitted report;
they cannot cancel an existing repair or mark a title fixed. Pin updates effective
policy and its audit atomically while retaining the original exclusion evidence.
Expired or released exclusions cannot be revived by pinning an old page snapshot.

Deliveries also shows immutable working, late-join and fixed-template commands
from the intake journal. `shadowed` means no message was sent. `uncertain` means
the result needs review; these follow-ups have no automatic resend. A template
action being applied confirms durable queuing, not remote delivery.

## Parallel integration contracts

`diagnoses` belongs to the intake pipeline and `report_progress` to the bridge.
The UI reads them from the intake database when present. The diagnosis decision
accepts either a serialized `Decision` or `DecisionReport`. An optional
`comparison` column is displayed when supplied. Report progress supplies
`report_id`, `job_id`, `state`, `updated_at` and `viewer_copy_key`. Missing tables
remain visibly disconnected; no job state or success is inferred.

The existing engine observations `diff` and `arbiter:recorded` feed parity views.
Optional producers may append records through `Store::ingest_observation` using:

| Source | Payload |
| --- | --- |
| `agent:shadow` | Saved agent/external-tool comparison facts |
| `decider:comparison` | Sanitized `Comparison` from the decision runner |
| `decider:usage` | `{"provider":"anthropic","usage":...}` where usage is the serialized `Usage`, or null for unreported usage |
| `admin:health` | Last-observed component reachability and mount/watchdog policy facts |

Use a stable unique observation key per call/comparison so a replay does not
double-count it. The runtime writes health and diagnosis comparison/usage records
from its saved evidence. Sources without a producer explicitly show unrecorded
data. Health freshness uses the journal's
observation timestamp; older than five minutes or future-dated is stale. A recent
observation is not a live health check. Accounting covers the most recent recorded
calls only, separates missing usage, and uses administrator-supplied rates for all
four token classes. Estimates exclude unknown usage; no estimate is shown when
all calls lack usage or rates are absent. They are neither invoices nor proof of
free requests.

The new `admin_requests` table is in the admin database. Commands are closed,
tagged JSON with `action` and typed subject fields:

| Action | Subject fields | Consumer requirements |
| --- | --- | --- |
| `approve`, `deny` | `report`, `decided_at` | Exact current replacement diagnosis, no already-attached job |
| `lower_quality` | `job`, `revision` | Current pending quality request, broken exact bytes, fresh viewer access, scoped approval and upgrade guard |
| `close` | `report` | Ticket closure semantics, distinct from cancellation or a ready notification |
| `message` | `report`, `template` | Resolve stored report recipient; durable outbox delivery, no browser-supplied recipient/text |
| `pin` | `id` | Current active exclusion; atomic effective pin and policy audit, preserve original exclusion spec |

Rows also carry `request_key`, authenticated `actor`, required `note`, `state`,
`created_at` and `updated_at`. `admin_audit` records every local state change in
the same transaction and rejects updates/deletes. The bridge consumer writes
`controller_claimed` before acting and commits its result and audit in one
transaction. Interrupted cross-database work remains uncertain and requires
review; polling a pending row alone never authorizes repeat execution.

## Offline validation and accessibility

Tests use temporary databases, synthetic accounts and owned fake HTTP servers.
They cover owner gating, single-use login, session expiry, CSRF/Origin, XSS,
oversized/redirected responses, audit atomicity, request-key races, restart,
source/sidecar protection and P3 API effects with a lost acknowledgement.
Workspace CI includes `pitcrew-admin` mutation testing with no exclusions.

The interface extends the viewer palette and system typography. Dark mode is the
default. Native labels, a skip link, semantic navigation and tables, visible focus,
44-pixel controls, wrapping tablet navigation and horizontally scrollable tables
support keyboard and touch use. Tests verify contrast and an HTML snapshot.
Runtime behavior on a physical iPad and live Plex/arr certification remain operator
validation tasks; no live service is needed for the automated suite.
