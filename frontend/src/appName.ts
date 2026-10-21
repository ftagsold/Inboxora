// Display name for app chrome. The frontend container fills VITE_APP_NAME into index.html at
// start-up (app-name.sh), so a deployment renames the app without rebuilding the image.
export const APP_NAME_PLACEHOLDER = '__INBOXORA_APP_NAME__';

/** The configured name from index.html, or null where the page was served unprocessed. */
export function runtimeAppName(doc: Pick<Document, 'querySelector'> | undefined): string | null {
  const value = doc?.querySelector('meta[name="application-name"]')?.getAttribute('content')?.trim();
  return value && value !== APP_NAME_PLACEHOLDER ? value : null;
}

// `typeof document` keeps this importable from node:test, which has no DOM.
export const APP_NAME = runtimeAppName(typeof document === 'undefined' ? undefined : document) || 'Inboxora';
