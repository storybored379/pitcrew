# Security policy

Pit Crew 0.1.0 is pre-alpha. The latest development branch receives fixes, but
there is no supported production release or completed live security certification.
Shadow mode is the default. Enabling live repairs requires the installation's
explicit gates and [live certification](docs/live-certification.md).

## Report a vulnerability privately

Use the repository's **Security → Advisories → Report a vulnerability** form on
GitHub. Include the affected commit, the trust boundary involved, expected and
actual behavior, and a minimal synthetic reproduction. Remove tokens, account
IDs, server addresses, viewing history, and production logs before submitting.
Do not open a public issue with exploit details or private data.

The maintainer must enable GitHub private vulnerability reporting before public
release. If the private reporting button is unavailable, open an issue asking
for private reporting to be enabled without describing the vulnerability. No
personal email address is required. Response times are best effort during
pre-alpha; a fix is not a certification of an installation.

## Trust boundaries

Viewer text and client-supplied identifiers are untrusted. Authentication alone
does not grant library access. Mutations require current, positive access for the
exact library, trusted media identity, a reserved budget, and the engine's safety
gates. Unknown access fails closed. Web and bot adapters cannot turn viewer text
into arbitrary upstream commands.

The engine commits intents before effects and reconciles ambiguous upstream
results. A library file must remain until a verified replacement exists. Ready
requires verification of the exact imported file. Terminal failures create one
admin notification intent; uncertain remote delivery is recorded rather than
claimed to be exactly once.

## Credentials and data

Configuration uses environment or file references for secrets. Keep secret files
private to the service account and give each process only its required access.
Never publish an installation's config, databases, raw reports, or log captures.
The local agent authenticates requests, limits its accessible paths, and bounds
media subprocesses. Use narrow media mounts and trusted TLS ingress for remote
access; see [agent limits](docs/agent.md).

Cloud processing is optional and must be explicitly configured. Provider requests
pass through the redaction boundary; suggestions remain advisory and cannot
bypass safety checks. Prefer the rules-only minimum install when cloud processing
is unnecessary. Use authenticated ingress and browser session protections as
documented for each surface. Deployment TLS, filesystem permissions, network
isolation, and credential rotation remain operator responsibilities.

Tests use synthetic fixtures and owned fakes. Automated checks and shadow logs do
not certify real upstream behavior or authorize live repairs.
