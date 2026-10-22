// /api/push
import { seedMail, U, M } from '../seed-mail.mjs';

const sessions = () => ({ alice: { userId: U.alice }, bob: { userId: U.bob } });
const dev = (extra = {}) => ({ deviceId: ' pixel-1 ', platform: 'Android', transport: 'FCM', endpoint: 'fcm-token-abc', appVersion: '1.2.3', ...extra });

export default [{
  name: 'device registry and native API',
  seed: async (ctx) => { const r = await seedMail(ctx); await ctx.sql('DELETE FROM push_devices WHERE user_id IN ($1, $2)', [U.alice, U.bob]); return r; },
  sessions,
  ignoreKeys: ['deviceToken'],
  steps: [
    { path: '/api/push/devices' },
    { path: '/api/push/status', as: 'alice' },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev(), capture: (j) => ({ token: j.deviceToken }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ deviceId: 'up-1', transport: 'unifiedpush', endpoint: 'http://push.test/x' }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ deviceId: 'up-2', transport: 'unifiedpush', endpoint: 'nope' }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ deviceId: 'up-3', transport: 'unifiedpush', endpoint: 'https://127.0.0.1/x' }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ deviceId: '' }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ platform: 'symbian' }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ transport: 'apns' }) },
    { method: 'POST', path: '/api/push/devices', as: 'alice', body: dev({ endpoint: '  ' }) },
    { path: '/api/push/devices', as: 'alice' },
    { path: '/api/push/status', as: 'alice' },
    (c) => ({ path: '/api/push/native/inbox', headers: { authorization: `Bearer ${c.token}` } }),
    (c) => ({ path: `/api/push/native/messages/${M(1)}`, headers: { authorization: `bearer  ${c.token} ` } }),
    (c) => ({ path: `/api/push/native/messages/${M(7)}`, headers: { authorization: `Bearer ${c.token}` } }),
    (c) => ({ path: '/api/push/native/messages/xyz', headers: { authorization: `Bearer ${c.token}` } }),
    { path: '/api/push/native/inbox', headers: { authorization: 'Bearer mf_push_00000000-0000-4000-8000-000000000000.aaaaaaaaaaaaaaaaaaaa' } },
    { path: '/api/push/native/inbox' },
    { method: 'DELETE', path: '/api/push/devices/nope', as: 'alice' },
    { method: 'DELETE', path: '/api/push/devices/pixel-1', as: 'alice' },
    (c) => ({ path: '/api/push/native/inbox', headers: { authorization: `Bearer ${c.token}` } }),
    { method: 'DELETE', path: '/api/push/devices', as: 'alice' },
  ],
  after: async ({ sql }) => (await sql('SELECT device_id, platform, transport, app_version, endpoint LIKE \'enc:v1:%\' AS encrypted FROM push_devices ORDER BY device_id')).rows,
}];
