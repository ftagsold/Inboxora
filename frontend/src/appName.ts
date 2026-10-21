// Display name for app chrome, baked in at build time from VITE_APP_NAME (frontend/Dockerfile).
// `?.` keeps this importable from node:test, where import.meta.env does not exist.
export const APP_NAME = import.meta.env?.VITE_APP_NAME || 'Inboxora';
