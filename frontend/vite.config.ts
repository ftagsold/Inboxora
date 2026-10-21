import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';

const HTML_ESCAPES: Record<string, string> = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };

// Fills %APP_NAME% in index.html, so the tab title matches VITE_APP_NAME before the app mounts.
function appNameHtml(appName: string) {
  const escaped = appName.replace(/[&<>"']/g, char => HTML_ESCAPES[char]);
  return { name: 'inboxora-app-name', transformIndexHtml: (html: string) => html.replaceAll('%APP_NAME%', escaped) };
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
