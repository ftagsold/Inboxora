import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties, FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import { Button, Dialog, inputStyle } from './ui.tsx';
import CalendarSourceIntervalSelect from './CalendarSourceIntervalSelect.tsx';
import { toAppError } from '../utils/errors.ts';

/** One remote calendar/address book as discovery returns it (never persisted yet). */
interface DiscoveredCollection { href: string; displayName: string; color?: string | null }

/** One collection already attached to a server, as GET /dav/servers returns it —
 * only an opaque id (the href hash), never the href itself. */
interface ServerCollection { id: string; displayName?: string; color?: string | null; selected?: boolean }

/** A collection entry as it lives in edit-dialog state: either a fresh discovery
 * result (has `href`, no `id` yet) or an already-attached one (has `id`, no `href`) —
 * toggling and saving both key off `id ?? href`, matching the backend's dual mode. */
interface EditCollection { id?: string; href?: string; displayName?: string; color?: string | null; selected?: boolean }

interface DavServer {
  id: string;
  displayName?: string;
  username?: string;
  intervalSeconds?: number;
  writable?: boolean;
  enabled?: boolean;
  calendarSync?: { at?: string | null; error?: string | null };
  addressBookSync?: { at?: string | null; error?: string | null };
  calendars?: ServerCollection[] | null;
  addressBooks?: ServerCollection[] | null;
  [key: string]: unknown;
}

interface DiscoverResult {
  calendars?: DiscoveredCollection[];
  calendarsError?: string;
  addressBooks?: DiscoveredCollection[];
  addressBooksError?: string;
}

function collectionKey(entry: { id?: string; href?: string }): string | undefined {
  return entry.id ?? entry.href;
}

