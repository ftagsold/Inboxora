import { useState } from 'react';
import type { CSSProperties, FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import type { WebDavServer } from '../utils/api.ts';
import { toAppError } from '../utils/errors.ts';
import { Button, Dialog, inputStyle } from './ui.tsx';
import { WEBDAV_SERVERS_CHANGED, defaultServer, serverHost, useWebDavServers } from './FilesPage.tsx';
import { useStore } from '../store/index.ts';

interface ServerForm { displayName: string; url: string; username: string; password: string }
const EMPTY: ServerForm = { displayName: '', url: '', username: '', password: '' };

const announce = () => window.dispatchEvent(new CustomEvent(WEBDAV_SERVERS_CHANGED));

/** Settings → Files: WebDAV servers browsed in Files and offered when attaching in compose. */
export default function WebDavFilesSettings() {
  const { t } = useTranslation();
  const { servers, error: loadError } = useWebDavServers();
  const defaultFileServerId = useStore(state => state.defaultFileServerId);
  const setDefaultFileServerId = useStore(state => state.setDefaultFileServerId);
  const defaultId = servers ? defaultServer(servers, defaultFileServerId)?.id : undefined;
  const [form, setForm] = useState<ServerForm>(EMPTY);
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<WebDavServer | null>(null);
  const [editForm, setEditForm] = useState<ServerForm>(EMPTY);
  const [editSaving, setEditSaving] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  const [removing, setRemoving] = useState<WebDavServer | null>(null);

  const submitAdd = async (event: FormEvent) => {
    event.preventDefault();
    setAdding(true); setError(null);
    try {
      await api.webdav.createServer({ displayName: form.displayName.trim(), url: form.url.trim(), username: form.username.trim(), password: form.password });
      setForm(EMPTY); announce();
    } catch (err) { setError(toAppError(err).message); } finally { setAdding(false); }
  };

  const openEdit = (server: WebDavServer) => {
    setEditing(server); setEditError(null);
    setEditForm({ displayName: server.displayName, url: server.url, username: server.username, password: '' });
  };

  const saveEdit = async () => {
    if (!editing) return;
    setEditSaving(true); setEditError(null);
    try {
      await api.webdav.updateServer(editing.id, {
        displayName: editForm.displayName.trim(), url: editForm.url.trim(), username: editForm.username.trim(),
        ...(editForm.password ? { password: editForm.password } : {}),
      });
      setEditing(null); announce();
    } catch (err) { setEditError(toAppError(err).message); } finally { setEditSaving(false); }
  };

  const confirmRemove = async () => {
    if (!removing) return;
    const server = removing;
    setRemoving(null); setError(null);
    try { await api.webdav.removeServer(server.id); announce(); } catch (err) { setError(toAppError(err).message); }
  };

  const fields = (value: ServerForm, change: (next: ServerForm) => void, editMode: boolean) => <>
    <label style={fieldStyle}>{t('admin.webdavFiles.name')}
      <input required maxLength={120} value={value.displayName} onChange={event => change({ ...value, displayName: event.target.value })} style={inputStyle} />
    </label>
    <label style={fieldStyle}>{t('admin.webdavFiles.url')}
      <input required type="url" value={value.url} onChange={event => change({ ...value, url: event.target.value })} style={inputStyle} />
    </label>
    <span className="settings-choice-description">{t('admin.webdavFiles.urlHint')}</span>
    <label style={fieldStyle}>{t('admin.webdavFiles.username')}
      <input required autoComplete="off" value={value.username} onChange={event => change({ ...value, username: event.target.value })} style={inputStyle} />
    </label>
    <label style={fieldStyle}>{t('admin.webdavFiles.password')}
      <input required={!editMode} type="password" autoComplete="new-password" placeholder={editMode ? t('admin.webdavFiles.keepPassword') : undefined} value={value.password} onChange={event => change({ ...value, password: event.target.value })} style={inputStyle} />
    </label>
  </>;

  const addReady = form.displayName.trim() && form.url.trim() && form.username.trim() && form.password;

  return <section data-testid="webdav-files-settings" style={section}>
    <div className="settings-switch-label">{t('admin.tabs.webdavFiles')}</div>
    <p className="settings-choice-description">{t('admin.webdavFiles.description')}</p>
    {(error || loadError) && <p role="alert" className="ui-alert">{error || loadError}</p>}

    <form onSubmit={submitAdd} className="ui-form" style={formStyle}>
      <div className="settings-switch-label">{t('admin.webdavFiles.addTitle')}</div>
      {fields(form, setForm, false)}
      <div><Button type="submit" variant="primary" disabled={adding || !addReady}>{adding ? t('admin.webdavFiles.connecting') : t('admin.webdavFiles.add')}</Button></div>
    </form>

    <div style={serverBlock}>
      <div className="settings-switch-label">{t('admin.webdavFiles.existingTitle')}</div>
      {servers && !servers.length && <p className="settings-choice-description">{t('admin.webdavFiles.empty')}</p>}
      {servers?.map(server => <div key={server.id} data-testid="webdav-server-row" style={serverRow}>
        <span style={serverText}>
          <strong>{server.displayName}</strong>
          <small>{serverHost(server)}</small>
          <small style={{ color: 'var(--text-tertiary)' }}>{server.url}</small>
        </span>
        <span style={serverActions}>
          {servers.length > 1 && (server.id === defaultId
            ? <span data-testid="webdav-server-default" style={defaultBadge}>{t('admin.webdavFiles.isDefault')}</span>
            : <Button onClick={() => setDefaultFileServerId(server.id)}>{t('admin.webdavFiles.makeDefault')}</Button>)}
          <Button onClick={() => openEdit(server)}>{t('admin.webdavFiles.edit')}</Button>
          <Button variant="danger" onClick={() => setRemoving(server)}>{t('admin.webdavFiles.remove')}</Button>
        </span>
      </div>)}
    </div>

    {editing && <Dialog testId="webdav-server-edit-dialog" title={t('admin.webdavFiles.editTitle', { name: editing.displayName })} closeLabel={t('files.close')} busy={editSaving} onClose={() => setEditing(null)} footer={<>
      <Button onClick={() => setEditing(null)} disabled={editSaving}>{t('files.cancel')}</Button>
      <Button variant="primary" disabled={editSaving || !editForm.displayName.trim() || !editForm.url.trim() || !editForm.username.trim()} onClick={saveEdit}>{editSaving ? t('admin.webdavFiles.connecting') : t('admin.webdavFiles.save')}</Button>
    </>}>
      <div className="ui-form">
        {editError && <p role="alert" className="ui-alert">{editError}</p>}
        {fields(editForm, setEditForm, true)}
      </div>
    </Dialog>}

    {removing && <Dialog testId="webdav-server-remove-dialog" title={t('admin.webdavFiles.removeTitle', { name: removing.displayName })} closeLabel={t('files.close')} onClose={() => setRemoving(null)} footer={<>
      <Button onClick={() => setRemoving(null)}>{t('files.cancel')}</Button>
      <Button variant="danger" autoFocus onClick={() => { void confirmRemove(); }}>{t('admin.webdavFiles.remove')}</Button>
    </>}>
      <p className="settings-choice-description">{t('admin.webdavFiles.removeWarning')}</p>
    </Dialog>}
  </section>;
}

const section: CSSProperties = { display: 'grid', gap: 10 };
const formStyle: CSSProperties = { display: 'grid', gap: 10, maxWidth: 560, marginTop: 8, padding: 12, border: '1px solid var(--border-subtle)', borderRadius: 8, background: 'var(--bg-elevated)' };
const fieldStyle: CSSProperties = { display: 'grid', gap: 6, fontSize: 12, color: 'var(--text-secondary)' };
const serverBlock: CSSProperties = { display: 'grid', gap: 8, marginTop: 18 };
const serverRow: CSSProperties = { display: 'flex', flexWrap: 'wrap', alignItems: 'center', justifyContent: 'space-between', gap: 8, padding: '9px 10px', border: '1px solid var(--border-subtle)', borderRadius: 8 };
const serverText: CSSProperties = { display: 'grid', gap: 2, minWidth: 0, overflowWrap: 'anywhere' };
const serverActions: CSSProperties = { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 6 };
const defaultBadge: CSSProperties = { fontSize: 11, fontWeight: 600, color: 'var(--accent)', border: '1px solid var(--accent)', borderRadius: 4, padding: '2px 6px' };
