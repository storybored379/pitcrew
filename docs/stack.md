# Running the shadow stack

The [README](../README.md) starts the control plane and viewer from one generated
configuration. Each component is a separately supervised process sharing local
SQLite journals. The agent is optional and runs beside the media files. Channels
and the owner desk are optional processes. Shadow mode records proposed repair
effects; it never executes the engine's upstream mutations.

## Configuration checks

`pitcrew init` writes a commented TOML configuration, secret files and a private
state directory. See [setup](setup.md). It runs the existing doctor against local
files. Doctor without `--connectivity` never creates databases, binds listeners
or contacts an upstream:

```sh
pitcrew doctor --config runtime/pitcrew.toml
pitcrew web --config runtime/pitcrew.toml --check
```

`pitcrew doctor --config FILE --connectivity` explicitly checks Plex library
access, arr system status and configured agent health with authenticated GETs.
Each request is bounded to five seconds and 64 KiB. Redirects, ambient proxies
and retries are disabled; a local secret failure prevents all requests. It sends
no channel/notifier messages, calls no models, reads no media bytes and does not
certify replacement safety. Errors identify configuration keys without exposing
credentials or upstream response bodies.

## Compose

The Linux [Compose file](../packaging/compose.yaml) uses host networking because
the binaries require loopback listeners. All services share the stack image,
read-only configuration and private secrets. The state directory is writable only
where needed. The root filesystem is read-only, capabilities are dropped and the
container UID/GID matches the account that created the private files.

The README creates `.env` with `PITCREW_HOME`, `PITCREW_UID` and `PITCREW_GID`.
[env.example](../packaging/env.example) lists image and healthcheck overrides.
`PITCREW_HOME` must be absolute. The generated config uses `/config` paths when
initialized with `--runtime-root /config`; the host directory is mounted there.
The setup command runs as your non-root account, so its 0600 files are readable
by the same numeric account in Compose. Do not make tokens world-readable.

```sh
# Validate interpolation without starting containers.
docker compose --env-file .env -f packaging/compose.yaml config --quiet
# Default services: control and web.
docker compose --env-file .env -f packaging/compose.yaml up -d --no-build
# Explicit read-only connectivity checks, when you want them.
docker compose --env-file .env -f packaging/compose.yaml run --rm --no-deps \
  control pitcrew doctor --config /config/pitcrew.toml --connectivity
```

Control health checks its local database. Web and owner checks test the local
HTTP entry page. Agent liveness expects an unauthenticated 401 response; an
unhealthy API requires authenticated doctor checks to diagnose. Channels have no
HTTP health endpoint and do not inherit the control check. If listener ports
change, change the corresponding `.env` healthcheck ports too.

Images are multi-stage builds; the runtime has binaries, certificates and the
needed diagnostic tools, without the Rust toolchain. The shared image includes
ffmpeg, ffprobe and timezone data. `Dockerfile` and `Dockerfile.agent` are smaller
component-specific alternatives. There is no published image at this pre-alpha
stage. `PITCREW_IMAGE` can select a reviewed future image by digest without
changing the service definitions. Use `up --no-build` to avoid rebuilding it.

### Optional media agent

Add `[agent]` and `[agent.server]` from
[the annotated example](../config.example.toml), create its private shared-secret
file, and configure exact roots. The example Compose mounts `/media` and
`/storage` read-only. Set `PITCREW_MEDIA` and `PITCREW_STORAGE` to absolute host
paths, or supply a Compose override with the **same absolute paths seen by Plex
and arr**. Include final symlink destinations in both the mounts and allow-list.
An arbitrary new container path will not match upstream file identity.

```sh
docker compose --env-file .env -f packaging/compose.yaml --profile agent up -d --no-build
```

The agent must be able to read those files as the configured UID/GID. Use a
reviewed `group_add` override if storage requires a supplementary read group.
Keep media mounts read-only. A remote agent instead needs protected ingress and
its own local configuration. See [agent operations](agent.md).

### Optional owner desk and channels

Add the `[admin]` section from [owner desk configuration](admin.md). For the
Compose layout use `/config/state/admin.sqlite`, `/config/state/pitcrew.sqlite`
and `/config/state/intake.sqlite`, with a separate admin API secret. The owner
account allowlist is mandatory; a shared Plex viewer is not an administrator.
Use `127.0.0.1:7402` for the default owner healthcheck or update its env port.
Then enable `--profile admin` on the Compose `up` command.

