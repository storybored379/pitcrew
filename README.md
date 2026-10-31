# Pit Crew

A self-hosted help desk for your Plex server. Viewers report a broken movie or
episode; Pit Crew gathers evidence, recommends a response, and tracks the fix.
The repair engine can select a replacement through Sonarr or Radarr, verify the
exact imported file, and send a reply. Rules work without an AI account.

```text
Viewer report → access check → diagnosis → fix job → verified replacement → reply
                                   ↓
                         advice or owner review
```

**0.1.0 pre-alpha. Shadow mode is the default.** Shadow mode records proposed
repair effects without downloading, importing, deleting, or sending engine repair
notifications. Viewer sign-in, report intake and explicitly enabled chat channels
are real interactions. Live repairs require separate, version-specific
[certification](docs/live-certification.md); an image build or a green test suite
is not certification. Runtime integration status is recorded in
[the stack guide](docs/stack.md).

Every repair reserves a budget before a grab, preserves the library copy until a
verified replacement exists, and requires fresh access and exact file identity.
Unknown evidence goes to review. A completed download is never enough to say
“fixed.” Optional cloud models receive redacted facts only, with explicit consent.

## Quickstart: Linux + Docker Compose

You need Docker Engine with Compose, Plex, and **one** Sonarr or Radarr. Have the
Plex server URL and owner token ready, plus the arr URL and API key. Use addresses
reachable **from the Linux host**. Sonarr/Radarr show the API key in Settings →
General. With `--discover`, setup reads the Plex server identifier and library
list for you (two read-only requests to Plex); without it, it asks for them. See
[setup details](docs/setup.md) for scripts and extra options.

From the root of your checkout:

```sh
# First build downloads public dependencies; this can take several minutes.
docker build -f Dockerfile.stack -t pitcrew-stack:local .

# A new private directory; tokens are entered with terminal echo disabled.
docker run --rm -it --network host --user "$(id -u):$(id -g)" \
  -v "$PWD:/work" -w /work pitcrew-stack:local \
  pitcrew init --discover --output runtime --runtime-root /config

# Only host paths and numeric ownership go in .env; credentials stay in files.
(umask 077; printf 'PITCREW_HOME=%s/runtime\nPITCREW_UID=%s\nPITCREW_GID=%s\n' \
  "$PWD" "$(id -u)" "$(id -g)" > .env)

docker compose --env-file .env -f packaging/compose.yaml up -d --no-build
docker compose --env-file .env -f packaging/compose.yaml ps
```

Open **http://127.0.0.1:7401** on that host and sign in with Plex. The control
health endpoint is **http://127.0.0.1:7400/healthz**. Config and secrets are mode
0600, directories mode 0700. `init` refuses to overwrite an existing directory
and runs offline doctor checks before finishing. Apart from `--discover`'s two
Plex reads, it does not contact Plex or arr; drop `--discover` and use
`--network none` for a fully offline setup.
The examples assume a non-root host account; do not run the setup container as root.

For a remote browser, put a TLS reverse proxy on the same host and set
`stack.web_public_origin` to its HTTPS origin. Listeners remain on loopback;
there is no public port binding to remove. Host networking makes this example
Linux-specific. Other platforms can use the bare-metal route or adapt networking
with their own ingress. The supplied examples do not deploy a proxy.

The slim stack image serves control, viewer, admin and bot without FFmpeg. The
optional agent uses a separate `Dockerfile.agent` image with ffmpeg/ffprobe; build
it before enabling the agent profile (see the stack guide). Both images run as
non-root and pin their base images and Debian package snapshot.

The minimum starts control and viewer. The optional media agent provides exact
probe/decode evidence; without it, replacement requests can remain in review.
Configure it before expecting useful broken-file diagnoses. The
[stack guide](docs/stack.md) explains media mounts, optional owner desk/channels,
current runtime limits, backup and stop commands. Initial compilation is outside
the five-minute configuration target. No prebuilt image is published yet; when a
reviewed image is available, set `PITCREW_IMAGE` in `.env` to its digest and use
`docker compose ... pull` followed by `up -d --no-build`.

## Bare metal

Install the Rust toolchain pinned in `rust-toolchain.toml`, then:

