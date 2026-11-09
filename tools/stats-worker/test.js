// `node test.js`: the receiver keeps valid lines and turns everything else away.
import assert from 'node:assert/strict';
import worker from './src/index.js';

const rows = [];
const env = {
	DB: { prepare: () => ({ bind: (...values) => ({ run: async () => rows.push(values) }) }) },
};
const post = (body, path = '/v1/performance') =>
	worker.fetch(
		new Request(`https://stats.test${path}`, {
			method: 'POST',
			body: typeof body === 'string' ? body : JSON.stringify(body),
		}),
		env,
	);
const install = '0123456789abcdef';

assert.equal(
	(
		await post({
			install,
			lines: [
				{ session: 1, version: '0.1.11' },
				{ at: 2, memory_mb: 70 },
				{ at: 3, stream: { kind: 'share' } },
				{ at: 4, version: '0.1.17', log: [[1, 'gateway', 'READY, session started']] },
				{ name: 'no' },
				[1],
				3,
			],
		})
	).status,
	204,
);
assert.equal(rows.length, 1);
assert.equal(rows[0][1], install);
assert.deepEqual(JSON.parse(rows[0][2]), [
	{ session: 1, version: '0.1.11' },
	{ at: 2, memory_mb: 70 },
	{ at: 3, stream: { kind: 'share' } },
	{ at: 4, version: '0.1.17', log: [[1, 'gateway', 'READY, session started']] },
]);
// Nothing worth keeping writes nothing.
assert.equal((await post({ install, lines: [{ name: 'no' }] })).status, 204);
assert.equal(rows.length, 1);

assert.equal((await post({ install: 'someone', lines: [] })).status, 400);
assert.equal((await post({ install, lines: {} })).status, 400);
assert.equal((await post('not json')).status, 400);
assert.equal((await post('x'.repeat(300 * 1024))).status, 413);
assert.equal((await post({ install, lines: [] }, '/')).status, 404);
assert.equal((await worker.fetch(new Request('https://stats.test/v1/performance'), env)).status, 405);

// Relay logins: none without the TURN key, Cloudflare's TURN login with it.
assert.equal((await post('', '/v1/relay')).status, 503);
assert.equal((await worker.fetch(new Request('https://stats.test/v1/relay'), env)).status, 405);
const real = globalThis.fetch;
let asked;
globalThis.fetch = async (url, init) => {
	asked = { url, init };
	return Response.json(
		{
			iceServers: [
				{ urls: ['stun:stun.cloudflare.com:3478'] },
				{ urls: ['turn:turn.cloudflare.com:3478?transport=udp'], username: 'u1', credential: 'c1' },
			],
		},
		{ status: 201 },
	);
};
env.TURN_KEY_ID = 'key';
env.TURN_KEY_TOKEN = 'token';
const relay = await post('', '/v1/relay');
assert.equal(relay.status, 200);
assert.deepEqual(await relay.json(), { username: 'u1', credential: 'c1' });
assert.match(asked.url, /\/turn\/keys\/key\/credentials\/generate-ice-servers$/);
assert.equal(asked.init.headers.Authorization, 'Bearer token');
globalThis.fetch = async () => new Response(null, { status: 401 });
assert.equal((await post('', '/v1/relay')).status, 502);
globalThis.fetch = real;
console.log('ok');
