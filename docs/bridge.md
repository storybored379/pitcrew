# Report-to-fix bridge

`pitcrew-bridge` connects trusted diagnosis records to the existing engine journal.
It never performs an upstream call. `pitcrew` runs its bounded tick when
`stack.enabled=true`; the executable still refuses live mutation mode.

The pipeline writes `diagnoses` in the private intake SQLite database. The bridge
reads these columns: `report_id` (the `web_intakes.id` integer), `decided_at`
(epoch seconds), `decision` (P4 `Decision` or `DecisionReport` JSON),
`evidence_summary` (`EvidenceFacts` JSON), `resource`, `library_id` (numeric Plex
section key), `arr_instance_id`, `arr_item_id`, `arr_file_id`, and `agent_file`
(`FileIdentity` JSON). The pipeline also owns `decider_kind`. The bridge does not
create or modify this table; an absent table is an idle pipeline. Unknown shapes
hold a report for review instead of guessing missing evidence.

The agreed redacted diagnosis does not contain access or exact-byte observation
provenance. A trusted local host must call `Bridge::record_authority` with the
current account, scoped resource, arr item/file and agent identity, observation
time, positive unexpired access, probe/decode outcome, and optionally an
authenticated channel route. The bridge persists this in **its own**
`bridge_authority` table. This method is not exposed as a viewer HTTP endpoint.
A recommendation or arbitrary intake JSON can never substitute for this record.
The trusted collector must also call the existing engine-store
`register_file(arr_instance_id, resource, &IdentityFingerprint, observed_at)` with
the exact arr file ID, size, date-added, serialized arr quality, and agent identity.
Use the arr adapter's `File.quality.to_string()` representation and the actual
observation time. Agent IDs and engine IDs have different meanings: the bridge
reads `Store::registered_file`, matches its arr/agent provenance, validates current
registry freshness, and uses the returned arr ID plus store-minted revision for
`NewJob.original`. It never registers or refreshes evidence from diagnosis JSON.
Missing or mismatched registration holds for review. The registry revision is
part of the frozen command key, including when a file changes and later returns
to the same metadata. Recovery only observes existing commits and never reinstalls
an old registry entry.

The pipeline-to-authority/registry adapter is a merge integration requirement.
Without it, the runtime honestly holds replacement recommendations for review.

A replacement requires fresh, matching evidence that the current bytes cannot
open, contain no video, or fail decoding. Whole-show reports, suspected injection,
unknown/expired/wrong-library access, future/stale evidence and identity mismatch
never create a job. Advice-only decisions never create jobs. Language and SDR
constraints pass to the engine unchanged. `hdr_ok_no_profile5` requires positive
probe evidence that the replacement does not contain Dolby Vision profile 5.
Unknown profiles and multipart/multiversion evidence remain held for review.

Before engine job creation the bridge freezes a command in `bridge_requests`
and its report association in `bridge_reports`. The command contains the exact
`NewJob`, policy envelope and nonzero cooldown. A deterministic opaque key dedupes
the same resource, identity and constraints across repeat reports and viewers.
New reservations always honor current budget caps and the stricter frozen/current
cooldown. The engine still atomically owns the resource and reserves global/viewer
budgets and cooldown. Conflicting replacement constraints or changed bytes do not steal
an active resource. Engine exclusions are checked by P3's candidate/effect router;
the bridge does not pick a release or bypass those checks.

The engine's `BrokenFileConfirmed` event carries the exact original identity.
It authorizes no side effect by itself. Later candidate selection still needs
fresh access, a budget, exclusions and the engine's other safety rails. Crashes
after engine commit but before progress projection replay the same command and
recover the same job. Stale uncommitted commands are held for review rather than
backdating a new budget reservation.

`report_progress(report_id, job_id, state, updated_at, viewer_copy_key)` is the
viewer/owner projection. An additional internal `job_revision` column prevents stale
projectors from overwriting newer engine state; timestamps cannot rewind or erase
an existing job association. The initial bridge schema upgrades add this field.
States are `received`, `needs_review`, `advice`,
`working`, `fixed`, and `could_not_fix`; copy keys are `report.<state>`.
Only reducer-reconstructed `Verified` or `Done` maps to fixed. A successful grab,
import, or download decode is insufficient. `Bridge::progress` requires the
intake account and returns no other viewer's report.

