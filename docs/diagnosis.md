# Diagnosis and optional deciders

`pitcrew-diagnosis` collects local evidence and returns typed recommendations.
`pitcrew-deciders` applies a mandatory redaction boundary, runs a selected primary
and optional comparison providers, and supplies replies. Neither crate executes
library mutations. Access, file identity, budgets, durable intents and replacement
verification remain the engine's responsibility.

## Evidence and rules

The P1 agent collector obtains media evidence and bounded Plex log snapshots.
Stale, future-dated or identity-mismatched media observations are unavailable,
never proof of a broken copy. Plex log paths supply context; they cannot establish
an exact file revision. Optional session collectors preserve unavailable telemetry
as unknown. The Tracearr integration uses the recorded read-only metrics bridge
contract, not an assumed native Tracearr API. Native Tautulli reads use its API
commands. All tests use synthetic in-process transports.

Rules port the wizard's server-side answer validation and ordered playcheck
logic: unreadable or undecodable media; stale Plex analysis; Dolby Vision
compatibility; slow source; 4K playback advice; client advice; audio tracks; and
conservative escalation. Whole-show reports require an exact episode before a
replacement recommendation. The reference wizard's `autofix` terminal is retained
as routing information, but a viewer complaint alone is not proof of broken bytes.
Rust parity tests replay ten recorded replacement-constraint expectations from
the existing pure 4K (T92) fixture and two playcheck wizard answer bodies. A
synthetic table covers every current wizard leaf plus invalid paths. These tests
do not claim complete legacy HTTP/UI or side-effect parity. An everything-broken
complaint requires proven server unavailability for urgent classification;
unknown health escalates normally and healthy-server reports run playcheck.

## Mandatory cloud boundary

Construct `LocalEvidence` from normalized `EvidenceFacts` and private local
context. `redact` drops **all** arbitrary text, including viewer words, names,
paths, tokens, IDs, titles and raw log strings. Only finite enums, booleans and
measurements survive, including correlated Plex error counts and explicit
completeness status and rejected timestamp counts. Log counts never become
stale-analysis or exact-file proof.
`RedactedEvidence` has private fields, no deserializer and
no mutable accessor. Both rules and configured cloud arms receive this exact
projection. Raw-text interpretation must happen at local intake before this
boundary; cloud deciders do not receive sanitized-looking free text.

The default `DecisionRunner` uses rules without credentials. A configured runner
selects one primary and compares all other arms on the same facts. Successful
shadow results never override the primary. A failed primary falls back to rules;
a failed shadow is recorded without canceling a usable primary. The JSONL
comparison contains only typed decisions, provider kinds and accounting, never
raw upstream error bodies or local context. A failed comparison write withholds
the selected recommendation. The host owns the sink's storage and retention.

## Providers

Four provider families exist; every one receives only the redacted facts and
can only recommend:

| Family | Servers | Notes |
| --- | --- | --- |
| Rules | built in | Default; no credentials, no network |
| Jev | TypeSafe `systemone` | Cloud |
| Anthropic | Claude Messages API | Cloud; native adapter |
| OpenAI-compatible | OpenAI, DeepSeek, Gemini and Groq compatibility endpoints, OpenRouter, any custom server; local Ollama, LM Studio, llama.cpp, vLLM | Presets supply endpoint and request dialect; the administrator chooses the model |

OpenAI-compatible profiles differ only in configuration:

- **Structured output.** `json_schema` (strict schema enforced by the server:
  OpenAI, self-hosted Ollama and most others), `json_object` (valid JSON only;
  DeepSeek, with the schema in the prompt) or `prompt_only` for servers that
  reject `response_format`. Enforcement is never trusted: every answer is parsed
  with unknown fields denied, range-checked and passed through the same
  replacement guard as Claude and Jev. Truncated, refused, tool-calling or
  multi-choice answers fall back to rules. One surrounding code fence is
  tolerated because local models often add it.
