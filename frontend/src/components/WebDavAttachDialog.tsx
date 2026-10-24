import { useCallback, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import type { WebDavEntry, WebDavServer } from '../utils/api.ts';
import { toAppError } from '../utils/errors.ts';
import { Button, Dialog } from './ui.tsx';
import FileExplorer, { ServerIcon, formatSize } from './FileExplorer.tsx';

/** The compose send limit for all attachments together (backend: 25 MB). */
export const ATTACHMENT_LIMIT = 25 * 1024 * 1024;
import { ServerCard } from './FilesPage.tsx';
import './files.css';

/** A compose attachment as ComposeModal keeps it (base64 content). */
export interface PickedAttachment { name: string; size: number; type: string; data: string }

/** "Attach from…": this device or one of the user's WebDAV servers. */
export function AttachSourceDialog({ servers, onLocal, onWebDav, onClose }: { servers: WebDavServer[]; onLocal: () => void; onWebDav: (serverId: string) => void; onClose: () => void }) {
  const { t } = useTranslation();
  return <Dialog testId="compose-attach-source" title={t('files.attachFrom')} closeLabel={t('files.close')} onClose={onClose}>
    <div className="files-attach-choices">
      <button type="button" className="files-attach-choice" data-testid="compose-attach-local" autoFocus onClick={onLocal}>
        <span className="files-server-badge"><DeviceIcon /></span>
        <span><strong>{t('files.attachLocal')}</strong><small>{t('files.attachLocalHint')}</small></span>
      </button>
      {servers.map(server => <button key={server.id} type="button" className="files-attach-choice" data-testid="compose-attach-webdav" onClick={() => onWebDav(server.id)}>
        <span className="files-server-badge"><ServerIcon /></span>
        <span><strong>{server.displayName}</strong><small>{t('files.attachWebDavHint')}</small></span>
      </button>)}
    </div>
  </Dialog>;
}

/** Browse a WebDAV server and attach the chosen files (fetched through the backend, 25 MB in total). */
/** `usedBytes`: what the message already carries, so the picker only offers what still fits. */
export function WebDavPickerDialog({ servers, initialServerId, usedBytes = 0, onAttach, onClose }: { servers: WebDavServer[]; initialServerId: string; usedBytes?: number; onAttach: (files: PickedAttachment[]) => void; onClose: () => void }) {
  const { t } = useTranslation();
  const [serverId, setServerId] = useState(initialServerId);
  const [picked, setPicked] = useState<WebDavEntry[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const server = servers.find(item => item.id === serverId) ?? servers[0];
  const onPickChange = useCallback((entries: WebDavEntry[]) => setPicked(entries), []);
  const total = picked.reduce((sum, entry) => sum + (entry.size ?? 0), 0);
  const budget = Math.max(0, ATTACHMENT_LIMIT - usedBytes);

  const attach = async () => {
    if (!picked.length) return;
    setBusy(true); setError(null);
    try {
      const result = await api.webdav.attachments(server.id, picked.map(entry => entry.path));
      onAttach(result.attachments);
    } catch (err) {
      setError(toAppError(err).message);
    } finally {
      setBusy(false);
    }
  };

  return <Dialog testId="compose-webdav-picker" className="files-picker-dialog" title={t('files.attachFromServer', { name: server.displayName })} closeLabel={t('files.close')} busy={busy} onClose={onClose} footer={<>
    <span className="files-preview-meta">{picked.length ? `${t('files.pickedCount', { count: picked.length })} · ${formatSize(total)} · ` : ''}{t('files.remaining', { size: formatSize(Math.max(0, budget - total)) })}</span>
    <Button onClick={onClose} disabled={busy}>{t('files.cancel')}</Button>
    <Button variant="primary" data-testid="compose-webdav-attach" disabled={busy || !picked.length} onClick={() => { void attach(); }}>{busy ? t('files.attaching') : t('files.attachCount', { count: picked.length })}</Button>
  </>}>
    <div className="files-picker-body">
      {servers.length > 1 && <div className="files-picker-servers">{servers.map(item => <ServerCard key={item.id} server={item} active={item.id === server.id} onSelect={() => { setServerId(item.id); setPicked([]); }} />)}</div>}
      {error && <div role="alert" className="ui-alert files-alert">{error}</div>}
      <FileExplorer key={server.id} server={server} mode="pick" onPickChange={onPickChange} pickBudget={budget} />
    </div>
  </Dialog>;
}

const DeviceIcon = () => <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><rect x="3" y="4" width="18" height="12" rx="2" /><path d="M8 20h8M12 16v4" /></svg>;
