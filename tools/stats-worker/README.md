# Leech performance statistics receiver

A Cloudflare Worker that stores the performance statistics Leech sends (see
`apps/desktop/src/perf_stats.rs` and `docs/storage-policy.md`) in a D1 database. Only the owner
of the Cloudflare account can read them. It runs on the free Workers plan.

It is deployed at `https://leech-stats.leech-stats.workers.dev`; Leech sends to
`/v1/performance`.

## Deploy

1. Create a free Cloudflare account and, in this folder, run `npx wrangler login`.
2. `npx wrangler d1 create leech-stats`, and put the printed `database_id` in `wrangler.toml`.
3. `npx wrangler d1 execute leech-stats --remote --file schema.sql`
4. `npx wrangler deploy`, then set `RECEIVER` in `perf_stats.rs` to the printed address
   followed by `/v1/performance`.

`node test.js` checks what the receiver keeps and turns away, and the relay logins.

## Relay logins for remote control

`POST /v1/relay` returns `{"username","credential"}`: a Cloudflare Realtime TURN login valid
for a day, which Leech uses when two PCs cannot reach each other directly
(`apps/desktop/src/remote.rs`, `crates/discord-voice/src/remote/turn.rs`). Without the TURN
key it answers 503 and remote control stays direct-only. To turn it on once:

1. In the Cloudflare dashboard open Realtime → TURN Server → Create, and keep the key ID and
   the API token it shows.
2. In this folder run `npx wrangler secret put TURN_KEY_ID` and
   `npx wrangler secret put TURN_KEY_TOKEN`, pasting each value when asked.

Cloudflare bills TURN traffic after the first 1,000 GB a month (shared with its SFU), at
$0.05 per GB; a relayed 1080p session uses about 2-4 GB an hour.

## Read

In the Cloudflare dashboard, open Storage & databases → D1 → leech-stats → Console, or:

```sh
npx wrangler d1 execute leech-stats --remote --command "SELECT datetime(received, 'unixepoch') AS received, install, value AS line FROM uploads, json_each(uploads.lines) ORDER BY received DESC LIMIT 100"
```

### Connection journal

Since 0.1.17 the lines also carry the connection journal (`client_core::journal`): every REST
request with its route (IDs removed), status and Discord error code, gateway connects, closes
and invalidations, voice socket losses, and why a session ended, with the `connection.rs` line
that ended it. A sign-out is sent within seconds. Read one install's timeline, oldest first:

```sh
node logs.mjs 390a292883a6b54e 48   # install, hours back (default: all installs, 24)
```
