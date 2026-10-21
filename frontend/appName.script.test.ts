import assert from 'node:assert/strict';
import test from 'node:test';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const script = new URL('./app-name.sh', import.meta.url).pathname;
const index = readFileSync(new URL('./index.html', import.meta.url), 'utf8');
const manifest = readFileSync(new URL('./public/manifest.json', import.meta.url), 'utf8');

function render(name: string | undefined) {
  const root = mkdtempSync(join(tmpdir(), 'app-name-'));
  writeFileSync(join(root, 'index.html'), index);
  writeFileSync(join(root, 'manifest.json'), manifest);
  const env: NodeJS.ProcessEnv = { PATH: process.env.PATH, APP_NAME_HTML_ROOT: root };
  if (name !== undefined) env.VITE_APP_NAME = name;
  execFileSync('sh', [script], { env, stdio: 'pipe' });
  return { root, html: readFileSync(join(root, 'index.html'), 'utf8'), json: JSON.parse(readFileSync(join(root, 'manifest.json'), 'utf8')) };
}

test('fills the runtime app name into index.html and manifest.json', () => {
  const { html, json } = render('KnödelMail');
  assert.match(html, /<title>KnödelMail<\/title>/);
  assert.match(html, /<meta name="application-name" content="KnödelMail"\/>/);
  assert.doesNotMatch(html, /__INBOXORA_APP_NAME__/);
  assert.equal(json.name, 'KnödelMail');
  assert.equal(json.short_name, 'KnödelMail');
});

test('escapes HTML, JSON and sed metacharacters in the name', () => {
  const name = `A & "B" <i>'C'</i> | \\ $HOME /x/`;
  const { html, json } = render(name);
  assert.match(html, /<title>A &amp; &quot;B&quot; &lt;i&gt;&#39;C&#39;&lt;\/i&gt; \| \\ \$HOME \/x\/<\/title>/);
  assert.equal(json.name, name);
});

test('defaults to Inboxora and re-renders from the template on every start', () => {
  const first = render(undefined);
  assert.match(first.html, /<title>Inboxora<\/title>/);
  writeFileSync(join(first.root, 'index.html'), 'stale output');
  execFileSync('sh', [script], { env: { PATH: process.env.PATH, APP_NAME_HTML_ROOT: first.root, VITE_APP_NAME: 'Renamed' }, stdio: 'pipe' });
  assert.match(readFileSync(join(first.root, 'index.html'), 'utf8'), /<title>Renamed<\/title>/);
});
