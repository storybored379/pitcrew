# Channel intake

`pitcrew-bot` hosts Discord direct messages through Twilight, Telegram private
messages through Bot API long polling, Seerr issue comments through its REST API,
and a separate authenticated loopback test channel. All use the same bounded
`pitcrew_bot::dispatch` function and conservative intake parser. Platform and test
messages also use the same durable journal and conversation context.

This is implemented and tested against synthetic events and local fake HTTP
servers. No live platform or media installation has been certified or deployed.
Intake records observations; this binary does not start repair jobs, reserve
re-downloads, or announce that a title is ready.

## Local synthetic test channel

Copy [the synthetic configuration](../bins/pitcrew-bot/examples/test-channel.json),
replace its example bearer secret, choose a short expiration, and run:

```sh
cargo run --locked -p pitcrew-bot -- test-channel ./test-channel.json
```

The bind address must be loopback. Port zero selects an available ephemeral port;
the process prints the selected address. Send `POST /test/messages` with
`Authorization: Bearer <configured-secret>` and this JSON:

```json
{"message_id":"drill-1","conversation_id":"case-1","text":"The Glass Harbor won't play"}
```

Only these three request fields are accepted. The server selects the account and
allowed libraries from its trusted configuration and checks the principal expiry
before returning a replay. Message identifiers are bounded to 128 bytes, input
text to 8192 bytes, and the entire JSON body to 12 KiB. Bearer secrets are compared
using constant-time SHA-256 digest comparison. The configuration's static grants
are synthetic fixtures, unsuitable for live viewer authorization.

Repeated identifiers within the same authenticated sender and conversation return
the first persisted result, including after restart. Changing the message text
does not create a second report. Identifiers in another conversation are distinct.
Follow-up messages retain server-owned report context. A fresh permission check
also gates cached selections, so withdrawing access does not expose the prior
selection on replay.

## Configured platforms

The explicit `run` mode enables the configured integrations:

```sh
cargo run --locked -p pitcrew-bot -- run ./channels.json
```

[The configuration example](../bins/pitcrew-bot/examples/channels.json) shows the
shape with Discord enabled. Set unused integrations to `null`. At least one is
required and all `channel_id` values must be distinct. Keep credentials in a
restricted configuration file. The bot shares the web app's intake database and
its private link-code key companion file; do not copy only one of the pair.

Optional Telegram configuration:

```json
{"channel_id":"telegram-primary","bot_token":"replace-with-bot-token"}
```

Optional Seerr configuration:

```json
{"channel_id":"seerr-primary","api_url":"https://seerr.example.invalid","api_key":"replace-with-api-key","bot_user_id":42}
```

A viewer signs in through the web app, creates a link code, then sends
`link CODE` through the platform. A fresh membership check precedes code
consumption; an outage or revoked access leaves the code unconsumed. Linking
proves identity only. Each subsequent message checks current Plex server
membership and library sections with that viewer's stored token. Unavailable or denied authorization never permits a report
selection. None of the sender's message fields can grant access.

The configured catalog currently remains an explicit snapshot. Refresh it from a
trusted catalog source when titles, episode numbering, library IDs, or copies
change. Automatic catalog synchronization and live certification are deferred.

## Platform contracts and delivery

- Discord uses a Twilight gateway shard with the direct-message intent and a
  `Bot` authorization header for REST sends. Before connecting, `/users/@me`
  must identify a bot account. Guild messages, bot authors, and webhook authors
  are ignored. Outbound `allowed_mentions` disables all parsing and reply pings;
  the conservative message limit is 2000 characters. No self-bot mode exists.
- Telegram polls `getUpdates` with only `message` updates enabled. Bot authors,
  groups, channels, and non-text updates are ignored. `sendMessage` uses literal
  text without a parse mode and disables link previews. Replies are sent only
  after an inbound private message; proactive Channel calls require a prior
  inbound timestamp. The limit is 4096 characters. An offset advances only after
  intake and the reply claim complete; replays are deduplicated durably.
- Seerr polls open issues, fetches issue details for comment authors, ignores its
  own configured bot user, and posts comments to `/api/v1/issue/{id}/comment`.
  It fetches movie/TV metadata and uses issue season/episode fields to provide
  title context. Library and 4K ambiguity still goes through the shared parser.
  Each cycle is bounded to ten pages of 100 issues and rejects repeated IDs or
  oversized pages. Seerr issue status is never changed by intake.

A durable delivery claim is written as `uncertain` before an outbound request.
Only a parsed platform receipt records confirmation. Timeouts, malformed success
responses, redirects, and server errors remain uncertain. A duplicate inbound
message cannot send a second reply, including after restart or a lost response.
Already claimed events also skip intake, so repolling a consumed link code does
not spend the lookup quota again. New events still require fresh authorization.
Automatic resends are intentionally absent; admin review/resend tooling is a
future integration. Definitive rate-limit responses and other rejections are
recorded separately. HTTP redirects are disabled, timeouts are finite, and JSON
responses are bounded to one MiB.

Polling errors currently stop the runner with a generic error. Run it under an
operator-managed restart policy after certification. Discord gateway session
reconnection is handled by Twilight. The gateway itself, real platform rate
limits, production account permissions, and eventual engine delivery are not
certified by the fake-only test suite.

The wire contracts follow the [Discord gateway documentation](https://docs.discord.com/developers/events/gateway),
[Twilight gateway API](https://docs.rs/twilight-gateway/latest/twilight_gateway/struct.Shard.html),
[Telegram Bot API](https://core.telegram.org/bots/api), and
[Seerr's issue routes](https://github.com/seerr-team/seerr/blob/develop/server/routes/issue.ts).
