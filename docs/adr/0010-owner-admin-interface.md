# 0010: Owner administration uses read-only projections and audited commands

Status: accepted for the owner interface; controller consumption of new commands
remains an integration seam.

The owner desk needs the intake report, recommendation, current repair state and
evidence without becoming a second repair engine. The existing control plane has
audited operations for exclusion release, capability reset and notification
resend. It does not yet expose report approval, quality approval, ticket closure,
template messages or exclusion pinning.

`pitcrew-admin` therefore opens the engine and intake SQLite databases read-only.
It replays job events through the domain reducer to display state. It stores
independent owner sessions and requests in its own database. Startup rejects
aliases between that writable database or its sidecars and either source database
or its sidecars. The sources must already exist.

Plex PIN sign-in proves a numeric account identity. An explicit configured
allowlist grants owner access. Every authenticated request rechecks that list;
unknown identities fail closed. Tokens are discarded after identity verification.
Random session proofs are stored as digests. Sessions expire after eight hours;
single-use login challenges expire after ten minutes. State-changing requests
require the configured Origin and, after sign-in, a session-bound CSRF proof.

For an existing API operation, the UI commits its authenticated actor, typed
command, unique request key and required reason before sending HTTP. An ambiguous
result is recorded as uncertain. A duplicate submission does not resend, and an
interrupted send becomes uncertain on restart. The control plane also records its
own transactional policy or resend audit. Browser clients cannot supply API
credentials, upstream URLs, roles, grants, recipients or arbitrary message text.

The remaining controls commit a `pending_controller` request and an audit event
in the same transaction. They do not execute an engine transition, alter a report,
pin an exclusion or deliver a message. The UI says so before submission and on the
audit page. A future consumer must supply current authorization and exact file
identity, enforce expected revision/diagnosis timestamp and all engine rails, and
reconcile crashes at its own side-effect boundary. An owner request alone is not
permission to mutate a library. Ticket closure and cancellation of in-flight
effects are distinct operations.

This boundary allows the interface to ship without introducing another mutation
path. Its limitation is deliberate and visible: newly queued controls remain
pending until the controller implements consumption. No browser-side fallback
calls an upstream directly.