- **Token limit field.** `max_completion_tokens` (OpenAI's newer models) or
  `max_tokens` (everything else).
- **Local vs cloud.** Cloud profiles need `cloud_opt_in`, an API key and HTTPS.
  Local profiles may use plain HTTP without a key and need no cloud consent,
  but the endpoint must be a loopback, private (RFC 1918, CGNAT/tailnet, ULA,
  link-local) or single-label/`.lan`/`.local`/`.internal`/`.home.arpa` host; a
  public host is rejected even if marked local. Local servers may omit usage;
  the call is counted as started with no usage record.
- **Usage.** OpenAI's `prompt_tokens_details.cached_tokens` and DeepSeek's
  `prompt_cache_hit_tokens` are split out of input tokens and accounted as
  cache reads, so cached input is never priced at the uncached rate.

Several profiles can run in shadow at once (for example a local model next to
GPT and Claude); the comparison log records each result under the
`openai_compatible` kind. Replies from any model remain private drafts; the
delivered text is always a template.

### Jev and Claude details

Jev uses the TypeSafe `systemone` request shape: `model`, `state`, and typed
`questions`. Responses are validated and mapped into the common finite decision
vocabulary. A replacement recommendation preserves deterministic language and
dynamic-range constraints. Whole-show, injection and non-playback routes cannot
be turned into replacements by a provider; unsupported audio-track claims also
fall back to rules. Claude uses the Anthropic Messages API through the shared
bounded
HTTP transport, with a JSON schema under `output_config.format`. API version and
model are explicit configuration. Defaults are `claude-sonnet-5-5` and API version
`2026-10-21`; the named cheaper option is `claude-haiku-5-5`. Model defaults follow
the assignment; availability has not been tested with a real request.

Provider construction requires explicit cloud consent, including shadow use.
Endpoints require HTTPS. Requests have byte, response, token, time and concurrency
bounds, no automatic retries or redirects, and no ambient proxy use in the
production HTTP transport. No provider call was made while developing this lane.

Usage counters include input, output and cache token classes. An optional estimate
uses administrator-supplied rates; no current price is hard-coded. Started calls
without reported usage may have been billed. Both input and output token counts
are required by the recorded/public provider envelope; missing or malformed
usage triggers rules fallback and remains unaccounted rather than becoming zero
cost. Valid usage is retained even when
the structured decision is rejected. Process counters are not a durable billing
ledger: the host must persist accounting snapshots if it needs history. A host
can retain an `Arc<JevDecider<_>>` or `Arc<ClaudeDecider<_>>` and pass a clone to
`DecisionRunner`; shared deciders implement the same trait, keeping accounting
readable even after a failed call falls back to rules.

Contract sources: [TypeSafe response schema](https://api.typesafe.ai/redoc),
[Messages API](https://platform.claude.com/docs/en/api/messages/create),
[structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs),
and [API versioning](https://platform.claude.com/docs/en/api/versioning).

## Replies and integration status

Templates are the deliverable reply writer. The owner display name is local
configuration, defaulting to “the server owner”; it never enters a cloud request.
Copy stays brief and informal, avoids technical detail and promises, and does not
claim notification delivery just because escalation was recommended.

A ready reply can only be constructed from a `FixJob` in `Verified` or `Done`:
the exact imported file has decoded and the target check has passed. A successful
download decode, an imported-but-unchecked file, or a model decision cannot produce
that reply. A viewer's self-resolution has distinct wording.

`ClaudeReplyWriter` requires a separate enable flag plus cloud consent. It returns
a template for delivery and an untrusted Claude draft for private review. Draft
errors leave the template usable. Drafts have no Debug/Serialize implementation
and never replace `delivery_text`; promotion to generated live prose remains
pending the review/certification required by ADR 0005.

The supervised runtime can opt into per-diagnosis model routing as described in
[Optional model scheduling](pipeline.md#optional-model-scheduling). Defaults remain
rules only. Scheduled models have bounded calls and a durable, non-retrying attempt
journal that preserves known usage even when post-call authority checks fail.
Live execution still requires its separate certification gate. Generated replies
remain private drafts; enabling a decision route does not change delivered text.