`Bridge::prepare_notifications` freezes the currently associated authenticated
routes into the P3 notification outbox. Repeat reports from the same platform
identity get one logical terminal reply. Failure prepares one admin alert and a
viewer fanout. Each fanout recipient is durably claimed before send; ambiguous
receipts remain uncertain. An audited admin resend retries uncertain recipients
without resending confirmed ones. Late joins cannot rewrite a prepared fanout.
The separate `bridge_lifecycle` outbox freezes a follow-up for each authenticated
recipient absent from that fanout, with verified ready or honest failure copy.

Received replies use the intake channel's existing durable send reservation.
Working status is available through the projection and a dedicated immutable
lifecycle command. Repeat reports from one platform identity share a working
message per job. Pending working messages become superseded after a terminal
result. `prepare_lifecycle_notifications` scans reports in bounded, persistent
round-robin order; call it after terminal notification preparation.
`dispatch_lifecycle` checks the authenticated route and adapter's proactive
policy, marks a command uncertain before sending, and confirms only an explicit
receipt. Uncertain sends are never retried automatically, including after a
restart. Shadow dispatch records `shadowed` without calling a transport. Missing
adapters leave commands pending. Terminal messages still use the P3 notifier/outbox.
A missing authenticated route stays
unprepared for administrative review, never inferred from a Plex account ID.

The admin consumer claims each owner intent once and records the result in the
owner journal. Approval reuses the same evidence, registry, access and budget
checks as automatic consumption. Denial and closure suppress creation of new
jobs for that report; an already frozen command cannot be cancelled this way.
Call `clear_authority` before refreshing a report so an observed access revocation
or failed refresh cannot leave a previous grant usable.

For bot reports, `intake_store::Store::report_route` requires the exact transport
message's durable reply reservation and a current platform-user/account link.
It uses the saved conversation and inbound time, and refuses ambiguous senders.
A linked Plex account alone cannot identify a recipient. This route reader grants
no library access; the runtime still refreshes access separately.

Offline tests cover evidence/access boundaries, deduplication, restart recovery,
budgets/cooldowns, SQL shapes, progress and terminal delivery. The drill's synthetic
scenario exercises the real message/diagnosis/engine/fake-effect/ready loop.
These tests do not certify any real platform or authorize live repair.

Bounded lower-quality approvals use a separate `bridge_lower_quality` read-side
snapshot. `record_lower_quality` accepts only trusted collector evidence tied to
the exact job revision, original bytes, arr instance/item, release and resolution
bound. Clear it before a release refresh, including a failed refresh. The runtime
refreshes current report authority first, then the exact pending release, then
consumes admin commands. A release proof older than current authority is refused.

`Consumer::with_lower_quality_shadow` explicitly enables the shadow approval
path. The consumer leases the job, checks its revision/state, positive current
access, exact broken-file authority and registry, current exclusions, and current
budget limits against the already reserved envelope. Approval expiry is capped by
access and evidence expiry; reading a saved proof never renews it. The reducer
persists the upgrade-guard intent before any effect dispatch. This path neither
fabricates guard success nor grants permission to send a live mutation. The
default consumer refuses live lower-quality requests because a certified
upgrade-guard transport is unavailable.

The resolution floor currently bounds the shadow candidate proposal. Live
lower-quality certification must additionally verify that downloaded/imported
media actually meets the floor; release metadata alone cannot prove decoded
resolution. The live lower-quality refusal remains in force until that proof and
the upgrade-guard transport are certified.

The shadow consumer also receives the current configured resolution floor. An
absent/zero floor disables approval, and a floor different from the pending bound
requires a new proposal. The stack disables this approval path when release
searches or report-runtime prerequisites are missing. This check still applies
when no runtime tick runs, so a cached proof cannot preserve a removed opt-in.
