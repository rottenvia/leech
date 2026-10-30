// Receives the performance statistics Leech sends (apps/desktop/src/perf_stats.rs) and keeps
// them in D1, where only the owner of the Cloudflare account can read them. Nothing else is
// stored: no address and no headers, only the lines, the installation's random name and when
// they came.

/// The only keys a line may have: a session's first line, then its summaries.
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
]);
const MAX_BYTES = 256 * 1024;
const MAX_LINES = 2000;

const status = (code) => new Response(null, { status: code });

export default {
	async fetch(request, env) {
		if (new URL(request.url).pathname !== '/v1/performance') {
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
