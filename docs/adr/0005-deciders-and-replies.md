# ADR 0005: Rules, optional deciders, and separate reply writers

Status: implemented library boundaries; live integration deferred. Updated: 2026-10-24.

Keep diagnosis policy, a `Decider` provider interface, and a `ReplyWriter` interface
separate. Rules decide proven conditions without an AI key. Ambiguous evidence can
be held for an administrator. Provider output never authorizes a mutation, raises
a budget, identifies a file, or bypasses a verification gate.

Implemented deciders are rules, Jev, and Anthropic. Jev is a structured decision API;
it is not a free-text reply writer. Implemented reply writers are deterministic
templates and opted-in Anthropic drafts. Templates remain live while Anthropic drafts are compared
in shadow. One decider is live per flow; additional configured providers may run
in shadow and be graded against administrator decisions. Shadow drafts are stored
for review and never sent to viewers automatically.

Use a small typed `reqwest` HTTP adapter for Anthropic's Messages API. The official
SDK list checked on 2026-10-23 has no Rust SDK. Do not label a community crate an
official client. API version and model identifiers must be explicit configuration
and contract-test fixtures, revalidated during implementation; P0 does not guess
a future model name or encode pricing. Bound timeouts, request/response sizes,
concurrency and retries, and treat malformed structured output as abstention.

Cloud use, including shadow requests, requires explicit administrator opt-in.
Before any cloud boundary, redact usernames, account IDs, IP addresses, tokens,
paths, and other identifying details into task-local opaque references. Treat
viewer text as untrusted data rather than instructions. Generated replies can only
state validated facts; a writer cannot turn pending verification into “ready.”
Logs must not contain unredacted requests or provider error bodies. The config
crate checks consent and provider selection; the P4 library adds a mandatory closed-fact projection before any provider request.
All free text is omitted, including viewer words. The host supplies configuration
and logging; the existing binary does not start provider traffic automatically.

A future promotion requires at least 50 graded decisions, at least 95% agreement
on action, zero unsafe decisions, and administrator approval. The reply-writer
choice remains open until shadow drafts have been reviewed. Provider failure must
leave rules/templates usable; private household-voice integrations are outside
this public product.

Source: [Anthropic official SDKs and API links](https://platform.claude.com/docs/en/cli-sdks-libraries/overview).

P4 details and explicit integration limits: [diagnosis](../diagnosis.md). Provider
contracts use synthetic transports only; model availability and live behavior are
not certified by offline tests.
