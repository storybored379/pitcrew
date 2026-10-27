# Local media agent

`pitcrew-agent` is a runnable Linux diagnostic server. It reads media and optional
Plex logs; it does not change downloads, library files, mounts or Plex settings.
The control plane now runs in [shadow mode](shadow.md); certified live effect
routing and agent operational execution remain later work.

Requires Linux with `openat2` (kernel 5.6+), `/proc` mounted, and **ffmpeg and
ffprobe** on PATH (tested offline with the installed FFmpeg 5.1 tools). Missing
executables produce unavailable evidence. Unknown kernel capabilities fail closed.
The agent and subprocesses run as the same non-root user, with read permission on
both the library and final storage directories. A symlinked final storage root is
rejected; configure physical storage roots. Logical media paths may use symlinks.

```sh
cargo build --locked --bin pitcrew-agent
./target/debug/pitcrew-agent --config /etc/pitcrew/agent.toml
```

See [config.example.toml](../config.example.toml) and the example
[systemd unit](../packaging/pitcrew-agent.service). The unit uses a dynamic non-root
user; an administrator must supply the local media group and secret permissions.
[Dockerfile.agent](../Dockerfile.agent) builds the same single binary and installs
FFmpeg/timezone data, running as UID 65532. Mount config, secrets, logs and both
media trees read-only at their configured paths. The listener is loopback-only;
a container needs an ingress in the same network namespace (or Linux host
networking). Published bridge ports alone cannot reach a loopback listener.
Packaging is supplied for review; no container image or service was run in P1.

## HTTP and security

All routes require `Authorization: Bearer SECRET`, including health and legacy
aliases. The secret must contain 32–4096 visible ASCII bytes; load it from a
secret-file/env reference. The comparison hashes both values to fixed-size
SHA-256 values and uses a constant-time comparison. No query-string credentials,
forwarded identity headers or unauthenticated health exception exist. Native
mTLS is deferred; for remote use terminate TLS/mTLS at a trusted ingress in the
same namespace, authenticate clients there, and retain Bearer auth to the agent.
The binary refuses non-loopback binds, including in compatibility mode.

| GET route | Result |
|---|---|
| `/v1/health` | Process liveness and API version; no upstream health assertion |
| `/v1/evidence?path=...&decode=true` | Domain `MediaEvidence` with exact local identity, timings and outcomes |
| `/v1/probe?path=...` | `opened`, `video_streams`, `duration_s`, full evidence |
| `/v1/decodecheck?path=...` | Conservative `ok` / `bad` / `unknown`, tested ranges, full evidence |
| `/v1/file?path=...` | Symlink/target status; source read-session statistics are unavailable |
| `/v1/plexfails?minutes=60` | Correlated log failures, ambiguous timestamp interval, truncation flags |

Encode the absolute path as a query parameter. All request bodies, duplicate
parameters/auth headers, transfer encoding and Content-Length headers are
rejected. Connections close after one request. Limits: 8 KiB headers, 4 KiB URI,
3 KiB path, 32 headers, 1 MiB response. Config bounds connections, global request
rate, media concurrency, filesystem workers and request time. The global rate
bucket covers unauthorized attempts too. A saturated agent returns 429/503 or
closes excess connections without allocating more workers. Startup failures name the configuration key and a safe reason; TOML value text,
requests, paths and secrets are never logged.
Evidence responses are local sensitive data and need redaction before cloud use.

Logical paths must be beneath `media_roots`; the final symlink target must be
beneath `storage_roots`. Caller traversal and special files are rejected. Descriptor opens use
NO_MAGICLINKS and reject symlink components; manually resolved chains may pass
through aliases only when their final physical target is allowed. The agent pins a descriptor, opens beneath a pinned storage root
with `openat2`, and re-verifies the descriptor's final location and metadata after
testing. ffmpeg reads that descriptor through `/proc`, never a newly resolved
caller path. Protocol/container whitelists prevent playlists from requesting
network URLs. An unkillable filesystem operation keeps its worker slot; it is
abandoned at the deadline and never joined on shutdown. Recovery of a permanently
stuck kernel task may require restarting the agent or repairing the mount.

The identity is a local device/inode plus metadata revision fingerprint, **not a
content checksum**. It detects replacements and ordinary in-place changes but
cannot certify against a malicious writer restoring metadata. Protect media roots
from untrusted writers. Sampled decode is not whole-file validation: `tested_ranges`
report each attempted sample and its confirmed progress. A zero-length range means
no progress was confirmed before the sample failed, timed out, or became unavailable.
Successful samples do not certify untested frames. The default samples start at
0%, 40%, and 80% of the header duration, each decoding up to 10 seconds. Configure
`agent.server.decode_offsets_percent` with 1–16 strictly increasing integers from
0 to 99 and `decode_seconds` (1–300) for each sample. Unknown duration falls back
to one start sample and retains `NoDuration`; it never produces an `ok` verdict.
No-duration and slow-read evidence is retained even if decoding succeeds.
A metadata or allow-list change during testing makes the result unavailable.

