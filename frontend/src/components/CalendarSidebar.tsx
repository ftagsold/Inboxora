import { calendarSyncWarning } from '../utils/calendarSyncWarning.ts';
import { useBackLayer } from '../hooks/useBackNavigation.ts';
import { useEffect, useMemo, useRef, useState } from 'react';
import { api } from '../utils/api.ts';
import { Button, Dialog } from './ui.tsx';
import CalendarSourceIntervalSelect from './CalendarSourceIntervalSelect.tsx';
import type { CSSProperties, FormEvent } from 'react';
import { toAppError } from '../utils/errors.ts';
import type { TFunction } from 'i18next';

/** One calendar a CalDAV account exposes, discovered or already imported. */
interface SourceCollection { id?: string; href?: string; displayName?: string; color?: string | null; selected?: boolean }

/** A calendar source as GET /calendar/sources returns it. */
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

/** Response of GET /calendar/sources. */
interface CalendarSourceList {
  sources?: CalendarSource[];
}

/** A calendar row as the sidebar receives it (local rows carry ownership fields). */
interface CalendarRow {
  id: string;
  name?: string | null;
  color?: string | null;
  source?: string | null;
  read_only?: boolean | null;
  owner_user_id?: string | null;
  display_visible?: boolean | null;
  custom_name?: boolean | null;
  [key: string]: unknown;
}

/** The appearance dialog's draft: the calendar plus its editable field values. */
interface CalendarEditDraft {
  calendar: CalendarRow;
  name: string;
  color: string;
}

/** Whether a value carries the id an external calendar source always has. */
function isCalendarSource(value: unknown): value is CalendarSource {
  return typeof value === 'object' && value !== null && 'id' in value && typeof value.id === 'string';
}

/** The source attached to a failed createSource response, when it has one. */
function failedCalendarSource(error: unknown): CalendarSource | null {
  if (typeof error !== 'object' || error === null || !('source' in error)) return null;
  return isCalendarSource(error.source) ? error.source : null;
}

function monthCells(anchor: Date, weekStartsOn: number): Date[] {
  const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
  const offset = (first.getDay() - weekStartsOn + 7) % 7;
  first.setDate(first.getDate() - offset);
  return Array.from({ length: 42 }, (_, index) => {
    const date = new Date(first);
    date.setDate(first.getDate() + index);
    return date;
  });
}

