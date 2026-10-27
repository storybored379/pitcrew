# Intake and offline conversation drills

P5 adds channel-independent intake, a Plex-authenticated web report flow, channel
adapters, and a synthetic conversation drill. These are pre-alpha components;
passing an intake drill does not certify repair effects or a live cutover.

## Boundaries

Transport authentication supplies the sender. Plex access supplies the authorized
catalog. Viewer text can describe a problem or select an accessible item; it cannot
change identity, grant library access, select a tool, or authorize a repair. An
ambiguous title, episode or library produces a clarification. An episode-number
mismatch never silently selects an adjacent file.

The test channel is authenticated and loopback-only. It uses the same dispatcher
as real channels. Its trusted fixture configuration supplies synthetic identities
and catalog access; those values are never accepted from a message body. No
self-bot accounts are supported. Telegram proactive delivery requires an inbound
message from that user.

## Corpus and score contract

`drills/` contains invented catalog entries and independently labeled conversations.
Labels cover exact item selection, typo/slang variants, ambiguous libraries,
multiple complaints, non-playback symptoms, follow-ups, unknown access and hostile
instructions. No private baseline reports or deployment values are included.

The harness reports separate intake, title, episode, library and symptom matches.
Reply constraints, diagnosis labels, ready latency and unsafe actions are independent
observations. Missing diagnosis/ready/effect evidence stays unobserved; the parser
cannot earn a diagnosis or ready pass by repeating the expected label. Cross-system
observations retain the same case and turn identifiers for comparison.

See [drill commands and schema](../drills/README.md), [web setup](web.md), and
[channel setup](bot.md). Accepted intake remains an observation awaiting review;
P3/P4 diagnosis and repair execution are separate integration work. The current
catalog is an explicitly configured snapshot, not a synchronized live index.

No drill contacts an existing server. Optional external adapters run one fresh,
isolated process per conversation with outbound operations disabled, as described
in [the drill guide](../drills/README.md). Request exports contain synthetic inputs,
not permission to send requests to a live help desk.
