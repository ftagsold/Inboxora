// Load the Playwright fixture's mail cache (key<TAB>JSON lines from playwright-seed.sql) into
// Redis, where the server reads message display fields and bodies (engine/cache.rs).
//
//   REDIS_URL=redis://127.0.0.1:6379 node backend/tests/e2e/load-mail-cache.mjs mail-cache.tsv
import { readFileSync } from 'node:fs';
import { connect } from 'node:net';

const PREFIX = 'ibx:mc:';
const TTL_SECONDS = String(Number(process.env.MAIL_CACHE_TTL_SECONDS) || 86400);
const file = process.argv[2];
if (!file) throw new Error('usage: load-mail-cache.mjs <cache.tsv>');
const url = new URL(process.env.REDIS_URL || 'redis://127.0.0.1:6379');

const bulk = (s) => `$${Buffer.byteLength(s)}\r\n${s}\r\n`;
const entries = readFileSync(file, 'utf8')
  .split('\n')
  .filter((line) => line.includes('\t'))
  .map((line) => {
    const tab = line.indexOf('\t');
    return [line.slice(0, tab), line.slice(tab + 1)];
  });
const payload = entries
  .map(([key, value]) => `*5\r\n${bulk('SET')}${bulk(PREFIX + key)}${bulk(value)}${bulk('EX')}${bulk(TTL_SECONDS)}`)
  .join('');

const socket = connect(Number(url.port) || 6379, url.hostname);
let replies = '';
socket.on('data', (chunk) => {
  replies += chunk.toString();
  const ok = replies.match(/\+OK\r\n/g)?.length ?? 0;
  if (replies.startsWith('-') || replies.includes('\r\n-')) {
    console.error(`Redis rejected the cache load: ${replies.trim()}`);
    process.exit(1);
  }
  if (ok === entries.length) {
    console.log(`loaded ${ok} mail cache entries`);
    socket.end();
  }
});
socket.on('error', (e) => {
  console.error(e.message);
  process.exit(1);
});
socket.write(payload);
