// Prints the connection journal Leech sends (client_core::journal), oldest first, one entry per
// line: `node logs.mjs [install] [hours]`. Without an install it shows every installation.
import { execFileSync, execSync } from 'node:child_process';

const [install, hours = '24'] = process.argv.slice(2);
if ((install && !/^[0-9a-f]{16}$/.test(install)) || !/^\d{1,5}$/.test(hours)) {
	console.error('usage: node logs.mjs [16-hex install] [hours back]');
	process.exit(1);
}
const sql =
	"SELECT install, json_extract(value, '$.version') AS version, json_extract(value, '$.log') AS log " +
	'FROM uploads, json_each(uploads.lines) ' +
	`WHERE json_extract(value, '$.log') IS NOT NULL AND received > unixepoch() - ${hours} * 3600` +
	(install ? ` AND install = '${install}'` : '');
const command = ['wrangler', 'd1', 'execute', 'leech-stats', '--remote', '--json', '--command'];
const options = { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 };
// Windows runs npx through cmd, which needs the query quoted; it holds no double quote.
const output =
	process.platform === 'win32'
		? execSync(`npx ${command.join(' ')} "${sql}"`, options)
		: execFileSync('npx', [...command, sql], options);
const entries = JSON.parse(output)[0].results.flatMap((row) =>
	JSON.parse(row.log).map(([at, kind, detail]) => ({ at, kind, detail, row })),
);
entries.sort((a, b) => a.at - b.at);
for (const { at, kind, detail, row } of entries) {
	const time = new Date(at).toISOString().replace('T', ' ').replace('Z', '');
	console.log(`${time} ${row.install} ${row.version} ${kind}: ${detail}`);
}
console.error(`${entries.length} entries`);