export default function DavServersSettings() {
  const { t } = useTranslation();
  const [servers, setServers] = useState<DavServer[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [syncingIds, setSyncingIds] = useState<Set<string>>(new Set());

  const [form, setForm] = useState({ displayName: '', url: '', username: '', password: '', intervalSeconds: 3600, writable: false });
  const [discovering, setDiscovering] = useState(false);
  const [discovered, setDiscovered] = useState<DiscoverResult | null>(null);
  const [selectedCalendars, setSelectedCalendars] = useState<Set<string>>(new Set());
  const [selectedAddressBooks, setSelectedAddressBooks] = useState<Set<string>>(new Set());
  const [creating, setCreating] = useState(false);

  const [editingServer, setEditingServer] = useState<DavServer | null>(null);
  const [editForm, setEditForm] = useState({ displayName: '', enabled: true, username: '', password: '', writable: false, intervalSeconds: 3600 });
  const [editCalendars, setEditCalendars] = useState<EditCollection[] | null>(null);
  const [editAddressBooks, setEditAddressBooks] = useState<EditCollection[] | null>(null);
  const [editDiscovering, setEditDiscovering] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  const [editSaving, setEditSaving] = useState(false);

  const load = useCallback(async () => {
    try {
      const result = await api.davServers.list();
      setServers(result.servers || []);
      setError(null);
    } catch (err) {
      setError(toAppError(err).message);
    } finally {
      setLoading(false);
    }
  }, []);
  useEffect(() => { load(); }, [load]);

  const resetAddForm = () => {
    setForm({ displayName: '', url: '', username: '', password: '', intervalSeconds: 3600, writable: false });
    setDiscovered(null); setSelectedCalendars(new Set()); setSelectedAddressBooks(new Set());
  };

  const discover = async () => {
    setDiscovering(true); setError(null);
    try {
      const result: DiscoverResult = await api.davServers.discover({ url: form.url, username: form.username, password: form.password });
      setDiscovered(result);
      setSelectedCalendars(new Set((result.calendars || []).map(entry => entry.href)));
      setSelectedAddressBooks(new Set((result.addressBooks || []).map(entry => entry.href)));
    } catch (err) {
      setError(toAppError(err).message);
    } finally {
      setDiscovering(false);
    }
  };

  const submitAdd = async (event: FormEvent) => {
    event.preventDefault();
    if (!discovered) return discover();
    setCreating(true); setError(null);
    try {
      const calendars = (discovered.calendars || [])
        .filter(entry => selectedCalendars.has(entry.href))
        .map(entry => ({ href: entry.href, displayName: entry.displayName, color: entry.color, selected: true }));
      const addressBooks = (discovered.addressBooks || [])
        .filter(entry => selectedAddressBooks.has(entry.href))
        .map(entry => ({ href: entry.href, displayName: entry.displayName, selected: true }));
      await api.davServers.create({
        displayName: form.displayName, url: form.url, username: form.username, password: form.password,
        intervalSeconds: form.intervalSeconds, writable: form.writable, calendars, addressBooks,
      });
      resetAddForm();
      await load();
    } catch (err) {
      setError(toAppError(err).message);
    } finally {
      setCreating(false);
    }
  };

  const removeServer = async (id: string) => {
    setError(null);
    try { await api.davServers.remove(id); await load(); }
    catch (err) { setError(toAppError(err).message); }
  };

  const syncServer = async (id: string) => {
    if (syncingIds.has(id)) return;
    setSyncingIds(current => new Set(current).add(id));
    try { await api.davServers.sync(id); await load(); }
    catch (err) { setError(toAppError(err).message); }
    finally { setSyncingIds(current => { const next = new Set(current); next.delete(id); return next; }); }
  };

  const changeInterval = async (server: DavServer, intervalSeconds: number) => {
    const previous = server.intervalSeconds;
    setServers(current => current.map(item => item.id === server.id ? { ...item, intervalSeconds } : item));
    try { await api.davServers.update(server.id, { intervalSeconds }); }
    catch (err) {
      setServers(current => current.map(item => item.id === server.id ? { ...item, intervalSeconds: previous } : item));
      setError(toAppError(err).message);
    }
  };

  const openEdit = (server: DavServer) => {
    setEditingServer(server); setEditError(null);
    setEditForm({ displayName: server.displayName || '', enabled: server.enabled !== false, username: '', password: '', writable: Boolean(server.writable), intervalSeconds: server.intervalSeconds || 3600 });
    setEditCalendars(server.calendars ? server.calendars.map(entry => ({ ...entry })) : null);
    setEditAddressBooks(server.addressBooks ? server.addressBooks.map(entry => ({ ...entry })) : null);
  };

  // Re-run discovery from the edit dialog and replace both selection lists with the
  // fresh result. Neither the server URL nor the credentials are ever echoed back by
  // the API, so both are asked for again here, the same as when the server was first added.
  const refreshEdit = async () => {
    if (!editForm.username.trim() || !editForm.password) {
      setEditError(t('calendar.editRefreshNeedsCredentials'));
      return;
    }
    setEditDiscovering(true); setEditError(null);
    try {
      const result: DiscoverResult = await api.davServers.discover({ username: editForm.username, password: editForm.password });
      setEditCalendars((result.calendars || []).map(entry => ({ href: entry.href, displayName: entry.displayName, color: entry.color, selected: true })));
      setEditAddressBooks((result.addressBooks || []).map(entry => ({ href: entry.href, displayName: entry.displayName, selected: true })));
    } catch (err) {
      setEditError(toAppError(err).message);
    } finally {
      setEditDiscovering(false);
    }
  };

  const toggleEditCollection = (list: 'calendars' | 'addressBooks', key: string | undefined) => {
    const setter = list === 'calendars' ? setEditCalendars : setEditAddressBooks;
    setter(current => current ? current.map(entry => collectionKey(entry) === key ? { ...entry, selected: !entry.selected } : entry) : current);
  };

  const saveEdit = async () => {
    if (!editingServer) return;
    setEditSaving(true); setEditError(null);
    try {
      const payload: Record<string, unknown> = { displayName: editForm.displayName.trim(), enabled: editForm.enabled, writable: editForm.writable, intervalSeconds: editForm.intervalSeconds };
      if (editForm.username.trim()) payload.username = editForm.username.trim();
      if (editForm.password) payload.password = editForm.password;
      if (editCalendars) payload.calendars = editCalendars.map(entry => entry.href ? { href: entry.href, displayName: entry.displayName, color: entry.color, selected: entry.selected } : { id: entry.id, selected: entry.selected });
      if (editAddressBooks) payload.addressBooks = editAddressBooks.map(entry => entry.href ? { href: entry.href, displayName: entry.displayName, selected: entry.selected } : { id: entry.id, selected: entry.selected });
      await api.davServers.update(editingServer.id, payload);
      setEditingServer(null); setEditCalendars(null); setEditAddressBooks(null);
      await load();
    } catch (err) {
      setEditError(toAppError(err).message);
    } finally {
      setEditSaving(false);
    }
  };

  const statusWord = (concern?: { at?: string | null; error?: string | null }) => {
    if (!concern) return null;
    if (concern.error) return t('calendar.syncFailed');
    return concern.at ? t('calendar.sourceReady') : t('calendar.sourceSyncing');
  };

  return <section data-testid="dav-servers-settings" style={section}>
    <div className="settings-switch-label">{t('admin.tabs.davServers')}</div>
    <p className="settings-choice-description">{t('admin.davServers.description')}</p>
    {error && <p role="alert" className="ui-alert">{error}</p>}

    <form onSubmit={submitAdd} className="ui-form" style={formStyle}>
      <div className="settings-switch-label">{t('admin.davServers.addTitle')}</div>
      <label style={fieldStyle}>{t('admin.davServers.serverName')}
        <input required maxLength={120} value={form.displayName} onChange={event => setForm(current => ({ ...current, displayName: event.target.value }))} style={inputStyle} />
      </label>
      <label style={fieldStyle}>{t('calendar.sourceUrl')}
        <input type="url" value={form.url} onChange={event => { setForm(current => ({ ...current, url: event.target.value })); setDiscovered(null); }} style={inputStyle} />
      </label>
      <span className="settings-choice-description">{t('calendar.sourceUrlOptionalHint')}</span>
      <label style={fieldStyle}>{t('calendar.sourceUsername')}
        <input required value={form.username} onChange={event => { setForm(current => ({ ...current, username: event.target.value })); setDiscovered(null); }} style={inputStyle} />
      </label>
      <label style={fieldStyle}>{t('calendar.sourcePassword')}
        <input required type="password" autoComplete="new-password" value={form.password} onChange={event => { setForm(current => ({ ...current, password: event.target.value })); setDiscovered(null); }} style={inputStyle} />
      </label>
      <label style={collectionRow}><input type="checkbox" checked={form.writable} onChange={event => setForm(current => ({ ...current, writable: event.target.checked }))} /> {t('calendar.sourceWritable')}</label>
      {form.writable && <p className="settings-choice-description">{t('calendar.sourceWritableWarning')}</p>}

      {discovered?.calendars && discovered.calendars.length > 0 && <fieldset style={collectionPicker}>
        <legend>{discovered.calendars.length > 1 ? t('calendar.sourcePickCalendars') : t('admin.davServers.calendarsLabel')}</legend>
        {discovered.calendars.map(entry => <label key={entry.href} style={collectionRow}>
          <input type="checkbox" checked={selectedCalendars.has(entry.href)} onChange={() => setSelectedCalendars(current => {
            const next = new Set(current);
            if (next.has(entry.href)) next.delete(entry.href); else next.add(entry.href);
            return next;
          })} />
          {entry.color && <span style={{ ...colorDot, background: entry.color }} />}
          {entry.displayName}
        </label>)}
      </fieldset>}
      {discovered?.calendarsError && <p role="alert" className="ui-alert">{discovered.calendarsError}</p>}

      {discovered?.addressBooks && discovered.addressBooks.length > 0 && <fieldset style={collectionPicker}>
        <legend>{discovered.addressBooks.length > 1 ? t('calendar.sourcePickAddressBooks') : t('admin.davServers.addressBooksLabel')}</legend>
        {discovered.addressBooks.map(entry => <label key={entry.href} style={collectionRow}>
          <input type="checkbox" checked={selectedAddressBooks.has(entry.href)} onChange={() => setSelectedAddressBooks(current => {
            const next = new Set(current);
            if (next.has(entry.href)) next.delete(entry.href); else next.add(entry.href);
            return next;
          })} />
          {entry.displayName}
        </label>)}
      </fieldset>}
      {discovered?.addressBooksError && <p role="alert" className="ui-alert">{discovered.addressBooksError}</p>}

      {discovered && <CalendarSourceIntervalSelect label={t('calendar.sourceSyncInterval')} value={form.intervalSeconds} onChange={value => setForm(current => ({ ...current, intervalSeconds: value }))} t={t} />}

      <div><Button type="submit" variant="primary" disabled={discovering || creating || !form.displayName.trim() || !form.username.trim() || !form.password || (discovered !== null && selectedCalendars.size === 0 && selectedAddressBooks.size === 0)}>
        {discovered === null ? (discovering ? t('calendar.sourceDiscovering') : t('calendar.sourceDiscover')) : (creating ? t('calendar.saving') : t('admin.davServers.addServer'))}
      </Button></div>
    </form>

    <div style={serverBlock}>
      <div className="settings-switch-label">{t('admin.davServers.existingTitle')}</div>
      {!loading && !servers.length && <p className="settings-choice-description">{t('admin.davServers.empty')}</p>}
      {servers.map(server => <div key={server.id} data-testid="dav-server-row" style={serverRow}>
        <span style={serverText}>
          <strong>{server.displayName}</strong>
          {server.enabled === false && <small>{t('calendar.sourcePaused')}</small>}
          {server.calendars != null && <small>{t('admin.davServers.calendarsLabel')}: {statusWord(server.calendarSync)}</small>}
          {server.addressBooks != null && <small>{t('admin.davServers.addressBooksLabel')}: {statusWord(server.addressBookSync)}</small>}
          <CalendarSourceIntervalSelect label={t('calendar.sourceSyncInterval')} value={server.intervalSeconds} onChange={value => changeInterval(server, value)} t={t} />
        </span>
        <span style={serverActions}>
          <Button onClick={() => openEdit(server)}>{t('calendar.editSourceButton')}</Button>
          <Button disabled={syncingIds.has(server.id)} onClick={() => syncServer(server.id)}>{t('calendar.syncSource')}</Button>
          <Button variant="danger" onClick={() => removeServer(server.id)}>{t('calendar.delete')}</Button>
        </span>
      </div>)}
    </div>

    {editingServer && <Dialog testId="dav-server-edit-dialog" title={t('calendar.editSource', { name: editingServer.displayName })} closeLabel={t('calendar.close')} busy={editSaving} onClose={() => { setEditingServer(null); setEditCalendars(null); setEditAddressBooks(null); }} footer={<>
      <Button onClick={() => { setEditingServer(null); setEditCalendars(null); setEditAddressBooks(null); }} disabled={editSaving}>{t('calendar.cancel')}</Button>
      <Button variant="primary" disabled={editSaving || !editForm.displayName.trim()} onClick={saveEdit}>{t(editSaving ? 'calendar.saving' : 'calendar.save')}</Button>
    </>}>
      <div className="ui-form">
        {editError && <p role="alert" className="ui-alert">{editError}</p>}
        <label style={fieldStyle}>{t('admin.davServers.serverName')}
          <input required maxLength={120} value={editForm.displayName} onChange={event => setEditForm(current => ({ ...current, displayName: event.target.value }))} style={inputStyle} />
        </label>
        <label style={collectionRow}><input type="checkbox" checked={editForm.enabled} onChange={event => setEditForm(current => ({ ...current, enabled: event.target.checked }))} /> {t('calendar.sourceEnabled')}</label>
        <label style={fieldStyle}>{t('calendar.sourceUsername')}
          <input placeholder={t('calendar.sourceKeepCurrent')} value={editForm.username} onChange={event => setEditForm(current => ({ ...current, username: event.target.value }))} style={inputStyle} />
        </label>
        <label style={fieldStyle}>{t('calendar.sourcePassword')}
          <input type="password" autoComplete="new-password" placeholder={t('calendar.sourceKeepCurrent')} value={editForm.password} onChange={event => setEditForm(current => ({ ...current, password: event.target.value }))} style={inputStyle} />
        </label>
        <label style={collectionRow}><input type="checkbox" checked={editForm.writable} onChange={event => setEditForm(current => ({ ...current, writable: event.target.checked }))} /> {t('calendar.sourceWritable')}</label>
        {editForm.writable && <p className="settings-choice-description">{t('calendar.sourceWritableWarning')}</p>}

        {editCalendars && editCalendars.length > 0 && <fieldset style={collectionPicker}>
          <legend>{t('admin.davServers.calendarsLabel')}</legend>
          {editCalendars.map(entry => <label key={collectionKey(entry)} style={collectionRow}>
            <input type="checkbox" checked={entry.selected !== false} onChange={() => toggleEditCollection('calendars', collectionKey(entry))} />
            {entry.color && <span style={{ ...colorDot, background: entry.color }} />}
            {entry.displayName}
          </label>)}
        </fieldset>}
        {editAddressBooks && editAddressBooks.length > 0 && <fieldset style={collectionPicker}>
          <legend>{t('admin.davServers.addressBooksLabel')}</legend>
          {editAddressBooks.map(entry => <label key={collectionKey(entry)} style={collectionRow}>
            <input type="checkbox" checked={entry.selected !== false} onChange={() => toggleEditCollection('addressBooks', collectionKey(entry))} />
            {entry.displayName}
          </label>)}
        </fieldset>}
        <div className="settings-choice-description">{t('calendar.editRefreshDescription')}</div>
        <div><Button disabled={editDiscovering} onClick={refreshEdit}>{editDiscovering ? t('calendar.sourceDiscovering') : t('admin.davServers.refresh')}</Button></div>
        <CalendarSourceIntervalSelect label={t('calendar.sourceSyncInterval')} value={editForm.intervalSeconds} onChange={value => setEditForm(current => ({ ...current, intervalSeconds: value }))} t={t} />
      </div>
    </Dialog>}
  </section>;
}

const section: CSSProperties = { display: 'grid', gap: 10 };
const formStyle: CSSProperties = { display: 'grid', gap: 10, maxWidth: 560, marginTop: 8, padding: 12, border: '1px solid var(--border-subtle)', borderRadius: 8, background: 'var(--bg-elevated)' };
const fieldStyle: CSSProperties = { display: 'grid', gap: 6, fontSize: 12, color: 'var(--text-secondary)' };
const serverBlock: CSSProperties = { display: 'grid', gap: 8, marginTop: 18 };
const serverRow: CSSProperties = { display: 'flex', flexWrap: 'wrap', alignItems: 'center', justifyContent: 'space-between', gap: 8, padding: '9px 10px', border: '1px solid var(--border-subtle)', borderRadius: 8 };
const serverText: CSSProperties = { display: 'grid', gap: 2, minWidth: 0, overflowWrap: 'anywhere' };
const serverActions: CSSProperties = { display: 'flex', flexWrap: 'wrap', gap: 6 };
const collectionPicker: CSSProperties = { display: 'grid', gap: 6, padding: 10, border: '1px solid var(--border-subtle)', borderRadius: 8, margin: 0 };
const collectionRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, fontSize: 12.5, fontWeight: 400 };
const colorDot: CSSProperties = { width: 10, height: 10, borderRadius: 3 };
