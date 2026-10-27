import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useStore } from '../store/index.ts';
import { api } from '../utils/api.ts';
import type { WebDavServer } from '../utils/api.ts';
import { toAppError } from '../utils/errors.ts';
import { useMobile } from '../hooks/useMobile.ts';
import { Button, EmptyState } from './ui.tsx';
import { MobileModuleHeader } from './MobileModuleHeader.tsx';
import FileExplorer, { ServerIcon } from './FileExplorer.tsx';
import './files.css';

/** Fired by Settings → Files whenever a WebDAV server is added, edited or removed. */
export const WEBDAV_SERVERS_CHANGED = 'inboxora:webdav-servers-changed';

export function serverHost(server: WebDavServer): string {
  try { const url = new URL(server.url); return `${server.username}@${url.host}`; } catch { return server.username; }
}

/** Load the user's WebDAV servers and keep them fresh when Settings changes them. */
export function useWebDavServers() {
  const [servers, setServers] = useState<WebDavServer[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => {
    try { const result = await api.webdav.listServers(); setServers(result.servers); setError(null); } catch (err) { setError(toAppError(err).message); setServers(current => current ?? []); }
  }, []);
  useEffect(() => {
    void load();
    const changed = () => { void load(); };
    window.addEventListener(WEBDAV_SERVERS_CHANGED, changed);
    return () => window.removeEventListener(WEBDAV_SERVERS_CHANGED, changed);
  }, [load]);
  return { servers, error, reload: load };
}

export function ServerCard({ server, active, onSelect }: { server: WebDavServer; active: boolean; onSelect: () => void }) {
  return <button type="button" className="files-server-card" aria-pressed={active} data-testid="files-server-card" onClick={onSelect}>
    <span className="files-server-badge"><ServerIcon /></span>
    <span className="files-server-text"><strong>{server.displayName}</strong><small>{serverHost(server)}</small></span>
  </button>;
}

/** The server Files opens with: the user's default when it still exists, else the first one. */
export function defaultServer(servers: WebDavServer[], defaultId: string): WebDavServer | null {
  return servers.find(server => server.id === defaultId) ?? servers[0] ?? null;
}

export default function FilesPage() {
  const { t } = useTranslation();
  const isMobile = useMobile();
  const { setAdminTab, setShowAdmin, defaultFileServerId } = useStore();
  const { servers, error } = useWebDavServers();
  // A server picked from the root crumb's menu holds for this visit; Files reopens on the default.
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selected = servers ? (servers.find(server => server.id === selectedId) ?? defaultServer(servers, defaultFileServerId)) : null;
  const manage = () => { setAdminTab('webdav-files'); setShowAdmin(true); };

  return <div className="files-page" data-testid="files-page">
    {isMobile && <MobileModuleHeader title={t('files.title')} subtitle={selected?.displayName} />}
    <main className="files-main">
      {error && <div role="alert" className="ui-alert files-alert">{error}</div>}
      {servers === null && <div className="files-loading" role="status"><span className="files-spinner" />{t('files.loading')}</div>}
      {servers && !selected && <EmptyState title={t('files.noServers')}>
        <span style={{ display: 'grid', gap: 12, justifyItems: 'center' }}>{t('files.noServersHint')}<Button variant="primary" onClick={manage}>+ {t('files.addServer')}</Button></span>
      </EmptyState>}
      {selected && servers && <FileExplorer key={selected.id} server={selected} servers={servers} onSelectServer={setSelectedId} onManageServers={manage} />}
    </main>
  </div>;
}
