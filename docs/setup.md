# Setup wizard

`pitcrew init` creates a new installation directory and runs offline doctor
checks. It never enables live mode. The interactive form asks for the minimum:
Plex URL, server identifier, owner token, one Sonarr/Radarr URL and API key,
and the Plex library section key(s) served by that instance. `--discover` fills
in the identifier and lists the library keys for you (see below). Tokens are read
with terminal echo disabled. No cloud account, chat bot or media agent is required
to create the initial configuration. Use the Plex server root URL (no path prefix);
arr URL prefixes such as `/sonarr/` are supported.

```sh
pitcrew init --output runtime
```

The directory must not already exist. The wizard creates `pitcrew.toml`,
`secrets/`, and `state/`. Config and secret files have Unix mode 0600; directories
have mode 0700. Run it as the account that will read those files. Tokens do not
appear in the config, output or command-line arguments. Config refers to files.
`--help` lists all supported options.

## Discovering Plex details

`--discover` saves looking up the Plex server identifier and library keys. After
the Plex URL and owner token, setup sends exactly two authenticated GETs to Plex
(`/identity` and `/library/sections`, 15-second timeout, 1 MiB response cap) and
nothing to Sonarr/Radarr. The identifier is filled in, the library prompt lists
the server's sections, and every chosen library key must exist on that server. A
supplied `--plex-server-id` must match the discovered one. If discovery fails,
setup stops before creating any files. In non-interactive mode `--library` is
still required, so a script never picks a library for you.

## Scripted setup

Provide secret **file paths** or **environment variable names**, never literal
tokens on the command line. Store input files privately before running this
example; the addresses and IDs below are documentation-only placeholders.

```sh
pitcrew init --non-interactive --output runtime \
  --plex-url http://192.0.2.10:32400 --plex-server-id example-server \
  --plex-token-file /run/secrets/plex \
  --arr-kind sonarr --arr-url http://192.0.2.20:8989 \
  --arr-api-key-file /run/secrets/sonarr --library 1
```

Use `--plex-token-env PLEX_TOKEN` and `--arr-api-key-env ARR_API_KEY` instead of
file flags if a secret manager supplies your environment. File/env choices are
mutually exclusive for each credential. Input validation reports fixed diagnostic
messages without repeating supplied values. Missing non-interactive fields fail
instead of prompting or guessing.

`--runtime-root /config` writes deployment paths rooted at `/config`, while the
actual files are created under `--output`. This is useful for the README's
container bind mount. The final doctor still checks the newly created local
files; it does not require that the future mount already exist. Without this
flag, the config uses the output directory's absolute local path.

`--connectivity` explicitly requests the existing bounded, authenticated GET
checks after writing the files. It is never enabled implicitly. A successful
check proves only configuration and reachability. It does not decode media,
write to arr, send notifications or authorize live repairs. If connectivity
fails, inspect the generated files and rerun doctor after correcting the cause.

## After setup

Keep the private output out of version control. The default `runtime/` path is
ignored by Git and Docker builds; protect any custom path yourself. Add optional
sections from [config.example.toml](../config.example.toml), use the
[stack guide](stack.md) for permissions and process supervision, and check edits
with `pitcrew doctor --config FILE`.

The wizard accepts an existing Plex owner token. Browser-assisted token
acquisition is not part of initial setup; viewer sign-in is available in the
web application. Server identifiers and library keys are explicit because a
non-interactive setup must not invent an authorization mapping.
