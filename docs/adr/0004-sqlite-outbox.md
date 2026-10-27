# ADR 0004: SQLite through SQLx, transactional intents and outbox

Status: implemented for offline Phase 2; upstream mutation certification pending. Date: 2026-10-23.

Use SQLite through SQLx with checked-in, ordered, immutable SQL migrations. The
single control-plane installation owns its database on local durable storage.
Enable foreign keys, WAL, `synchronous=FULL`, and a bounded busy timeout explicitly.
Keep write transactions short; no HTTP or decoding while holding a transaction.
SQLx exposes these connection options, and SQLite permits one writer at a time.

The Phase 2 schema represents immutable job specifications (including reporter,
resource and constraints), accepted events, budget reservations, media ownership,
worker leases, effect outbox rows and an authenticated observation journal. Case
management is in the separate intake journal. Later migrations add immutable
notification payloads, recipient-level dispatch attempts and audited resends. Constraints belong in
columns or versioned typed payloads, never loosely related key/value flags.
Migrations must preserve outstanding intents and permit replay; do not silently
reset a job to Picked after a schema upgrade.

One immediate write transaction must:

1. Acquire/check the job revision and worker fence; verify the current media owner.
2. Revalidate authorization for a mutation and atomically check/reserve the global,
   viewer, item/cooldown and job budget scopes. P0's job budget envelope is not a
   substitute for these installation-wide checks.
3. Commit the event, resulting state/deadline, immutable constraints and attempt,
   plus all resulting intents. Initial report/reporter/constraint publication is
   atomic, so a worker cannot see an unlinked repair.
4. Insert each intent with a unique `(job_id, sequence)` key. A terminal job's admin
   failure intent also has a unique semantic `(job_id, notification_kind)` key.

A worker claims a committed intent using a lease and fencing token, records the
attempt, then calls upstream outside the transaction. On restart, an intent that
might have executed becomes reconciliation work. A lost POST response is not
permission to POST again. Check the exact release, download, queue (including
blocked/completed entries), command and import history. A queue miss alone cannot
prove absence. The same rule applies to DELETE and asynchronous ManualImport.
A failed job with an unresolved mutation retains media ownership until reconciled
or explicitly handed off to an administrator.

The outbox promises exactly one **logical notification intent**, with retryable
attempts. It does not promise exactly one delivery to a service without an
idempotency key. On ambiguous delivery, use downstream idempotency/reconciliation
where possible; otherwise expose the uncertainty and document the duplicate/loss
tradeoff. An uncertain ready send without downstream deduplication must remain
reconciliation/review work, never be sent a second time automatically. Persist the
logical `(job, purpose, recipient)` key and an incrementing dispatch attempt before
sending. A callback must match both; a late rejection cannot turn a newer uncertain
attempt into “unsent.” Confirmed state is monotonic. The store persists the logical outbox and durable started/uncertain/completed
status. Subsequent notification migrations also persist recipient-level attempts
and administrator-requested resends. Physical delivery still needs certification.
Never set `delivered_at` merely because a job became terminal. Preserve
audit evidence and bounded backoff, including an administrator-visible dead-letter
state. Failed delivery must not erase the pending intent.

Use SQLx's migration runner; validate migrations against a temporary database in
tests, including concurrent reservation and lease races, rollback, kill/resume,
and persisted outbox retries. Prefer runtime typed queries initially so CI does
not need a live database; if query macros are introduced, commit offline metadata
and verify its freshness. Phase 2 implements the journal with SQLx 0.8 and embedded immutable migrations.
Job restoration runs the constructor and every committed event through the pure
reducer at its original timestamp, checking contiguous revisions and job identity.
The specification and events are read in one SQLite snapshot even while another
connection commits. No unchecked `FixJob` snapshot is loaded. SQLite triggers reject journal updates
and deletes. Generic observations deduplicate by source and key; changed payloads
under the same key are rejected instead of silently losing conflicting evidence.

### Phase 2 reservation and worker semantics

Creation atomically reserves the full job envelope against installation-wide and
per-viewer rolling hourly/daily limits. Every grab consumes the domain envelope
before its outbox row is committed. An active or quarantined job's entire envelope
continues counting even after its original timestamp leaves a rolling window;
each new grab refreshes the envelope timestamp. Terminal, safely released envelopes
age out from that last spending time. Ambiguity never refunds units. This is
intentionally conservative: unused units are not refunded, and a long unresolved
job can continue blocking new work. Subsequent policy migrations enforce item cooldowns through
`create_job_with_cooldown`; callers must use its nonzero configured cooldown
when creating autonomous jobs. Replay must not bypass existing reservations.

`BEGIN IMMEDIATE` serializes writers across independent store connections and
processes. A lease cannot be taken while unexpired, even by a worker using the same
owner label. Every acquisition increments a fencing token. Event commits, intent
starts and completions validate owner, fence and exact expiry against the durable
lease. Claiming a pending intent also compares the exact job revision the worker
used to validate dispatch, closing the read/claim race with incoming observations.
Recorded shadow replay keeps the original event timestamp separate from the current
processing clock used for lease validity. Successful replay can release its lease
with the current fencing token; a stale holder cannot release a successor's lease.
A crashed replay's lease expires in processing time, so historical input cannot
permanently lock itself out on retry.
Fencing prevents stale database writes; upstream adapters must additionally
recheck authorization before dispatch and reconcile effects that might already
have crossed the network boundary.

The outbox has pending, started, completed, uncertain, suppressed and shadowed
states. Started intents are never claimed again. Engine recovery either commits a
correlated result and completes the old intent atomically, or leaves uncertainty
for reconciliation/review. Terminal transitions suppress earlier unstarted effects
before creating their terminal notification intents. Media ownership remains held
when either an active download or an unresolved mutation survives terminal failure.
A generic store event does not by itself establish that a side effect ran: workers
also validate each effect against the latest reducer state before dispatch.

Tests migrate every historical migration prefix, reopen populated databases,
exercise concurrent reservations, force rollback after journal writes, and kill
subprocesses at each observed suspension of real create/transition/start/result
futures. Restart must recover the full commit or no commit; a started request
remains started. Engine tests separately exercise named deterministic await
boundaries around dispatch and recovery. Property tests generate mixed viewer
reservation sequences and assert that accepted envelopes never exceed a cap.

Sources:

- [SQLx SQLite connection options](https://docs.rs/sqlx/latest/sqlx/sqlite/struct.SqliteConnectOptions.html)
- [SQLite transactions](https://www.sqlite.org/lang_transaction.html)
- [SQLx migrations](https://docs.rs/sqlx/latest/sqlx/migrate/index.html)
