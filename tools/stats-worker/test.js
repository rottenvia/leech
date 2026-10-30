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
			lines: [{ session: 1, version: '0.1.11' }, { at: 2, memory_mb: 70 }, { name: 'no' }, [1], 3],
		})
	).status,
	204,
);
assert.equal(rows.length, 1);
assert.equal(rows[0][1], install);
assert.deepEqual(JSON.parse(rows[0][2]), [{ session: 1, version: '0.1.11' }, { at: 2, memory_mb: 70 }]);
// Nothing worth keeping writes nothing.
assert.equal((await post({ install, lines: [{ name: 'no' }] })).status, 204);
assert.equal(rows.length, 1);

assert.equal((await post({ install: 'someone', lines: [] })).status, 400);
assert.equal((await post({ install, lines: {} })).status, 400);
assert.equal((await post('not json')).status, 400);
assert.equal((await post('x'.repeat(300 * 1024))).status, 413);
assert.equal((await post({ install, lines: [] }, '/')).status, 404);
assert.equal((await worker.fetch(new Request('https://stats.test/v1/performance'), env)).status, 405);
console.log('ok');
