# ADR 0008: External queue actors and bounded operational jobs

Status: accepted for P1 pure policies and fake scenarios; production mutation dispatch deferred.

## External actors are evidence, not ownership

An arr queue can have several writers. A download disappearing is not proof that
Pit Crew deleted it, that the source failed, or that the library imported it.
`domain::queue` attributes an external removal only when all of these hold:

- The download identity matches our immutable owned download.
- Authenticated history records a completed download subsequently marked failed,
  removed and blocklisted. A missing queue row or request timeout is insufficient.
- The durable local mutation journal, including claimed effects with uncertain
  responses, proves no matching Pit Crew mutation. An incomplete audit cannot
  establish external attribution.
- A named actor comes from authenticated actor/audit evidence. Without that evidence
  the actor is `UnknownExternal`; a ten-second delay never identifies a product.

Store the evidence and the downgrade to `ImportImmediatelyThenVerify` with the
client/job capability record. Do not automatically restore holding after a single
successful download. The corresponding authoritative failure ends the current
FixJob wait, excludes that release and spends the existing reservation; it does not
refund or silently repeat the grab.

The executable fake scenario in `crates/fakes/tests/queue_actor.rs` completes a held
download at second 30, removes and blocklists it at second 40, attributes the
interference, and reserves another release. Independent arr handling imports the
next download immediately, before cleanup; the reducer still requires exact
imported-file decode and target checks before creating its sole ready intent.

### Exemptions to certify for each installed version

These are desired configuration semantics, **not verified setting names or supported
version claims**. Each installed version needs a recorded offline contract/certification
result before a real adapter reports holding isolation. Never modify another tool's
settings silently or assume a category name is an exemption.

| Known actor | Required exclusion to investigate and certify | P1 status |
| --- | --- | --- |
| nzbdav / InfiniDysk QueueRules | Exclude Pit Crew's dedicated client/category/tag from queue rules that remove, blocklist or search after an import warning; prove the rule also excludes already imported downloads | Unverified for every version |
| Cleanuparr | Exclude the owned category/tag/downloads from failed, stalled and unwanted-item removal and replacement searches | Unverified for every version |
| Decluttarr | Exclude the owned category/tag/downloads from queue removal, blocklisting and search rules | Unverified for every version |
| Debrid tools (including download bridges and queue cleaners) | Exclude the owned category/tag and staging source from cleanup/removal/retry; retain source identity through import | Unverified for every version |
| Other arr importers / custom scripts | Exclude the owned category/tag/staging source from both mutation and automatic import while holding | Unverified for every version |

When exclusion cannot be proven, degrade to immediate import followed by destination
verification. That route is **not** authorization for Pit Crew to delete an unverified
old library file. It can observe ordinary independent arr auto-import, whose weaker
preservation guarantee is already documented in ADR 0002. A Pit Crew initiated
immediate import would instead need a certified non-destructive copy/staging contract
that retains the old bytes until the new destination verifies. That executor is
explicitly deferred; the existing verified-source ManualImport rail is unchanged.

## Operational jobs

`agent::ops` supplies deterministic planners for mount self-heal and Plex publish
recovery. Both are disabled and dry-run by default. They use explicit `Watching`,
`Intent`, `Reconciling` and `NeedsAdmin` states. Persist every returned state and
intent atomically before dispatch; recovery replays accepted observations, not
unchecked deserialization. Concurrent workers require a per-resource lease and
compare-and-swap revision in the later store/worker implementation.

Both planners enforce two distinct consecutive unhealthy checks, a readable absent
pause file, positively zero active sessions, cooldown, and a reserved UTC-calendar-day
budget. Healthy, unknown, paused or disabled checks break the failure streak.
Unknown sessions and unreadable pause state block mutation. Defaults are 15 minutes
between mount repairs, 30 minutes between publish restarts, and four reserved
operations per day. Failed or ambiguous operations retain that reservation. Clock
regression is rejected; the same timestamp cannot count as two bad checks. Dry-run
returns the proposed operation and reserves nothing.

The publish adapter must supply an authenticated exact-server resource observation:
healthy means the server is present, advertises the server capability and has a
remote connection. Unreachable/malformed resources are unknown, never a restart
signal. Session and pause observations must be refreshed at dispatch to prevent
interrupting a viewer who started after planning.

Intent creation is not an imperative restart call. An ambiguous response or process
restart enters reconciliation, never an automatic duplicate. Fresh post-action health
resolves recovery. Confirmed failure enters `NeedsAdmin` with one logical admin
notification intent; repeated failure observations cannot create another alert.

### Matched-by-ID imports

The confirmer only submits a verified proposal to `ImportArbiter`. It has no arr
HTTP client, token, command runner or bypass. Admission requires current positive
library access, exact owned download, import-blocked state whose only warning is
matched-by-ID, positive matching series IDs, no proposal rejections, non-sample
content at least 70 MiB, a bounded filename episode set exactly matching all proposed
episodes, and a successful decode of the exact source revision. Paths, ambiguous
multiple episode tokens and duplicate proposed episodes are rejected.

The arbiter receipt says queued, uncertain or rejected; it is never proof of an
import or playable file. Production arbiter admission must retain the FixJob's quality,
language/SDR constraints and ownership fence, deduplicate by job/source, persist its
ImportIntent, and dispatch only through the existing verified-source contract. An
uncertain receipt requires reconciliation. Dry-run validates and returns the same
proposal without calling even the arbiter.

### Explicit P1 deferral

No privileged mount/unmount, service restart, live Plex-resource/session collector,
pause-file poller or arr command executor ships in this phase. Those actions need the
transactional store, durable worker, per-resource leases, dispatch-time rail refresh,
secret references and version-specific adapter certification. Binding a shell command
to a pure plan would skip those safety requirements. The planners and fake admission/
fault tests are built and exercised offline; enabling desired ops config alone never
starts destructive work. This limitation must remain visible in packaging/config docs.
