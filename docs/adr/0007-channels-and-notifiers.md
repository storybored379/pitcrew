# ADR 0007: Conversation channels, generic identity links and one-way notifications

Status: accepted for P0.5; extends ADR 0003. Date: 2026-10-23.

A report's conversation is not a Discord session. Keep cases, identity, authorization,
reply policy and delivery state in the control plane. A `Channel` adapter receives
normalized messages, replies to a received message, and sends a proactive follow-up.
It declares plain/rich text and button support, message-size bounds, and proactive
messaging requirements. Opaque user/conversation/message IDs and delivery recipient references are scoped by channel
instance; identical strings on two transports never establish the same identity.
Inbound platform message IDs deduplicate repeat delivery before opening a case.

Capabilities include “user must message first” and a bounded window after the most
recent qualifying inbound message. The engine checks capabilities at dispatch, not
just when a future reply is queued. Expired windows produce an explicit undeliverable
or uncertain result and administrator visibility, not an unauthorized send. Adapters
must revalidate actual platform rules when built; the schema can represent first-contact
restrictions and a 24-hour window without assuming every platform uses one. Any
approved-template exception must be represented explicitly before it can bypass a window.

| Stage | Channels |
| --- | --- |
| v1 | Discord (Twilight), Telegram, web, Seerr issues |
| v1.x | Email, Matrix |
| Later/community | Slack, paid SMS, WhatsApp through its official API only |
| Community/private bridges | Signal and iMessage; not core supported transports |

Twilight remains the Discord choice from ADR 0003; it is an implementation detail of
that adapter. A bot binary may host several channel adapters, and web/Seerr ingress
uses the same normalized identity and conversation contract. No live transport ships
in P0.5. The optional config model is the declaration of desired adapters, not a claim
that an accepted config starts them.

## One identity-linking flow

1. The viewer signs into the web using Plex and passes membership/access checks.
2. The control plane creates a short, random, single-use, expiring challenge tied to
   that verified Plex principal. Store a keyed digest, not the plaintext code.
3. On any channel, the viewer sends `link <code>`. The authenticated transport supplies
   its own scoped sender identity; never accept a sender ID embedded in viewer text.
4. Consume the challenge and create the identity link in one transaction. Bind it to
   the intended channel instance where known. Enforce expiry, attempt limits, unique
   consumption and explicit account-conflict handling. Do not silently steal a link.
5. Replies and proactive updates resolve through that link; every media mutation still
   requires a fresh positive Plex library-access snapshot. Linking alone grants none.

Web-only viewers need no bot. Short-code generation, keyed hashing, throttling,
Plex sign-in, transactional storage and link revocation are later implementation work;
P0.5 represents the challenge/consumption rules and contracts. Codes, tokens, platform
identifiers and message text never appear in public logs or cloud evidence.

## Admin alerts are a separate boundary

`Notifier` is one-way delivery, without conversation receive/reply or identity-linking
requirements. Initial targets are ntfy, Pushover, email and webhooks, independently
optional. A notification request carries a logical delivery key, destination reference
and rendered content. Secret references resolve in the adapter; parsing config and
building a request perform no network I/O.

Persist each logical notification once, with an idempotency key scoped to job,
purpose and recipient. Transport receipts and attempts are separate from that intent.
Use at-least-once delivery where the adapter/receiver can deduplicate the stable key;
a retry never invents a new logical message. Confirmed delivery cannot regress because
an older timeout arrives. The admin UI must expose **delivery uncertain** when the
transport cannot prove acceptance. The accepted administrator recovery is a one-tap
resend on that flagged job, explicitly triggered by the administrator; there is no
automatic duplicate. Persist the explicit decision and transport attempt under the
same logical ready intent. The admin UI and its resend endpoint are a later phase.

“No second ready for the same job” requires more than a unique outbox row. For an
ambiguous ready send to a transport with no idempotency support, do not blindly retry:
reconcile by receipt/message history where possible, otherwise retain uncertainty for
administrator review. This is the unavoidable boundary between at-least-once attempts
and duplicate-free visible messages on such transports. Definite non-delivery may be
retried. Never convert uncertainty to delivered, failed or a second ready just to clear
an outbox row. Terminal admin alerts still have one logical intent; their physical
transport retries use the same key. See ADR 0004 for transaction requirements.

## P5 implementation note (2026-10-24)

The intake phase implements the web PIN flow, short-code linking, Twilight
Discord DMs, Telegram private messages, Seerr issue comments and an authenticated
loopback drill ingress. `intake-store` is a separate SQLite journal so new identity
and conversation records do not alter the repair engine's schema. Short codes
use HMAC-SHA256 with a private companion key file, durable attempt limits and
atomic single-use consumption; account conflicts do not silently reassign links.
Both web and channel paths refresh Plex access before selecting catalog media.

Reply transport claims are persisted as uncertain before dispatch. An ambiguous
send is retained for review rather than blindly retried. This implements intake
reply protection, not the engine's verified-ready outbox or administrator resend
interface. The catalog remains a configured snapshot, and accepted reports await
review until the diagnosis/repair bridge is integrated. The synthetic corpus and
fake transport tests certify those bounded contracts only. See
[implementation limits](../intake-drills.md).
