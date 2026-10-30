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

`node test.js` checks what the receiver keeps and turns away.

## Read

In the Cloudflare dashboard, open Storage & databases → D1 → leech-stats → Console, or:

```sh
npx wrangler d1 execute leech-stats --remote --command "SELECT datetime(received, 'unixepoch') AS received, install, value AS line FROM uploads, json_each(uploads.lines) ORDER BY received DESC LIMIT 100"
```
