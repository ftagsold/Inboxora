import assert from 'node:assert/strict';
import test from 'node:test';
import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const srcDir = fileURLToPath(new URL('../', import.meta.url));

async function sourceFiles(dir: string): Promise<string[]> {
  const entries = await readdir(dir, { withFileTypes: true });
  const nested = await Promise.all(entries.map(entry => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return entry.name === 'locales' ? [] : sourceFiles(path);
    return /\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name) ? [path] : [];
  }));
  return nested.flat();
}

test('no part of the web UI checks for or announces app updates', async () => {
  const offenders: string[] = [];
  for (const file of await sourceFiles(srcDir)) {
    const content = await readFile(file, 'utf8');
    if (/\/api\/update\b|updateAvailable|inboxoraNative\?\.updates\?\./.test(content)) offenders.push(file.slice(srcDir.length));
  }
  assert.deepEqual(offenders, []);
  for (const locale of await readdir(join(srcDir, 'locales'))) {
    if (!locale.endsWith('.json')) continue;
    assert.doesNotMatch(await readFile(join(srcDir, 'locales', locale), 'utf8'), /"updateAvailable"/, locale);
  }
});
