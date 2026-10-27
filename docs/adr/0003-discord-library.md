# ADR 0003: Twilight for Discord transport

Status: accepted for the Discord adapter; overall intake extended by [ADR 0007](0007-channels-and-notifiers.md). Implemented in the optional bot adapter. Date checked: 2026-10-23.

Use `twilight-gateway`, `twilight-http`, and `twilight-model` when the bot is
implemented. The bot consumes DMs and sends replies; music/voice and a general
command framework are outside this product.

The current official releases checked for this decision are Serenity 0.12.5 and
Twilight 0.17.1. Both provide gateway and HTTP support. Serenity offers a convenient
high-level client and optional cache/framework. Twilight exposes the gateway and
HTTP pieces separately, matching a thin transport process whose conversation state
and outbox live in the control plane. Twilight documents Rust 1.89 as its MSRV;
the workspace's Rust 1.99.1 meets that requirement.

The choice is architectural, not a claim that either library is more reliable or
faster. Serenity would also work; its default batteries add little for this DM-only
boundary. Twilight's documented willingness to make breaking changes is an accepted
maintenance cost. Pin releases through Cargo.lock and test upgrades against contract fixtures.
No Discord dependency is built into the stub or any live Discord session opened
in P0.

Future implementation must explicitly select the minimum gateway intents, disable
mention expansion, limit message sizes, redact logs, handle reconnect/resume and
rate limits, and preserve outbox IDs across process restarts. HTTP success must be
recorded separately from an enqueued notification. A disconnect after sending can
be ambiguous; see ADR 0004 rather than claiming exactly-once transport delivery.

Sources:

- [Serenity releases](https://github.com/serenity-rs/serenity/releases)
- [Serenity features and MSRV](https://github.com/serenity-rs/serenity)
- [Twilight releases](https://github.com/twilight-rs/twilight/releases)
- [Twilight gateway](https://api.twilight.rs/twilight_gateway/index.html)
- [Twilight HTTP](https://api.twilight.rs/twilight_http/index.html)
- [Twilight support policy](https://twilight.rs/support)
