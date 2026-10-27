# ADR 0010: diagnosis authority and report-to-job projection

Status: accepted for offline implementation; live certification remains separate.

A diagnosis recommendation is not an access grant or proof of the currently
installed bytes. The redacted diagnosis seam deliberately omits private
provenance. The bridge therefore stores trusted local authority separately and
requires an exact fresh match before creating a FixJob. Unavailable provenance
holds the report for review. The collector registers the complete arr/agent
fingerprint through P3's existing file registry. The bridge reads and validates
that registration, then freezes its normalized arr ID and store revision; a P1
agent identity alone is not an engine original-file identity. Registry revisions
also participate in deduplication so an ABA metadata sequence cannot reuse an
old terminal job.

The intake and engine databases remain separate. A frozen creation command in
the intake journal precedes the engine's transactional budget, ownership and
cooldown reservation. Replays reuse its deterministic key and exact NewJob.
Existing engine commits recover across a missed projection; stale commands that
never committed cannot create a job using an old budget timestamp.

Equivalent reports share a job only when scoped resource, current identity and
constraints agree. A differing constraint is held rather than weakening an
active repair. Progress is derived from the engine's replayed state, with fixed
reserved for verified imported bytes and target checks.

Terminal delivery uses the existing P3 intent outbox plus an immutable recipient
list. Each recipient has a durable attempt claim before sending. An ambiguous
receipt never automatically resends; an audited retry preserves confirmed
recipients. A late report may read terminal progress but cannot mutate a message
already prepared for delivery. Proactive intermediate and late-join messages need
separate immutable lifecycle commands before they can be enabled.

See [bridge implementation and seam](../bridge.md) for public APIs, exact SQL
shapes, integration requirements and remaining certification boundaries.
