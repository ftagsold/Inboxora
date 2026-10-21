import { useCallback, useEffect, useMemo, useState } from 'react';
import type { CSSProperties, FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import { Button, Dialog, inputStyle } from './ui.tsx';
import CalendarSourceIntervalSelect, { DEFAULT_SOURCE_SYNC_INTERVAL } from './CalendarSourceIntervalSelect.tsx';
import { calendarSyncWarning } from '../utils/calendarSyncWarning.ts';
import { intlLocale } from '../utils/intlLocale.ts';
import { toAppError } from '../utils/errors.ts';
import {
  HOLIDAY_CALENDARS,
  HOLIDAY_SYNC_INTERVAL_SECONDS,
  defaultHolidayCountry,
  holidayCalendarUrl,
  holidayCountryName,
  normalizeSubscriptionUrl,
} from '../utils/calendarSubscriptions.ts';

// The calendar page listens for this instead of sharing React state with the settings
// modal: adding a subscription here must refresh a calendar that is already mounted
// behind the dialog, exactly like accepting an invitation from the mail reader does.
function notifyCalendarChanged() {
  if (typeof window !== 'undefined') window.dispatchEvent(new Event('inboxora:calendar-changed'));
}

/** One calendar a CalDAV account exposes, as GET /calendar/sources returns it. */
interface SourceCollection { id?: string; displayName?: string; color?: string | null; selected?: boolean }

interface CalendarSource {
  id: string;
  displayName?: string;
  kind?: string;
  intervalSeconds?: number;
  writable?: boolean;
  enabled?: boolean;
  collections?: SourceCollection[] | null;
  lastError?: string | null;
  lastSyncAt?: string | null;
  [key: string]: unknown;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isCalendarSource(value: unknown): value is CalendarSource {
  if (!isRecord(value) || typeof value.id !== 'string') return false;
  return (value.displayName === undefined || typeof value.displayName === 'string')
    && (value.kind === undefined || typeof value.kind === 'string')
    && (value.intervalSeconds === undefined || typeof value.intervalSeconds === 'number')
    && (value.lastError === undefined || value.lastError === null || typeof value.lastError === 'string')
    && (value.lastSyncAt === undefined || value.lastSyncAt === null || typeof value.lastSyncAt === 'string');
}

function calendarSources(value: unknown): CalendarSource[] {
  if (!isRecord(value) || !Array.isArray(value.sources)) return [];
  return value.sources.filter(isCalendarSource);
}

export default function CalendarSubscriptionsSettings({ locale }: { locale?: string }) {
  const { t, i18n } = useTranslation();
  // The resource ids are not BCP 47 tags (zhCN), so spell them out before any Intl call.
  const language = intlLocale(locale || i18n.resolvedLanguage || i18n.language) || 'en';
  const [sources, setSources] = useState<CalendarSource[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [form, setForm] = useState({ displayName: '', url: '' });
  const [country, setCountry] = useState(() => defaultHolidayCountry(language));
  const [editingSource, setEditingSource] = useState<CalendarSource | null>(null);
  const [editForm, setEditForm] = useState({ displayName: '', intervalSeconds: DEFAULT_SOURCE_SYNC_INTERVAL, writable: false, enabled: true, username: '', password: '' });
  const [editCollections, setEditCollections] = useState<SourceCollection[] | null>(null);
  const [editError, setEditError] = useState<string | null>(null);
  const [editSaving, setEditSaving] = useState(false);

  const load = useCallback(async () => {
    try {
      const result = await api.calendar.listSources();
      // CalDAV accounts are managed exclusively from Settings → DAV servers now;
      // this panel stays focused on plain ICS/webcal subscriptions + holidays.
      setSources(calendarSources(result).filter(source => source.kind !== 'caldav'));
      setError(null);
    } catch (err) {
      setError(toAppError(err).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  // Localized country names come from Intl.DisplayNames, so the option list stays
  // language-aware without a translated country table in every locale file.
  const countries = useMemo(() => HOLIDAY_CALENDARS
    .map(entry => ({ ...entry, name: holidayCountryName(entry.code, language) }))
    .sort((a, b) => a.name.localeCompare(b.name, language)), [language]);

  const addSubscription = async ({ displayName, url, intervalSeconds }: { displayName: string; url: string; intervalSeconds: number }) => {
    setBusy(true); setError(null); setNotice(null);
    try {
      await api.calendar.createSource({ kind: 'ical_url', displayName, url, intervalSeconds });
      setNotice(t('calendar.subscribeSuccess'));
      await load();
      notifyCalendarChanged();
      return true;
    } catch (err) {
      setError(toAppError(err).message || t('calendar.subscribeFailed'));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const submitUrl = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const url = normalizeSubscriptionUrl(form.url);
    if (!url) return;
    const added = await addSubscription({ displayName: form.displayName.trim(), url, intervalSeconds: DEFAULT_SOURCE_SYNC_INTERVAL });
    if (added) setForm({ displayName: '', url: '' });
  };

  const addHolidays = async () => {
    const entry = countries.find(item => item.code === country);
    if (!entry) return;
    await addSubscription({
      displayName: t('calendar.holidayName', { country: entry.name }),
      url: holidayCalendarUrl(entry.file),
      intervalSeconds: HOLIDAY_SYNC_INTERVAL_SECONDS,
    });
  };

  const removeSource = async (id: string) => {
    setBusy(true); setError(null); setNotice(null);
    try {
      await api.calendar.deleteSource(id);
      await load();
      notifyCalendarChanged();
    } catch (err) {
      setError(toAppError(err).message || t('calendar.subscribeFailed'));
    } finally {
      setBusy(false);
    }
  };

  const syncSource = async (id: string) => {
    setBusy(true); setError(null); setNotice(null);
    try {
      await api.calendar.syncSource(id);
      await load();
      notifyCalendarChanged();
    } catch (err) {
      setError(toAppError(err).message || t('calendar.subscribeFailed'));
    } finally {
      setBusy(false);
    }
  };

  const openEditSource = (source: CalendarSource) => {
    setEditingSource(source); setEditError(null);
    setEditForm({ displayName: source.displayName || '', intervalSeconds: source.intervalSeconds || DEFAULT_SOURCE_SYNC_INTERVAL, writable: Boolean(source.writable), enabled: source.enabled !== false, username: '', password: '' });
    setEditCollections(source.collections ? source.collections.map(entry => ({ ...entry })) : null);
  };
  const toggleEditCollection = (id: string | undefined) => {
    setEditCollections(current => current ? current.map(entry => entry.id === id ? { ...entry, selected: !entry.selected } : entry) : current);
  };
  const saveEditSource = async () => {
    if (!editingSource) return;
    setEditSaving(true); setEditError(null);
    try {
      const payload: Record<string, unknown> = { displayName: editForm.displayName.trim(), intervalSeconds: editForm.intervalSeconds, enabled: editForm.enabled };
      if (editingSource.kind === 'caldav') payload.writable = editForm.writable;
      if (editForm.username.trim()) payload.username = editForm.username.trim();
      if (editForm.password) payload.password = editForm.password;
      if (editCollections) payload.collections = editCollections.map(entry => ({ id: entry.id, selected: entry.selected }));
      await api.calendar.updateSource(editingSource.id, payload);
      setEditingSource(null); setEditCollections(null);
      await load();
      notifyCalendarChanged();
    } catch (err) {
      setEditError(toAppError(err).message || t('calendar.subscribeFailed'));
    } finally {
      setEditSaving(false);
    }
  };

  return <section data-testid="calendar-subscriptions-settings" style={section}>
    <div className="settings-switch-label">{t('calendar.subscribeTitle')}</div>
    <p className="settings-choice-description">{t('calendar.subscribeDescription')}</p>
    {error && <p role="alert" className="ui-alert">{error}</p>}
    {notice && <p role="status" data-testid="calendar-subscription-notice" style={success}>{notice}</p>}
    <form onSubmit={submitUrl} style={formStyle}>
      <label style={fieldStyle}>{t('calendar.sourceName')}
        <input required maxLength={120} value={form.displayName} onChange={event => setForm(current => ({ ...current, displayName: event.target.value }))} style={inputStyle} />
      </label>
      <label style={fieldStyle}>{t('calendar.sourceUrl')}
        <input required type="url" placeholder="https://example.com/calendar.ics" value={form.url} onChange={event => setForm(current => ({ ...current, url: event.target.value }))} style={inputStyle} />
      </label>
      <span className="settings-choice-description">{t('calendar.subscribeHint')}</span>
      <div><Button type="submit" variant="primary" disabled={busy || !form.displayName.trim() || !form.url.trim()}>{t('calendar.subscribeAdd')}</Button></div>
    </form>
    <div style={holidayBlock}>
      <div className="settings-switch-label">{t('calendar.holidayTitle')}</div>
      <p className="settings-choice-description">{t('calendar.holidayDescription')}</p>
      <div style={holidayRow}>
        <label style={fieldStyle}>{t('calendar.holidayCountry')}
          <select value={country} onChange={event => setCountry(event.target.value)} style={inputStyle}>
            {countries.map(entry => <option key={entry.code} value={entry.code}>{entry.name}</option>)}
          </select>
        </label>
        <div><Button disabled={busy} onClick={addHolidays}>{t('calendar.holidayAdd')}</Button></div>
      </div>
    </div>
    <div style={sourceBlock}>
      <div className="settings-switch-label">{t('calendar.subscribeExisting')}</div>
      {!loading && !sources.length && <p className="settings-choice-description">{t('calendar.subscribeEmpty')}</p>}
      {sources.map(source => {
        const warning = calendarSyncWarning(source.lastError);
        const status = warning ? t('calendar.syncFailed') : (source.lastSyncAt ? t('calendar.sourceReady') : t('calendar.sourceSyncing'));
        return <div key={source.id} data-testid="calendar-subscription-row" style={sourceRow}>
          <span style={sourceText}>
            <strong>{source.displayName}</strong>
            <small>{source.kind === 'caldav' ? t('calendar.caldav') : t('calendar.icsWebcal')} &middot; {status}</small>
          </span>
          <span style={sourceActions}>
            <Button onClick={() => openEditSource(source)} disabled={busy}>{t('calendar.editSourceButton')}</Button>
            <Button onClick={() => syncSource(source.id)} disabled={busy}>{t('calendar.syncSource')}</Button>
            <Button variant="danger" onClick={() => removeSource(source.id)} disabled={busy}>{t('calendar.delete')}</Button>
          </span>
        </div>;
      })}
    </div>
    {editingSource && <Dialog testId="calendar-subscription-edit-dialog" title={t('calendar.editSource', { name: editingSource.displayName })} closeLabel={t('calendar.close')} busy={editSaving} onClose={() => { setEditingSource(null); setEditCollections(null); }} footer={<>
      <Button onClick={() => { setEditingSource(null); setEditCollections(null); }} disabled={editSaving}>{t('calendar.cancel')}</Button>
      <Button variant="primary" disabled={editSaving || !editForm.displayName.trim()} onClick={saveEditSource}>{t(editSaving ? 'calendar.saving' : 'calendar.save')}</Button>
    </>}>
      <div className="ui-form">
        {editError && <p role="alert" className="ui-alert">{editError}</p>}
        <label style={fieldStyle}>{t('calendar.sourceName')}
          <input required maxLength={120} value={editForm.displayName} onChange={event => setEditForm(current => ({ ...current, displayName: event.target.value }))} style={inputStyle} />
        </label>
        <label style={collectionRow}><input type="checkbox" checked={editForm.enabled} onChange={event => setEditForm(current => ({ ...current, enabled: event.target.checked }))} /> {t('calendar.sourceEnabled')}</label>
        {editingSource.kind === 'caldav' && <>
          <label style={fieldStyle}>{t('calendar.sourceUsername')}
            <input placeholder={t('calendar.sourceKeepCurrent')} value={editForm.username} onChange={event => setEditForm(current => ({ ...current, username: event.target.value }))} style={inputStyle} />
          </label>
          <label style={fieldStyle}>{t('calendar.sourcePassword')}
            <input type="password" autoComplete="new-password" placeholder={t('calendar.sourceKeepCurrent')} value={editForm.password} onChange={event => setEditForm(current => ({ ...current, password: event.target.value }))} style={inputStyle} />
          </label>
          <label style={collectionRow}><input type="checkbox" checked={editForm.writable} onChange={event => setEditForm(current => ({ ...current, writable: event.target.checked }))} /> {t('calendar.sourceWritable')}</label>
          {editForm.writable && <p className="settings-choice-description">{t('calendar.sourceWritableWarning')}</p>}
          {editCollections && editCollections.length > 1 && <fieldset style={collectionPicker}>
            <legend>{t('calendar.sourcePickCalendars')}</legend>
            {editCollections.map(entry => <label key={entry.id} style={collectionRow}>
              <input type="checkbox" checked={entry.selected !== false} onChange={() => toggleEditCollection(entry.id)} />
              {entry.color && <span style={{ ...colorDot, background: entry.color }} />}
              {entry.displayName}
            </label>)}
          </fieldset>}
        </>}
        <CalendarSourceIntervalSelect label={t('calendar.sourceSyncInterval')} value={editForm.intervalSeconds} onChange={value => setEditForm(current => ({ ...current, intervalSeconds: value }))} t={t} />
      </div>
    </Dialog>}
  </section>;
}

const section: CSSProperties = { display: 'grid', gap: 10, marginTop: 22, paddingTop: 18, borderTop: '1px solid var(--border-subtle)' };
const formStyle: CSSProperties = { display: 'grid', gap: 10, maxWidth: 560 };
const holidayBlock: CSSProperties = { display: 'grid', gap: 8, marginTop: 8, padding: 12, border: '1px solid var(--border-subtle)', borderRadius: 8, background: 'var(--bg-elevated)' };
const holidayRow: CSSProperties = { display: 'flex', flexWrap: 'wrap', alignItems: 'flex-end', gap: 10 };
const fieldStyle: CSSProperties = { display: 'grid', gap: 6, flex: '1 1 220px', minWidth: 0, fontSize: 12, color: 'var(--text-secondary)' };
const sourceBlock: CSSProperties = { display: 'grid', gap: 8, marginTop: 8 };
const sourceRow: CSSProperties = { display: 'flex', flexWrap: 'wrap', alignItems: 'center', justifyContent: 'space-between', gap: 8, padding: '9px 10px', border: '1px solid var(--border-subtle)', borderRadius: 8 };
const sourceText: CSSProperties = { display: 'grid', gap: 2, minWidth: 0, overflowWrap: 'anywhere' };
const sourceActions: CSSProperties = { display: 'flex', flexWrap: 'wrap', gap: 6 };
const success: CSSProperties = { color: 'var(--accent)', fontSize: 12 };
const collectionPicker: CSSProperties = { display: 'grid', gap: 6, padding: 10, border: '1px solid var(--border-subtle)', borderRadius: 8, margin: 0 };
const collectionRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, fontSize: 12.5, fontWeight: 400 };
const colorDot: CSSProperties = { width: 10, height: 10, borderRadius: 3 };