For channels, configure one or more implemented adapters and enable
`--profile channels`. Starting a bot connects to its configured platforms even
in engine shadow mode; read [channel setup](bot.md) first. The minimum web install
does not need a channel or notifier credential. Omit unused optional sections.

## systemd

Build the required binaries from a reviewed checkout. This example installs
control and viewer; the `pitcrew` binary hosts the viewer subcommand:

```sh
cargo build --release --locked -p pitcrew
sudo useradd --system --user-group --home-dir /var/lib/pitcrew \
  --shell /usr/sbin/nologin pitcrew
sudo install -m 0755 target/release/pitcrew /usr/local/bin/pitcrew
sudo /usr/local/bin/pitcrew init --output /etc/pitcrew
# Put mutable state in the systemd StateDirectory, outside /etc.
sudo sed -i 's|/etc/pitcrew/state|/var/lib/pitcrew|g' /etc/pitcrew/pitcrew.toml
sudo install -d -m 0700 -o pitcrew -g pitcrew /var/lib/pitcrew
sudo chown -R pitcrew:pitcrew /etc/pitcrew
sudo -u pitcrew /usr/local/bin/pitcrew doctor --config /etc/pitcrew/pitcrew.toml
sudo install -m 0644 packaging/pitcrew.service packaging/pitcrew-web.service \
  packaging/pitcrew-stack.target /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now pitcrew-stack.target
```

Skip `useradd` if the dedicated account already exists. `init` refuses an existing
output directory, including `/etc/pitcrew`; use the existing config when upgrading.
Units use private temporary directories, restrictive umasks, no new privileges,
and write access only to state. They do not install certifications or live-mode
acknowledgements. Configure a host TLS reverse proxy for remote viewers/webhooks.

Optional `pitcrew-agent.service`, `pitcrew-bot.service` and
`pitcrew-admin.service` require their respective binaries and TOML sections.
Build/install the binary and unit, run its configuration check where supported,
then enable that unit explicitly. Agent supplementary groups and media paths
belong in an installation-specific drop-in. The owner unit waits for control and
viewer; its separate journal must not alias an engine/intake source or sidecar.

## Supervised catalog and diagnosis runtime

With `stack.enabled=true`, Plex and at least one arr configured, the control plane
now schedules read-only catalog sync, access snapshots, diagnosis, independent
bridge authority, admin command consumption and continuous shadow-worker ticks in
one supervised task. A task failure terminates the process for its supervisor to
restart. HTTP handling remains separately bounded. The control plane never starts
viewer, bot or agent processes itself.

The default catalog export is the intake database path with its extension changed
to `catalog.json`; `stack.catalog_snapshot_file` overrides it. The web launcher
reads this same private, atomically replaced file. Configure each arr's `libraries`
as unique positive decimal Plex section keys. Identical Plex/arr/agent paths need
no extra mapping. Different mount layouts use explicit configuration:

```toml
[runtime]
sync_seconds = 300
snapshot_ttl_seconds = 900
batch_size = 10
max_pages = 100
max_series = 1000
search_enabled = false

[[runtime.libraries]]
library_id = "1"
plex_root = "/plex/movies"
arr_root = "/media/movies"
agent_root = "/library/movies"
```

Sync reads metadata only, with capped pages/rows/series and a 120-second sweep
deadline. A failed or capped sweep keeps the prior snapshot's original expiry;
it never makes stale metadata fresh. Evidence collection is limited to reported
items. Each report requires independently refreshed Plex access, an exact catalog
mapping, current arr file metadata and agent byte identity. The installed arr file
is checked before and after collection. Missing or ambiguous facts cannot create
a repair. Multipart, multiversion and shared episode files require review.

`search_enabled=true` explicitly permits interactive arr release searches in
shadow mode. These authenticated GETs can consume indexer quota. The conservative
selector requires a known positive arr quality ID and an exact quality/revision
JSON match, positive approval and
download permission, explicit absence of rejection reasons, sufficient size and
no current exclusion. It creates a budgeted `SelectCandidate` transition; the shadow
worker journals the resulting effects as would-do and calls no write method. With
search disabled, diagnosis and budgeted job creation still work, but no grab is
proposed. Shadow progress never claims a completed repair.

