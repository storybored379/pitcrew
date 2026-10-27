# ADR 0001: One control plane, four process boundaries

Status: accepted for the foundation. Date: 2026-10-23.

A repair crosses intake, authorization, evidence, downloading, import, verification,
and notification. Independent sweepers cannot safely infer ownership from queue
state or release-group names. One control plane must own that lifecycle.

`pitcrew` owns cases, the fix engine, import arbiter, database, outbox, diagnosis,
and eventual administrator UI. `pitcrew-web` is public intake and holds only its
session and control-plane credentials. `pitcrew-bot` translates configured conversational channels into
control-plane requests. `pitcrew-agent` performs authenticated, bounded file checks
next to media storage. Private deployment extensions belong outside this repository.

The public product uses these names consistently; historical private names in the
source design are not binary names or configuration defaults.

The workspace separates these responsibilities:

| Crate | Responsibility |
| --- | --- |
| domain | Pure lifecycle, constraints, evidence identity, effect intents |
| store | SQLite transactions, event journal, reservations, leases, outbox |
| arr / plex | Typed upstream protocols and normalization |
| engine | Drives committed intents, deadlines, reconciliation |
| arbiter | Sole owner of manual import, queue removal, and blocklist decisions |
| diagnosis / deciders | Evidence and rules / optional provider adapters |
| notify | Templates, delivery adapters and retry results |
| config | Versioned config, validation, secret references |
| fakes / scenarios | Test upstreams and executable acceptance fixtures |
| xtask | Local and CI verification entry point |

Dependencies point inward: domain knows no HTTP, database, clock, filesystem, or
provider. The reducer receives an explicit timestamp and emits transition plans.
The store atomically commits these plans; only committed intents reach adapters.
Only tests depend on fakes. Crates with future responsibilities are deliberately
empty boundaries in P0/P0.5; four binaries print their pre-alpha status and exit.

Every integration is optional. Empty config supports setup; useful repairs need
Plex and at least one arr plus web or bot intake. Rules and reply templates must
work without cloud keys. Unknown evidence or authorization yields a hold/failure,
never an inferred permission. Cloud models propose decisions; rails remain in code.

Webhook intake and deadline ticks converge on the same reducer. Per-job leases
and a per-media ownership fence prevent duplicate workers; blocking I/O and decode
run outside database transactions. Decode concurrency is bounded across jobs.
A timed-out decode must not hold other jobs hostage. No daemon or network listener
is implemented or deployed in P0.5.

Consequences: more explicit adapter interfaces and reconciliation work, but a
single auditable mutation path and independently testable domain. A separate
sweeper may discover work, but must submit it to the same arbiter, never mutate arr
behind its back. Distributed control-plane writers and network-mounted SQLite
are outside the initial deployment model.

## Storage-agnostic evidence and optional source adapters

A media file is an exact content identity plus an observed storage chain:
`library entry → zero or more symlinks → mount → optional remote source`.
Local downloads are the zero-symlink, ordinary-filesystem case. WebDAV/rclone mounts,
usenet streaming and cached or uncached debrid are peers, not special success paths.
Paths and backend identifiers stay on the installation; cloud diagnosis receives
only redacted evidence. A release name or release group is never content identity.

The agent's mandatory core contracts are independent of source software:

| Check | Evidence to preserve |
| --- | --- |
| Symlink traversal | Every hop, dangling target, cycle/depth limit, resolved-root confinement |
| Mount liveness | Mount identity, elapsed time, success or explicit timeout; no unbounded stat |
| Timed reads | Offset, requested/returned bytes, elapsed latency, throughput, timeout or I/O failure |
| Header probe | Parsed video/audio facts, missing stream, no duration, elapsed time and failure |
| Decode | Exact content revision, tested ranges, bounded elapsed time, corruption/no-duration/slow-read/timeout evidence |
| Plex logs | Matched media identity, normalized timestamp/timezone, failure category and bounded excerpts |

A readable header is not a passing decode. Offset reads must include samples beyond
the header, with total byte/time budgets; no-duration and slow reads remain concrete
evidence rather than disappearing into “unknown.” No generic check promises to prove
every byte of an arbitrarily large file. Mount/read deadlines and decode concurrency
must isolate a slow source from other jobs. Filesystem traversal and decode execution
remain agent implementation work; P0.5 supplies serializable evidence and contracts.

An optional `SourceAdapter` adds `facts(file)` and may advertise a `SourceRepair`
capability. Facts are read-only diagnostics; repair is a separately authorized,
budgeted, journaled side effect whose result may be uncertain. No adapter is needed
for the minimum install, and source failure cannot bypass the generic checks.

| Specified adapter | Facts to normalize | Optional repair boundary |
| --- | --- | --- |
| nzbdav / InfiniDysk | Download/content identity, provider misses, source availability, streaming health | Provider-supported repair only when capability and exact content are established |
| Debrid via rdt-client, Decypharr or Zurg | Account/cache presence, torrent infohash, selected file, cached/downloading/missing status | Re-add/refresh only with explicit permission and duplicate/budget protection |
| qBittorrent | Torrent identity, completion, file availability, bounded progress/rate observations | Supported recheck/resume; never automatic deletion or a hidden second grab |
| SABnzbd | Queue/history download identity, completion, failure, unpack/repair evidence | Explicit supported retry/repair, spending the same repair budget |

These are specifications, not built integrations. Verify current official APIs,
versions, authentication, response shapes and repair semantics when implementing each
adapter. No inference about today's remote account or server state is made here.

Failed-release exclusions retain every available immutable identity: release GUID
scoped to its indexer/instance, torrent infohash, and download ID scoped to its client.
A renamed cached torrent must still match its infohash. Normalize case and known
identifier formats at the adapter boundary; retain alias relationships transactionally.
Do not deduplicate unrelated clients' opaque IDs globally. Group exclusions remain
optional selection hints and never replace content-based exclusions.

Per-client deadline profiles prevent cached sources from imposing their expectations
on ordinary torrents. Cached debrid/usenet streaming/local defaults allow 180 seconds
for downloading, 120 for import and 180 for decode; uncached torrents allow 3,600
seconds for downloading. A fresh-import bad decode waits 180 seconds before recheck.
Every value can be overridden per client and remains bounded by the job total deadline.
These are conservative product defaults, not performance guarantees.


## P1 implementation status

The generic Linux agent now implements descriptor-verified final-target roots,
bounded abandoned filesystem workers, timed reads, ffprobe and bounded decode,
and timezone-aware rotation-safe Plex log snapshots. Local HTTP is authenticated,
loopback-only and bounded. No upstream connection is necessary. The nzbdav adapter
normalizes facts from a fakeable transport; version-certified network transport
remains deferred. Sampled decode and metadata identity limits are documented in
`docs/agent.md`; no whole-file or hostile-writer integrity guarantee is claimed.
Queue interference and disabled ops policies are in ADR 0008. Subsequent work
added the durable store, control plane, viewer/channels, diagnosis, owner desk and
fake-tested effect router. Current integration and certification status is tracked
in [the stack guide](../stack.md) and [certification](../live-certification.md).