/** The calendar rail: mini month, source panel and calendar toggles. */
interface CalendarSidebarProps {
  anchor: Date;
  calendars: CalendarRow[];
  visibleCalendarIds: string[] | null;
  weekStartsOn?: number;
  locale: string;
  onSelectDate: (date: Date) => void;
  onShiftMonth: (delta: number) => void;
  onToggleCalendar: (id: string) => void;
  onSourcesChanged: () => void;
  onCalendarsChanged: () => void;
  onCreate: () => void;
  canCreate: boolean;
  sourcePanelRequest?: number;
  t: TFunction;
}
export default function CalendarSidebar({ anchor, calendars, visibleCalendarIds, weekStartsOn = 1, locale, onSelectDate, onShiftMonth, onToggleCalendar, onSourcesChanged, onCalendarsChanged, onCreate, canCreate, sourcePanelRequest = 0, t }: CalendarSidebarProps) {
  const [showSources, setShowSources] = useState(false);
  const [sources, setSources] = useState<CalendarSource[]>([]);
  const [sourceError, setSourceError] = useState<string | null>(null);
  const mounted = useRef(false);
  const pendingSourceIds = useRef(new Set<string>());
  const sourcePolls = useRef(new Map<string, ReturnType<typeof setTimeout>>());
  const sourceRequestGeneration = useRef(0);
  const [form, setForm] = useState({ kind: 'ical_url', displayName: '', url: '', username: '', password: '', color: '#7c6af7', intervalSeconds: 3600, writable: false });
  const [openCalendarMenu, setOpenCalendarMenu] = useState<string | null>(null);
  const [syncingSourceIds, setSyncingSourceIds] = useState<Set<string>>(new Set());
  const [calendarEdit, setCalendarEdit] = useState<CalendarEditDraft | null>(null);
  const [editError, setEditError] = useState<string | null>(null);
  const [calendarSaving, setCalendarSaving] = useState(false);
  // Discovery for a CalDAV account that may expose more than one calendar: `discovered`
  // is null until "Discover calendars" runs; once populated, a checkbox list replaces
  // the plain URL field and the submit button switches to actually creating the source.
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [discovered, setDiscovered] = useState<Array<{ href: string; displayName: string; color: string | null }> | null>(null);
  const [selectedHrefs, setSelectedHrefs] = useState<Set<string>>(new Set());
  const [editingSource, setEditingSource] = useState<CalendarSource | null>(null);
  const [editForm, setEditForm] = useState({ displayName: '', color: '#7c6af7', intervalSeconds: 3600, writable: false, enabled: true, username: '', password: '', url: '' });
  const [editCollections, setEditCollections] = useState<SourceCollection[] | null>(null);
  const [editDiscovering, setEditDiscovering] = useState(false);
  const [editSourceError, setEditSourceError] = useState<string | null>(null);
  const [editSaving, setEditSaving] = useState(false);
  useBackLayer(openCalendarMenu, () => { if (!calendarSaving) setOpenCalendarMenu(null); }, 4510);
  const cells = useMemo(() => monthCells(anchor, weekStartsOn), [anchor, weekStartsOn]);
  const weekdays = useMemo(() => Array.from({ length: 7 }, (_, index) => new Date(2026, 0, 4 + ((index + weekStartsOn) % 7)).toLocaleDateString(locale, { weekday: 'short' })), [locale, weekStartsOn]);
  const isVisible = (id: string) => visibleCalendarIds == null || visibleCalendarIds.includes(id);
  const clearSourcePoll = (id: string) => {
    const timer = sourcePolls.current.get(id);
    if (timer) clearTimeout(timer);
    sourcePolls.current.delete(id);
    pendingSourceIds.current.delete(id);
  };
  useEffect(() => {
    mounted.current = true;
    const polls = sourcePolls.current;
    const pending = pendingSourceIds.current;
    return () => {
      mounted.current = false;
      polls.forEach(timer => clearTimeout(timer));
      polls.clear();
      pending.clear();
    };
  }, []);
  const loadSources = async (): Promise<CalendarSourceList | null> => {
    const generation = sourceRequestGeneration.current;
    try {
      const result: CalendarSourceList = await api.calendar.listSources();
      if (!mounted.current || generation !== sourceRequestGeneration.current) return result;
      setSources(result.sources || []); setSourceError(null);
      return result;
    } catch (error) {
      if (!mounted.current || generation !== sourceRequestGeneration.current) return null;
      pendingSourceIds.current.forEach(clearSourcePoll);
      if (mounted.current) setSourceError(toAppError(error).message);
      return null;
    }
  };
  const waitForInitialSync = (sourceId: string) => {
    if (!sourceId || !mounted.current) return;
    clearSourcePoll(sourceId);
    pendingSourceIds.current.add(sourceId);
    let attempts = 0;
    const maxAttempts = 70;
    const poll = async () => {
      if (!mounted.current || !pendingSourceIds.current.has(sourceId)) return;
      const result = await loadSources();
      if (!mounted.current || !pendingSourceIds.current.has(sourceId)) return;
      const source = result?.sources?.find(item => item.id === sourceId);
      if (source?.lastSyncAt || source?.lastError) {
        clearSourcePoll(sourceId);
        await onSourcesChanged();
        return;
      }
      attempts += 1;
      if (attempts >= maxAttempts) {
        clearSourcePoll(sourceId);
        setSourceError(t('calendar.sourceSyncTimeout', 'Source synchronization timed out.'));
        return;
      }
      sourcePolls.current.set(sourceId, setTimeout(poll, 500));
    };
    poll();
  };
  const openSources = async () => { setShowSources(true); await loadSources(); };
  useEffect(() => {
    if (!sourcePanelRequest) return;
    let active = true;
    setShowSources(true);
    api.calendar.listSources()
      .then(result => {
        if (!active || !mounted.current) return;
        setSources(result.sources || []); setSourceError(null);
      })
      .catch(error => {
        if (!active || !mounted.current) return;
        setSourceError(toAppError(error).message);
      });
    return () => { active = false; };
  }, [sourcePanelRequest]);
  const resetAddForm = () => {
    setForm({ kind: 'ical_url', displayName: '', url: '', username: '', password: '', color: '#7c6af7', intervalSeconds: 3600, writable: false });
    setDiscovered(null); setSelectedHrefs(new Set()); setDiscoveryError(null);
  };
  // A CalDAV account may expose more than one calendar: discovery runs first and shows
  // a checkbox per calendar found, skipping straight to the picker's single implicit
  // selection when there's exactly one — no extra step for the common case.
  const discoverSourceCalendars = async () => {
    setDiscovering(true); setDiscoveryError(null);
    try {
      const result = await api.calendar.discoverSource({ url: form.url, username: form.username, password: form.password });
      const calendars = result?.calendars || [];
      setDiscovered(calendars);
      setSelectedHrefs(new Set(calendars.map((entry: { href: string }) => entry.href)));
    } catch (error) {
      setDiscoveryError(toAppError(error).message);
    } finally {
      setDiscovering(false);
    }
  };
  const addSource = async (event: FormEvent) => {
    event.preventDefault();
    if (form.kind === 'caldav' && discovered === null) return discoverSourceCalendars();
    try {
      const collections = form.kind === 'caldav' && discovered
        ? discovered.map(entry => ({ href: entry.href, displayName: entry.displayName, color: entry.color, selected: selectedHrefs.has(entry.href) }))
        : undefined;
      const result = await api.calendar.createSource({ ...form, password: form.kind === 'caldav' ? form.password : undefined, username: form.kind === 'caldav' ? form.username : undefined, writable: form.kind === 'caldav' ? form.writable : undefined, collections });
      resetAddForm();
      await loadSources();
      await onSourcesChanged();
      if (result?.sync?.pending) waitForInitialSync(result.source?.id);
    } catch (error) {
      const failedSource = failedCalendarSource(error);
      if (failedSource) {
        setSources(current => [...current.filter(source => source.id !== failedSource.id), failedSource]);
        try { await onSourcesChanged(); } catch { /* keep the persisted source visible even if refresh fails */ }
      }
      setSourceError(toAppError(error).message);
    }
  };
  const removeSource = async (id: string) => {
    sourceRequestGeneration.current += 1;
    try { await api.calendar.deleteSource(id); clearSourcePoll(id); await loadSources(); await onSourcesChanged(); }
    catch (error) { setSourceError(toAppError(error).message); }
  };
  const syncSource = async (id: string) => {
    if (syncingSourceIds.has(id)) return;
    setSyncingSourceIds(current => new Set(current).add(id));
    try { await api.calendar.syncSource(id); await loadSources(); await onSourcesChanged(); }
    catch (error) { setSourceError(toAppError(error).message); }
    finally { setSyncingSourceIds(current => { const next = new Set(current); next.delete(id); return next; }); }
  };
  // The cadence is per calendar, so it saves on change rather than behind a save
  // button: one control, one decision. The row already shows the resulting state.
  const changeSourceInterval = async (source: CalendarSource, intervalSeconds: number) => {
    const previous = source.intervalSeconds;
    setSources(current => current.map(item => item.id === source.id ? { ...item, intervalSeconds } : item));
    try { await api.calendar.updateSource(source.id, { intervalSeconds }); }
    catch (error) {
      // Put the previous value back, so the control never claims a cadence the server
      // did not accept.
      setSources(current => current.map(item => item.id === source.id ? { ...item, intervalSeconds: previous } : item));
      setSourceError(toAppError(error).message);
    }
  };
  const openEditSource = (source: CalendarSource) => {
    setEditingSource(source); setEditSourceError(null);
    setEditForm({ displayName: source.displayName || '', color: (source.color as string) || '#7c6af7', intervalSeconds: source.intervalSeconds || 3600, writable: Boolean(source.writable), enabled: source.enabled !== false, username: '', password: '', url: '' });
    setEditCollections(source.collections ? source.collections.map(entry => ({ ...entry })) : null);
  };
  // Re-run discovery from the edit dialog (a newly created remote calendar won't
  // appear until this runs) and replace the whole selection with the fresh one.
  // The server URL isn't in `editingSource` — the API never echoes a stored source
  // URL back — so refreshing asks for it again, same as the credentials. The URL
  // itself stays optional: a blank one plus an email-shaped username resolves the
  // server from the address's domain, same as when the source was first added.
  const refreshEditCollections = async () => {
    if (!editForm.username.trim() || !editForm.password) {
      setEditSourceError(t('calendar.editRefreshNeedsCredentials'));
      return;
    }
    setEditDiscovering(true); setEditSourceError(null);
    try {
      const result = await api.calendar.discoverSource({ url: editForm.url, username: editForm.username, password: editForm.password });
      const calendars = result?.calendars || [];
      setEditCollections(calendars.map((entry: { href: string; displayName: string; color: string | null }) => ({ href: entry.href, displayName: entry.displayName, color: entry.color, selected: true })));
    } catch (error) {
      setEditSourceError(toAppError(error).message);
    } finally {
      setEditDiscovering(false);
    }
  };
  const toggleEditCollection = (id: string | undefined) => {
    setEditCollections(current => current ? current.map(entry => (entry.id ?? entry.href) === id ? { ...entry, selected: !entry.selected } : entry) : current);
  };
  const saveEditSource = async () => {
    if (!editingSource) return;
    setEditSaving(true); setEditSourceError(null);
    try {
      const payload: Record<string, unknown> = { displayName: editForm.displayName.trim(), color: editForm.color, intervalSeconds: editForm.intervalSeconds, enabled: editForm.enabled };
      if (editingSource.kind === 'caldav') payload.writable = editForm.writable;
      if (editForm.username.trim()) payload.username = editForm.username.trim();
      if (editForm.password) payload.password = editForm.password;
      if (editCollections) payload.collections = editCollections.map(entry => entry.href ? { href: entry.href, displayName: entry.displayName, color: entry.color, selected: entry.selected } : { id: entry.id, selected: entry.selected });
      await api.calendar.updateSource(editingSource.id, payload);
      setEditingSource(null); setEditCollections(null);
      await loadSources();
      await onSourcesChanged();
    } catch (error) {
      setEditSourceError(toAppError(error).message);
    } finally {
      setEditSaving(false);
    }
  };
  const ownedCalendar = (calendar: CalendarRow) => Boolean(calendar.source === 'local' && !calendar.read_only && calendar.owner_user_id);
  const updateCalendarAppearance = async (calendar: CalendarRow, changes: { name?: string; color?: string }) => {
    setCalendarSaving(true); setEditError(null);
    try {
      await api.calendar.updateCalendar(calendar.id, { name: changes.name || calendar.name, color: changes.color || calendar.color, displayVisible: calendar.display_visible !== false, customName: Boolean(calendar.custom_name || changes.name !== calendar.name) });
      setOpenCalendarMenu(null); setCalendarEdit(null); await onCalendarsChanged?.();
    } catch (error) { setEditError(toAppError(error).message); } finally { setCalendarSaving(false); }
  };
  const editCalendar = (calendar: CalendarRow) => {
    setOpenCalendarMenu(null); setEditError(null);
    setCalendarEdit({ calendar, name: calendar.name ?? '', color: calendar.color || '#35558a' });
  };
  const deleteCalendar = async (calendar: CalendarRow) => {
    if (calendar.name === null || calendar.name === undefined) {
      throw new Error('Cannot delete a calendar without a name');
    }
    if (!window.confirm(t('calendar.confirmCalendarDelete', { name: calendar.name }))) return;
    setCalendarSaving(true); setSourceError(null);
    try { await api.calendar.deleteCalendar(calendar.id, calendar.name); setOpenCalendarMenu(null); await onCalendarsChanged?.(); }
    catch (error) { setSourceError(toAppError(error).message); } finally { setCalendarSaving(false); }
  };
  return <aside data-testid="calendar-sidebar" className="calendar-rail" style={panel} aria-label={t('calendar.panel')}>
    <h1 className="calendar-rail-heading">{t('calendar.title')}</h1>
    {onCreate && <Button variant="primary" className="calendar-rail-create" disabled={!canCreate} onClick={onCreate}>+ {t('calendar.newEvent')}</Button>}
    <div data-testid="calendar-mini-month" style={miniMonth}>
      <div style={miniMonthHeading}>
        <strong>{anchor.toLocaleDateString(locale, { month: 'long', year: 'numeric' })}</strong>
        <div style={miniMonthNavigation}>
          <button type="button" data-testid="calendar-mini-month-previous" aria-label={t('calendar.previousMonth')} onClick={() => onShiftMonth?.(-1)} className="calendar-mini-nav" style={miniMonthButton}>‹</button>
          <button type="button" data-testid="calendar-mini-month-next" aria-label={t('calendar.nextMonth')} onClick={() => onShiftMonth?.(1)} className="calendar-mini-nav" style={miniMonthButton}>›</button>
        </div>
      </div>
      <div style={weekdayGrid}>{weekdays.map((day, index) => <span data-testid="calendar-mini-weekday" key={index}>{day}</span>)}</div>
      <div style={dayGrid}>{cells.map(day => <button key={day.toISOString()} aria-pressed={day.toDateString() === anchor.toDateString()} aria-label={day.toLocaleDateString(locale, { day: 'numeric', month: 'long', year: 'numeric' })} className={`calendar-mini-day${day.toDateString() === anchor.toDateString() ? ' calendar-mini-selected' : ''}`} onClick={() => onSelectDate(day)} style={{ ...dayButton, ...(day.toDateString() === new Date().toDateString() ? today : {}), ...(day.getMonth() !== anchor.getMonth() ? muted : {}) }}>{day.getDate()}</button>)}</div>
    </div>
    <section style={section}>
      <div style={sectionHeading}><strong>{t('calendar.calendars')}</strong><button data-testid="calendar-sidebar-manage-sources" onClick={openSources} style={linkButton}>{t('calendar.manageSources')}</button></div>
      {[...calendars].sort((a, b) => Number(a.source !== 'local') - Number(b.source !== 'local')).map((calendar, index, all) => <div key={calendar.id}>{(index === 0 || (all[index - 1].source === 'local') !== (calendar.source === 'local')) && <h2 style={{ ...sectionHeading, margin: '12px 0 4px' }}>{calendar.source === 'local' ? t('calendar.myCalendars') : t('calendar.sourceCalendar')}</h2>}<div key={calendar.id} className="cal-row" style={calendarRow}><label style={calendarToggle}><input data-testid="calendar-visibility-toggle" type="checkbox" checked={isVisible(calendar.id)} onChange={() => onToggleCalendar(calendar.id)} /><span style={{ ...colorDot, background: calendar.color || 'var(--accent)' }} />{calendar.name}{ownedCalendar(calendar) ? <small style={owned}>{t('calendar.owned')}</small> : <small style={readOnly}>{t('calendar.sourceCalendar')}</small>}</label>{<div style={menuWrap}><button type="button" aria-label={t('calendar.calendarActions', { name: calendar.name })} aria-expanded={openCalendarMenu === calendar.id} onClick={() => setOpenCalendarMenu(openCalendarMenu === calendar.id ? null : calendar.id)} style={menuButton} disabled={calendarSaving}>⋮</button>{openCalendarMenu === calendar.id && <div role="menu" aria-label={t('calendar.calendarActions', { name: calendar.name })} style={contextMenu}><button role="menuitem" onClick={() => editCalendar(calendar)}>{t('calendar.rename')}</button><button role="menuitem" onClick={() => editCalendar(calendar)}>{t('calendar.changeColor')}</button>{ownedCalendar(calendar) && <button role="menuitem" onClick={() => deleteCalendar(calendar)} style={dangerButton}>{t('calendar.deleteCalendar')}</button>}</div>}</div>}</div></div>)}
    </section>
    {calendarEdit && <Dialog testId="calendar-appearance-dialog" title={t('calendar.calendarActions', { name: calendarEdit.calendar.name })} closeLabel={t('calendar.close')} busy={calendarSaving} onClose={() => setCalendarEdit(null)} footer={<>
      <Button onClick={() => setCalendarEdit(null)} disabled={calendarSaving}>{t('calendar.cancel')}</Button>
      <Button variant="primary" disabled={calendarSaving || !calendarEdit.name.trim() || !/^#[0-9a-f]{6}$/i.test(calendarEdit.color)} onClick={() => updateCalendarAppearance(calendarEdit.calendar, { name: calendarEdit.name.trim(), color: calendarEdit.color })}>{t(calendarSaving ? 'calendar.saving' : 'calendar.save')}</Button>
    </>}>
      <div className="ui-form">
        {editError && <p role="alert" className="ui-alert">{editError}</p>}
        <label>{t('calendar.renamePrompt')}<input maxLength={120} value={calendarEdit.name} onChange={event => setCalendarEdit(current => current ? { ...current, name: event.target.value } : current)} /></label>
        <label>{t('calendar.changeColor')}<input type="color" style={{ height: 44, padding: 4, boxSizing: 'border-box' }} value={calendarEdit.color} onChange={event => setCalendarEdit(current => current ? { ...current, color: event.target.value } : current)} /></label>
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
          {['#35558a', '#35793a', '#e879f9', '#e05252', '#d79a28', '#7c6af7'].map(color => <button key={color} type="button" aria-label={`${t('calendar.changeColor')} ${color}`} aria-pressed={calendarEdit.color === color} onClick={() => setCalendarEdit(current => current ? { ...current, color } : current)} style={{ width: 44, height: 44, borderRadius: 8, border: calendarEdit.color === color ? '3px solid var(--text-primary)' : '3px solid transparent', background: color }} />)}
        </div>
      </div>
    </Dialog>}
    {showSources && <Dialog title={t('calendar.manageSources')} closeLabel={t('calendar.close')} onClose={() => setShowSources(false)}>
      {sourceError && <p role="alert" style={error}>{sourceError}</p>}
      <form onSubmit={addSource} className="ui-form" style={formStyle}>
        <label>{t('calendar.sourceType')}<select value={form.kind} onChange={event => { setForm(current => ({ ...current, kind: event.target.value })); setDiscovered(null); setDiscoveryError(null); }}><option value="ical_url">{t('calendar.icsWebcal')}</option><option value="caldav">{t('calendar.caldav')}</option></select></label>
        <label>{t('calendar.sourceName')}<input required value={form.displayName} onChange={event => setForm(current => ({ ...current, displayName: event.target.value }))} /></label>
        <label>{t('calendar.sourceUrl')}<input required={form.kind !== 'caldav'} type="url" value={form.url} onChange={event => { setForm(current => ({ ...current, url: event.target.value })); setDiscovered(null); }} /></label>
        {form.kind === 'caldav' && <>
          <span className="settings-choice-description">{t('calendar.sourceUrlOptionalHint')}</span>
          <label>{t('calendar.sourceUsername')}<input required value={form.username} onChange={event => { setForm(current => ({ ...current, username: event.target.value })); setDiscovered(null); }} /></label>
          <label>{t('calendar.sourcePassword')}<input required type="password" autoComplete="new-password" value={form.password} onChange={event => { setForm(current => ({ ...current, password: event.target.value })); setDiscovered(null); }} /></label>
          <label style={collectionRow}><input type="checkbox" checked={form.writable} onChange={event => setForm(current => ({ ...current, writable: event.target.checked }))} /> {t('calendar.sourceWritable')}</label>
          {form.writable && <p className="settings-choice-description">{t('calendar.sourceWritableWarning')}</p>}
        </>}
        {discoveryError && <p role="alert" style={error}>{discoveryError}</p>}
        {discovered && discovered.length > 1 && <fieldset style={collectionPicker}>
          <legend>{t('calendar.sourcePickCalendars')}</legend>
          {discovered.map(entry => <label key={entry.href} style={collectionRow}>
            <input type="checkbox" checked={selectedHrefs.has(entry.href)} onChange={() => setSelectedHrefs(current => {
              const next = new Set(current);
              if (next.has(entry.href)) next.delete(entry.href); else next.add(entry.href);
              return next;
            })} />
            {entry.color && <span style={{ ...colorDot, background: entry.color }} />}
            {entry.displayName}
          </label>)}
        </fieldset>}
        <CalendarSourceIntervalSelect
          label={t('calendar.sourceSyncInterval')}
          value={form.intervalSeconds}
          onChange={value => setForm(current => ({ ...current, intervalSeconds: value }))}
          t={t}
        />
        <button type="submit" disabled={discovering || (form.kind === 'caldav' && discovered !== null && selectedHrefs.size === 0)} style={primaryButton}>
          {form.kind === 'caldav' && discovered === null ? (discovering ? t('calendar.sourceDiscovering') : t('calendar.sourceDiscover')) : t('calendar.addSource')}
        </button>
      </form>
      <div style={sourceList}>{sources.map(source => <div key={source.id} data-testid="calendar-source-row" style={sourceRow}><span style={sourceDetails}><strong>{source.displayName}</strong><small style={{ display: 'block' }}>{source.kind === 'caldav' ? t('calendar.caldav') : t('calendar.icsWebcal')}{source.kind === 'caldav' && source.writable ? ` · ${t('calendar.sourceWritable')}` : ''}{source.enabled === false ? ` · ${t('calendar.sourcePaused')}` : ''}</small><SourceStatus source={source} pending={pendingSourceIds.current.has(source.id) || syncingSourceIds.has(source.id)} t={t} /><CalendarSourceIntervalSelect label={t('calendar.sourceSyncInterval')} value={source.intervalSeconds} onChange={value => changeSourceInterval(source, value)} t={t} /></span><span style={sourceActions}><button onClick={() => openEditSource(source)} style={linkButton}>{t('calendar.editSourceButton')}</button><button disabled={syncingSourceIds.has(source.id) || pendingSourceIds.current.has(source.id)} onClick={() => syncSource(source.id)} style={linkButton}>{t('calendar.syncSource')}</button><button onClick={() => removeSource(source.id)} style={dangerButton}>{t('calendar.delete')}</button></span></div>)}</div>
    </Dialog>}
    {editingSource && <Dialog testId="calendar-source-edit-dialog" title={t('calendar.editSource', { name: editingSource.displayName })} closeLabel={t('calendar.close')} busy={editSaving} onClose={() => { setEditingSource(null); setEditCollections(null); }} footer={<>
      <Button onClick={() => { setEditingSource(null); setEditCollections(null); }} disabled={editSaving}>{t('calendar.cancel')}</Button>
      <Button variant="primary" disabled={editSaving || !editForm.displayName.trim()} onClick={saveEditSource}>{t(editSaving ? 'calendar.saving' : 'calendar.save')}</Button>
    </>}>
      <div className="ui-form">
        {editSourceError && <p role="alert" className="ui-alert">{editSourceError}</p>}
        <label>{t('calendar.sourceName')}<input required maxLength={120} value={editForm.displayName} onChange={event => setEditForm(current => ({ ...current, displayName: event.target.value }))} /></label>
        <label style={collectionRow}><input type="checkbox" checked={editForm.enabled} onChange={event => setEditForm(current => ({ ...current, enabled: event.target.checked }))} /> {t('calendar.sourceEnabled')}</label>
        {editingSource.kind === 'caldav' && <>
          <label>{t('calendar.sourceUsername')}<input placeholder={t('calendar.sourceKeepCurrent')} value={editForm.username} onChange={event => setEditForm(current => ({ ...current, username: event.target.value }))} /></label>
          <label>{t('calendar.sourcePassword')}<input type="password" autoComplete="new-password" placeholder={t('calendar.sourceKeepCurrent')} value={editForm.password} onChange={event => setEditForm(current => ({ ...current, password: event.target.value }))} /></label>
          <label style={collectionRow}><input type="checkbox" checked={editForm.writable} onChange={event => setEditForm(current => ({ ...current, writable: event.target.checked }))} /> {t('calendar.sourceWritable')}</label>
          {editForm.writable && <p className="settings-choice-description">{t('calendar.sourceWritableWarning')}</p>}
          {editCollections && <fieldset style={collectionPicker}>
            <legend>{t('calendar.sourcePickCalendars')}</legend>
            {editCollections.map(entry => <label key={entry.id ?? entry.href} style={collectionRow}>
              <input type="checkbox" checked={entry.selected !== false} onChange={() => toggleEditCollection(entry.id ?? entry.href)} />
              {entry.color && <span style={{ ...colorDot, background: entry.color }} />}
              {entry.displayName}
            </label>)}
          </fieldset>}
          <div className="settings-choice-description">{t('calendar.editRefreshDescription')}</div>
          <label>{t('calendar.sourceUrl')}<input type="url" value={editForm.url} onChange={event => setEditForm(current => ({ ...current, url: event.target.value }))} /></label>
          <span className="settings-choice-description">{t('calendar.sourceUrlOptionalHint')}</span>
          <div><Button disabled={editDiscovering} onClick={refreshEditCollections}>{editDiscovering ? t('calendar.sourceDiscovering') : t('calendar.editRefreshCalendars')}</Button></div>
        </>}
        <CalendarSourceIntervalSelect label={t('calendar.sourceSyncInterval')} value={editForm.intervalSeconds} onChange={value => setEditForm(current => ({ ...current, intervalSeconds: value }))} t={t} />
      </div>
    </Dialog>}
  </aside>;
}


function SourceStatus({ source, pending, t }: { source: { id: string; displayName?: string; kind?: string; intervalSeconds?: number; lastError?: string | null; lastSyncAt?: string | null; [key: string]: unknown }; pending: boolean; t: TFunction }) {
  const warning = calendarSyncWarning(source.lastError);
  if (pending) return <small>{t('calendar.sourceSyncing')}</small>;
  if (!warning) return <small>{t('calendar.sourceReady')}</small>;
  return <div><small role="status">{warning.count ? t('calendar.syncSkipped', { count: warning.count }) : t('calendar.syncFailed')}</small>
    <details style={{ marginTop: 6 }}><summary style={{ cursor: 'pointer', color: 'var(--text-secondary)' }}>{t('calendar.syncDetails')}</summary><pre style={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', maxHeight: 140, overflow: 'auto', fontSize: 11 }}>{warning.details}</pre></details>
  </div>;
}

const panel: CSSProperties = { boxSizing: 'border-box', flexShrink: 0, padding: 14, borderRight: '1px solid var(--border-subtle)', background: 'var(--bg-primary)', overflow: 'auto' };
const miniMonth: CSSProperties = { display: 'grid', gap: 2, padding: 8, marginBottom: 16, background: 'var(--bg-elevated)', border: '1px solid var(--border-subtle)', borderRadius: 8 };
const miniMonthHeading: CSSProperties = { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8, fontSize: 12, fontWeight: 600, padding: '2px 4px 6px' }; const miniMonthNavigation: CSSProperties = { display: 'flex', gap: 2 }; const miniMonthButton: CSSProperties = { minWidth: 28, minHeight: 28, padding: 0, border: '1px solid var(--border-subtle)', borderRadius: 6, background: 'transparent', color: 'var(--accent)', cursor: 'pointer', fontSize: 18, fontWeight: 650, lineHeight: 1 };
const weekdayGrid: CSSProperties = { display: 'grid', gridTemplateColumns: 'repeat(7, 1fr)', textAlign: 'center', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10, padding: '3px 0' };
const dayGrid: CSSProperties = { display: 'grid', gridTemplateColumns: 'repeat(7, 1fr)', gap: 1 };
// Sizing lives in CSS (.calendar-mini-day / .calendar-mini-nav) so a sheet can raise
// the touch target without inline styles winning over it.
const dayButton = { margin: '0 auto', padding: 0, border: 0, borderRadius: 5, background: 'transparent', color: 'var(--text-secondary)', cursor: 'pointer', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10.5, display: 'flex', alignItems: 'center', justifyContent: 'center' };
const today = { background: 'var(--accent)', color: 'var(--accent-text)', fontWeight: 600 }; const muted = { opacity: .38 };
const section: CSSProperties = { display: 'grid', gap: 4, paddingTop: 10 }; const sectionHeading: CSSProperties = { display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: 8, fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10, letterSpacing: '0.09em', textTransform: 'uppercase', color: 'var(--text-tertiary)' };
const calendarRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 4, padding: '6px 8px', borderRadius: 6, fontSize: 12.5, color: 'var(--text-secondary)' }; const calendarToggle: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, flex: 1, minWidth: 0, color: 'var(--text-secondary)', fontSize: 12.5, cursor: 'pointer' }; const colorDot = { width: 10, height: 10, borderRadius: 3 }; const readOnly: CSSProperties = { marginLeft: 'auto', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10 }; const owned: CSSProperties = { marginLeft: 'auto', color: 'var(--accent)', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10 }; const menuWrap: CSSProperties = { position: 'relative' }; const menuButton: CSSProperties = { border: 0, background: 'transparent', color: 'var(--text-secondary)', cursor: 'pointer', fontSize: 18, padding: '0 6px' }; const contextMenu: CSSProperties = { position: 'absolute', right: 0, top: '100%', zIndex: 3, display: 'grid', minWidth: 150, padding: 4, border: '1px solid var(--border)', borderRadius: 7, background: 'var(--bg-secondary)', boxShadow: '0 8px 22px rgba(0,0,0,.18)' };
const linkButton: CSSProperties = { border: 0, background: 'transparent', color: 'var(--accent)', cursor: 'pointer', padding: 4, fontWeight: 650 }; const dangerButton: CSSProperties = { ...linkButton, color: 'var(--red)' }; const primaryButton: CSSProperties = { border: 0, borderRadius: 7, background: 'var(--accent)', color: 'var(--accent-text)', padding: '8px 10px', cursor: 'pointer', fontWeight: 650 };
const formStyle: CSSProperties = { display: 'grid', gap: 10, minWidth: 0 }; const sourceList: CSSProperties = { display: 'grid', gap: 8, marginTop: 16, minWidth: 0 }; const sourceRow: CSSProperties = { display: 'flex', justifyContent: 'space-between', gap: 8, flexWrap: 'wrap', padding: 10, minWidth: 0, border: '1px solid var(--border-subtle)', borderRadius: 8 }; const sourceDetails: CSSProperties = { minWidth: 0, overflowWrap: 'anywhere' }; const sourceActions: CSSProperties = { display: 'flex', flexWrap: 'wrap', justifyContent: 'flex-end', gap: 4, minWidth: 0 }; const error: CSSProperties = { color: 'var(--red)' };
const collectionPicker: CSSProperties = { display: 'grid', gap: 6, padding: 10, border: '1px solid var(--border-subtle)', borderRadius: 8, margin: 0 }; const collectionRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, fontSize: 12.5, fontWeight: 400 };