```sh
cargo build --locked --release -p pitcrew
./target/release/pitcrew init --discover --output runtime
./target/release/pitcrew --config runtime/pitcrew.toml
# In a second terminal:
./target/release/pitcrew web --config runtime/pitcrew.toml
```

This uses the same viewer URL and configuration as Compose. The setup wizard
writes absolute local paths when `--runtime-root` is omitted. See
[systemd installation](docs/stack.md#systemd) for running the processes at boot.
The optional agent needs ffmpeg/ffprobe and read access to the exact media paths.

## Configuration and integrations

Edit the generated `runtime/pitcrew.toml`; keep its secrets directory private.
The [annotated configuration](config.example.toml) documents optional sections.
[Setup](docs/setup.md) describes flags and non-interactive use. To check edits:

```sh
# Compose: check paths as they are mounted inside the container.
docker compose --env-file .env -f packaging/compose.yaml run --rm --no-deps \
  control pitcrew doctor --config /config/pitcrew.toml
# Add --connectivity only when you want authenticated, bounded GET checks.
```

For bare metal, use `pitcrew doctor --config runtime/pitcrew.toml` instead.
Connectivity checks do not decode files or certify safe replacements.

| Integration | Needed? | Purpose |
| --- | --- | --- |
| Plex | Yes | Viewer sign-in, library access and media identity |
| Sonarr **or** Radarr | One | Library mapping and replacement candidates; multiple instances supported |
| Viewer web | Included | Report wizard and account-scoped report history |
| Media agent | For file evidence | Read-only probes and bounded decode tests beside your media |
| Owner desk | Optional | Saved evidence, review requests and audited policy actions |
| Discord / Telegram / Seerr | Optional | Additional authenticated intake channels |
| Tautulli / Tracearr bridge | Optional | Playback context; missing telemetry stays unknown |
| Local or cloud model | Optional | Off by default; per-kind choices stay within rules-approved options and fall back to rules |
| Notifier / download-source adapters | Optional | Explicitly configured delivery and source evidence |

Start with rules and web intake. Add integrations only as needed using the
[documentation index](docs/README.md). No Discord bot, cloud key, special storage
backend, or existing help-desk application is required.

## Questions and troubleshooting

**Why does a report need review?** Access, exact file mapping or decode evidence
may be missing or stale. Multiple versions, multipart files and ambiguous matches
fail closed. Review the stored diagnosis; a title match does not prove identity.

**Why are no repairs happening?** Shadow mode never executes repair effects.
Use [live certification](docs/live-certification.md) to understand the prerequisites
and unsupported paths before considering live mode.

**Why is the catalog empty?** The control log's `catalog_sync_incomplete` event
names the reason (for example a Plex library whose type doesn't match its arr).
Check the Plex server identifier, library keys and arr mapping, then doctor with
`--connectivity`. Catalog refresh and diagnosis
scheduling status is documented in the [stack guide](docs/stack.md). Empty or
incomplete catalog evidence never creates guessed matches.

**Permission denied or database unavailable?** The configured account must read
the 0600 secrets and write `runtime/state`. In Compose, `.env` UID/GID must match
the account that created `runtime`; changing only the container user cannot fix
host ownership. Keep SQLite on local disk, including its WAL/SHM sidecars.

**Unhealthy container?** Run `docker compose --env-file .env -f
packaging/compose.yaml logs --tail=50 control web` and offline doctor. Healthcheck
ports in `.env` must match TOML listeners. The agent check tests liveness only;
use authenticated doctor connectivity to check its API. Scrub logs before sharing.

**How do I stop it?** Run `docker compose --env-file .env -f
packaging/compose.yaml down`. The private bind-mounted configuration and state
remain on disk. Back up with the processes stopped or with a SQLite-aware backup;
do not copy an active database while omitting its WAL.

## Development

```sh
cargo test --workspace --locked
cargo xtask check
```

Tests use synthetic fixtures and owned fake processes, without a real media stack
or credentials. See [CONTRIBUTING.md](CONTRIBUTING.md) for prerequisites, privacy
checks, fault tests and mutation gates. [ARCHITECTURE.md](ARCHITECTURE.md) and the
[ADRs](docs/adr/0001-architecture.md) explain the safety boundaries.
Report vulnerabilities through [SECURITY.md](SECURITY.md).

Dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
