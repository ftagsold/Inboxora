// AI provider configuration, connection test, status and the streaming chat proxy against a
// mock OpenAI-compatible endpoint, plus the ChatGPT device-authorization error paths (which
// never reach OpenAI).
import http from 'node:http';
import { U } from '../seed-mail.mjs';

// The model name chooses the mock's behaviour.
function mockProvider(port) {
  const state = { port, requests: [] };
  const server = http.createServer(async (req, res) => {
    let raw = '';
    for await (const chunk of req) raw += chunk;
    const body = JSON.parse(raw || '{}');
    state.requests.push({ path: req.url, auth: req.headers.authorization || null, body });
    if (body.model === 'bad') {
      res.writeHead(401, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ error: { message: 'invalid key sk-secret-1' } }));
    }
    if (!body.stream) {
      res.writeHead(200, { 'content-type': 'application/json' });
      const message = body.model === 'empty' ? { content: null } : { content: 'ok' };
      return res.end(JSON.stringify({ choices: [{ message, finish_reason: body.model === 'empty' ? 'length' : 'stop' }] }));
    }
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const part of ['Hel', 'lo', ' “wörld”']) res.write(`data: ${JSON.stringify({ choices: [{ delta: { content: part } }] })}\n\n`);
    if (body.model === 'cut') return res.end();
    res.write(`data: ${JSON.stringify({ choices: [{ delta: {}, finish_reason: 'stop' }] })}\n\n`);
    res.end('data: [DONE]\n\n');
  });
  server.listen(port, '127.0.0.1');
  server.unref();
  return state;
}

const MOCK = { node: mockProvider(2571), rust: mockProvider(2572) };
const FLOW = '00000000-0000-4000-8000-00000000f101';

async function seed({ sql, backend }) {
  await sql(`INSERT INTO users (id, username, password_hash) VALUES ($1, 'alice@example.test', 'x'), ($2, 'bob@example.test', 'x') ON CONFLICT DO NOTHING`, [U.alice, U.bob]);
  await sql(`UPDATE users SET is_admin = true, preferences = '{"language":"de"}'::jsonb WHERE id = $1`, [U.alice]);
  await sql(`UPDATE users SET is_admin = false, preferences = '{}'::jsonb WHERE id = $1`, [U.bob]);
  await sql(`DELETE FROM system_settings WHERE key = 'ai_config'`);
  await sql(`INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'true') ON CONFLICT (key) DO UPDATE SET value = 'true'`);
  await sql(`DELETE FROM ai_codex_credentials`);
  await sql(`DELETE FROM ai_codex_device_flows`);
  MOCK[backend].requests.length = 0;
  return { keep: [U.alice, U.bob, FLOW], backend, base: `http://127.0.0.1:${MOCK[backend].port}/v1/` };
}

const sessions = () => ({ admin: { userId: U.alice, username: 'alice@example.test' }, user: { userId: U.bob } });
const admin = (method, path, body) => ({ method, path: `/api/admin${path}`, as: 'admin', ...(body !== undefined ? { body } : {}) });
const cfg = (c, model, extra = {}) => admin('PATCH', '/ai', { provider: 'api-key', apiKeyConfig: { baseUrl: c.base, apiKey: '••••••••', model }, ...extra });
const chat = (as, messages) => ({ method: 'POST', path: '/api/ai/chat', as, body: { messages } });
const hello = [{ role: 'user', content: 'Say hello' }];

export default [{
  name: 'AI: configuration, test, status, streaming chat and ChatGPT device errors',
  seed,
  sessions,
  // Each backend has its own mock port.
  ignoreKeys: ['baseUrl'],
  steps: [
    admin('GET', '/ai'),
    { method: 'GET', path: '/api/admin/ai', as: 'user' },
    { path: '/api/ai/status', as: 'user' },
    chat('user', hello),
    admin('PATCH', '/ai', { provider: 'nope' }),
    admin('PATCH', '/ai', { provider: 'chatgpt', chatgptConfig: { model: ' ' } }),
    admin('PATCH', '/ai', { provider: 'api-key', apiKeyConfig: { baseUrl: '', model: '' } }),
    admin('PATCH', '/ai', { provider: 'api-key', apiKeyConfig: { baseUrl: 'not a url', model: 'm' } }),
    (c) => admin('PATCH', '/ai', { provider: 'api-key', apiKeyConfig: { baseUrl: c.base, apiKey: 'sk-secret-1', model: ' m1 ' }, features: { summarize: false } }),
    admin('GET', '/ai'),
    (c) => cfg(c, 'm1'),
    { path: '/api/ai/status', as: 'user' },
    admin('POST', '/ai/test'),
    chat('user', []),
    chat('user', [{ role: 'tool', content: 'x' }]),
    chat('user', [{ role: 'user' }]),
    chat('user', [{ role: 'user', content: 'x'.repeat(32001) }]),
    chat('user', hello),
    chat('admin', [{ role: 'system', content: 'Be brief' }, ...hello]),
    (c) => cfg(c, 'cut'),
    chat('user', hello),
    (c) => cfg(c, 'bad'),
    admin('POST', '/ai/test'),
    chat('user', hello),
    (c) => cfg(c, 'empty'),
    admin('POST', '/ai/test'),
    admin('GET', '/ai/codex/status'),
    admin('POST', '/ai/codex/device/poll', {}),
    admin('POST', '/ai/codex/device/poll', { flowId: 'nope' }),
    admin('POST', '/ai/codex/device/poll', { flowId: FLOW }),
    admin('DELETE', '/ai/codex/device', { flowId: FLOW }),
    admin('DELETE', '/ai/codex'),
    admin('PATCH', '/ai', { provider: 'chatgpt', chatgptConfig: { model: 'gpt-x' } }),
    { path: '/api/ai/status', as: 'user' },
    chat('user', hello),
    admin('DELETE', '/ai'),
    admin('GET', '/ai'),
  ],
  after: async ({ ctx }) => ({ requests: MOCK[ctx.backend].requests.map(({ path, auth, body }) => ({ path, auth, body })) }),
}];