Saved diagnosis comparisons and usage are projected idempotently to the engine
observation journal. Saved health describes the runtime's own bounded operations;
the admin UI performs no probes. Rules remain the runtime decider. Optional cloud
and local model scheduling, optional playback telemetry enrichment, and autonomous
live effect orchestration remain separate integration work. A configured admin
consumer reads the private admin journal that the owner UI creates. Services can
start in any order: until the journal exists, the controller logs
`admin_journal_waiting` each tick and connects once it appears.
Owner commands are consumed after report evidence refresh and before new bridge
jobs are created. Unsupported lower-quality execution is audited as deferred.

## State, backup and shutdown

Keep SQLite databases and their WAL/SHM files on local persistent storage, not an
SMB/NFS share. Preserve configuration and secrets separately from public source.
For a simple consistent backup, stop every stack process, copy the entire private
runtime directory, then restart. A live backup must use SQLite's backup API;
copying just the main `.sqlite` file while writers run can lose committed state.

```sh
# Compose: bind-mounted state survives down.
docker compose --env-file .env -f packaging/compose.yaml down
# systemd: also stops enabled units marked PartOf the target.
sudo systemctl stop pitcrew-stack.target
```

A backup contains viewer identities and credentials; keep it private. Restore
all related journals together using the same reviewed binary/configuration.

## Current runtime boundaries

`stack.enabled=true` opens the intake journal and schedules bounded bridge work.
Missing diagnosis schema is idle; missing independent access/current-byte
evidence puts reports into review. Static `[[stack.catalog]]` entries provide
search metadata only, never authorization. `stack.catalog_snapshot_file` accepts
a private atomic export from catalog sync. An empty catalog invents no matches.

The control plane composes catalog sync, diagnosis, bridge authority and shadow
engine ticks as described in the runtime section above; each stage has fake-based
tests. The executable still refuses live repairs. A UI that starts successfully is
not proof of a completed repair pipeline. [Live certification](live-certification.md)
tracks the operator-run evidence required before live mode.

Default bridge stage timeouts are 180 seconds for download/decode/verify retry,
120 seconds for import and one hour total. Budgets, cooldown and evidence
freshness come from shared configuration. Unknown access, byte identity or
required media constraints never authorize mutation. Multipart/multiversion
media and missing source evidence remain explicit review cases.

## Offline live composition and refusal

`pitcrew::live` now validates local certification inputs and constructs
`Router<Arr, Agent, TargetVerifier<Plex, Agent, Notifications>>` without contacting
an upstream. `[live]` defaults to `enabled=false`; the engine default remains shadow.
The deliberately expired example and complete `[[live.certifications]]` schema are
in [live certification](live-certification.md#version-contracts-and-certification-files).
Validation requires explicit engine/live acknowledgement, a durable store, the stack,
Plex, an agent, configured arr clients, opt-in release searches and one certification
per client. Certificate, acknowledgement and capability records are bounded regular
JSON files. Their exact version and fixture digest must match the reviewed build;
capabilities and evidence must match the existing version-bound ledger, whose
observation time cannot be in the future. Startup never restores an administrator's
reset capability from a file.

The executable still refuses live dispatch after successful composition. It logs
`pitcrew_live_refused` with `effects_enabled=false` and names the missing trusted
ownership receipt producer, current-access refresh on subsequent transitions,
per-intent download/import and Plex target binding, and authoritative interference/
dead-release policy polling. The pinned stock Sonarr/Radarr grab endpoints echo the
submitted release without a download ownership receipt; no configuration flag can
supply that evidence. Existing runtime tests prove shadow effects remain unexecuted.

Delivery composition now supports Discord, Telegram and Seerr viewer transports,
plus exactly one webhook or native ntfy admin notifier. A webhook must acknowledge
`accepted:true` with the exact submitted delivery key; ntfy must return a message
receipt for the configured topic. Email, Matrix and Pushover without a certified
relay receipt protocol fail with specific messages. Outbox payloads stay immutable,
and uncertain sends are not automatically repeated. Loopback HTTP tests cover
request/receipt behavior, not certification of an installed platform. Live notification
preparation and scheduling, held-category/upgrade-guard transports and dynamic-range
collector certification remain deferred as detailed in the certification checklist.
