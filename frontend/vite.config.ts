import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';

const HTML_ESCAPES: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };

// Production builds keep the __INBOXORA_APP_NAME__ placeholder, which the container fills
// from VITE_APP_NAME at start-up (app-name.sh). The dev server fills it the same way here.
function appNameHtml(appName: string) {
  const escaped = appName.replace(/[&<>"']/g, char => HTML_ESCAPES[char]);
  return {
    name: 'inboxora-app-name',
    apply: 'serve' as const,
    transformIndexHtml: (html: string) => html.replaceAll('__INBOXORA_APP_NAME__', escaped),
  };
}

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const apiTarget = env.VITE_E2E_API_TARGET || 'http://backend:3000';
  const proxy = env.VITE_E2E_MOCKED === 'true' ? {} : {
    '/api': { target: apiTarget, changeOrigin: true },
    '/ws': { target: apiTarget.replace(/^http/, 'ws'), ws: true },
  };
  return {
    plugins: [react(), appNameHtml(env.VITE_APP_NAME || 'Inboxora')],
    server: {
      port: 5173,
      proxy,
    },
  };
});
