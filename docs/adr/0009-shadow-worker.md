# 0009 — Durable workers and conservative shadow observations

Status: accepted for offline Phase 2; upstream mutation certification pending.

The store replays validated domain events. It commits each accepted event, job
revision and resulting intents in one SQLite transaction. A worker holds a
monotonic fencing token; stale workers cannot commit results. A dispatch is
marked started before contacting an upstream. Restarted mutations reconcile
instead of being blindly repeated. Ambiguous notifications remain uncertain.
Budget envelopes are reserved before a job can produce its first grab, for
both installation and viewer hourly/daily limits.

Shadow is the default. Shadow workers persist every proposed effect without
calling an upstream effect method. Observed external activity advances the
reducer only when it has exact original intent correlation. A webhook is an
observation, never an authorization grant. Duplicate or old observations cannot
be relabelled with a new intent sequence.

An external tool's report-list GET may perform its fix sweep. HTTP GET alone
does not establish read-only behavior. Observation therefore uses arr history
plus explicit fix/download correlation. A queue miss is not proof of failure, and a matching item or time
alone is not proof an external tool performed an action. Reports preserve unknown actor,
missing decode/access evidence and incomplete history as unknowns.

Shadow parity reports are local JSONL with a human-readable summary. Exact
verified bytes, positive access, reserved budget, non-destructive replacement,
and single logical notification are safety gates. Unknown evidence is a hold,
never a safe-parity success. Live deployment requires at least seven days of
shadow with zero unsafe divergences, a rollback rehearsal and maintainer
approval. This phase does not authorize deployment or contact with live services.