The filesystem deadline combines symlink resolution and mount open, using the
smaller configured symlink/mount deadline. Reads and header/decode subprocesses
have separate configured deadlines. `decode_timeout_seconds` (1–600, default 180)
is one total wall-time budget across all sequential decode samples, including
seeking. The first failed or timed-out sample stops later samples. Missing/cyclic targets report mount liveness
as unavailable; they do not prove a mount is dead. Read offsets beyond the exact
file size are skipped so EOF is not misclassified as a slow source. Work within a request is sequential; a global
request/process cap bounds concurrent ffmpeg work. Timeout/cancellation sends a
kill to the subprocess group and reaps children; a supervisor retains its slot
until reaping completes. SIGINT/SIGTERM abort request handlers and allow a bounded
five-second supervisor drain; abandoned filesystem workers are never joined. Output
is bounded and diagnostic stderr is not exposed.

## Legacy compatibility and explicit gaps

Set `[agent.server] compatibility = true` to add `/probe`, `/decodecheck`, `/file`
and `/plexfails`. The first three retain the legacy compatibility fields. `/plexfails` returns `{count, fails}` with session/user/rating_key/path,
minutes_ago and errors, plus truncation and `omitted_ambiguous` counts. Legacy
responses omit DST-fold rows because old clients cannot represent their uncertainty;
versioned responses retain both candidates. DST folds
retain both UTC candidates; nonexistent spring-forward times are rejected. No
log path is treated as proof of an exact imported media file identity.

A **URL-only change is insufficient for clients without Authorization headers**.
The client must add Bearer auth, or use a trusted local
proxy that supplies it. The agent never enables an unauthenticated legacy mode.
`/health` (provider metrics), `/sessions`, `/tracearr-health`, and `/watchdog-jev`
return explicit 503 integration-unavailable responses. `/file.read_sessions` is
null and backend-specific booleans are omitted: P1 does not query private metrics
or the Tracearr database. This is an intentional incomplete compatibility boundary;
those methods cannot yet replace all uses of the old service.

Header probes may take longer than the legacy client's 12-second deadline on cold
storage; align its timeout with this agent or accept unknown and retry later.
`/file` currently runs the bounded generic header/read checks too. `unknown` never
means ready. The compatibility decode verdict is conservative: any no-duration,
slow-read or media fault prevents `ok`. It still only covers the reported range.

The nzbdav/InfiniDysk `SourceAdapter` normalizes read-only observations from a
fakeable transport; results explicitly include `Unknown` for the unverified
version contract. Repair is absent. A real HTTP/metrics transport needs version
certification and is deferred. Debrid and qBittorrent remain specified-only.

## Queue actors and disabled ops

[ADR 0008](adr/0008-external-queue-actors-and-ops.md) documents the completed-download
race and version-unverified exemption settings. An authoritative external failure
with a complete local mutation audit downgrades future imports to immediate import
followed by exact destination verification. Timing alone does not identify an actor.
A fake scenario removes and blocklists a held download ten seconds after completion,
then exercises the real FixJob reducer's budgeted fallback and verification path.

Mount heal, publish watchdog and matched-by-ID import confirmation are built as
explicit intent/reconciliation policies with fake tests. They are disabled and
in dry-run by default. There is **no daemon scheduler or destructive executor** in
P1: persisted leases/state, real pause-file/session collectors and certified arr
transports are needed before enabling them. The confirmer delegates solely to the
import-arbiter contract, which requires exact source verification, access and
non-destructive import authorization. No direct arr key or queue mutation exists.

## Offline verification and next shadow run

`cargo xtask ci` runs unit/property/fault tests, generated good/truncated/random/
no-duration media, log DST/rotation fixtures, HTTP contracts, queue scenarios,
external adapter isolation and domain/agent/arbiter mutation checks. The media
fixtures require ffmpeg/ffprobe even for tests; no download or external network is
used. Generic AST extraction tests select one external client class without executing
unrelated source globals. They do not claim compatibility with an installed client.

Before a live shadow run, an owner must authorize it, choose physical read-only
roots and local log directory/timezone, provision a new secret, review UID/group
permissions and limits, and choose an authenticated client/proxy. Keep production
queue tools enabled unless their exemptions are separately certified. First shadow
only read-only evidence and compare failures/latencies; do not enable ops or replace
the old sessions/provider-stat endpoints. Live certification, whole-file decoding,
remaining compatibility transports and the durable worker are still open work.
