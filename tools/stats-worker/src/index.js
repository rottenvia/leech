// Receives the performance statistics Leech sends (apps/desktop/src/perf_stats.rs) and keeps
// them in D1, where only the owner of the Cloudflare account can read them. Nothing else is
// stored: no address and no headers, only the lines, the installation's random name and when
// they came.
//
// Also hands out relay logins for remote control (apps/desktop/src/remote.rs): short-lived
// Cloudflare Realtime TURN credentials, minted with the TURN key kept in this Worker's secrets
// TURN_KEY_ID and TURN_KEY_TOKEN. Nothing about the request is stored.

/// The only keys a line may have: a session's first line, then its summaries and streams.
const KEYS = new Set([
	'session',
	'version',
	'system',
	'arch',
	'gpu',
	'refresh_hz',
	'at',
	'startup_ms',
	'frame_us_limits',
	'frame_us_buckets',
	'frame_us_max',
	'memory_mb',
	'memory_peak_mb',
	// A finished share or watched stream, summed up.
	'stream',
	// Connection journal entries: [unix ms, kind, detail], at most 200 per line.
	'log',
]);
const MAX_BYTES = 256 * 1024;
const MAX_LINES = 2000;

const status = (code) => new Response(null, { status: code });
// A login outlives the longest remote control session; the relay refuses it afterwards.
const RELAY_TTL = 86400;

// ponytail: anyone can ask for a login, so the free 1,000 GB a month is shared with whoever
// finds this address; add a per-address rate limit if the usage page shows strangers.
async function relayLogin(env) {
	if (!env.TURN_KEY_ID || !env.TURN_KEY_TOKEN) {
		return status(503);
	}
	const response = await fetch(
		`https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_KEY_ID}/credentials/generate-ice-servers`,
		{
			method: 'POST',
			headers: {
				Authorization: `Bearer ${env.TURN_KEY_TOKEN}`,
				'Content-Type': 'application/json',
			},
			body: JSON.stringify({ ttl: RELAY_TTL }),
		},
	);
	if (!response.ok) {
		return status(502);
	}
	const { iceServers } = await response.json();
	const turn = [iceServers].flat().find((server) => server?.username && server?.credential);
	if (!turn) {
		return status(502);
	}
	return Response.json(
		{ username: turn.username, credential: turn.credential },
		{ headers: { 'Cache-Control': 'no-store' } },
	);
}

export default {
	async fetch(request, env) {
		const path = new URL(request.url).pathname;
		if (path === '/v1/relay') {
			return request.method === 'POST' ? relayLogin(env) : status(405);
		}
		if (path !== '/v1/performance') {
			return status(404);
		}
		if (request.method !== 'POST') {
			return status(405);
		}
		if (Number(request.headers.get('content-length')) > MAX_BYTES) {
			return status(413);
		}
		const text = await request.text();
		if (text.length > MAX_BYTES) {
			return status(413);
		}
		let body;
		try {
			body = JSON.parse(text);
		} catch {
			return status(400);
		}
		if (
			typeof body?.install !== 'string' ||
			!/^[0-9a-f]{16}$/.test(body.install) ||
			!Array.isArray(body.lines) ||
			body.lines.length > MAX_LINES
		) {
			return status(400);
		}
		const lines = body.lines.filter(
			(line) =>
				line &&
				typeof line === 'object' &&
				!Array.isArray(line) &&
				Object.keys(line).every((key) => KEYS.has(key)),
		);
		if (lines.length) {
			await env.DB.prepare('INSERT INTO uploads (received, install, lines) VALUES (?1, ?2, ?3)')
				.bind(Math.floor(Date.now() / 1000), body.install, JSON.stringify(lines))
				.run();
		}
		return status(204);
	},
};
