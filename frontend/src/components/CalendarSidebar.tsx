import { useBackLayer } from '../hooks/useBackNavigation.ts';
import { useEffect, useMemo, useState } from 'react';
import { api } from '../utils/api.ts';
import { Button, Dialog } from './ui.tsx';
import type { CSSProperties } from 'react';
import { toAppError } from '../utils/errors.ts';
import type { TFunction } from 'i18next';
import { EditIcon, PaletteIcon, TrashIcon } from './menuIcons.tsx';

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

/** The calendar rail: mini month and calendar toggles. Adding, editing and
 * removing remote CalDAV/CardDAV servers lives in Settings → DAV servers now —
 * this pane only shows the resulting calendars and lets you show/hide, rename,
 * recolor or delete the ones you own. */
interface CalendarSidebarProps {
  anchor: Date;
  calendars: CalendarRow[];
  visibleCalendarIds: string[] | null;
  weekStartsOn?: number;
  locale: string;
  onSelectDate: (date: Date) => void;
  onShiftMonth: (delta: number) => void;
  onToggleCalendar: (id: string) => void;
  onCalendarsChanged: () => void;
  onCreate: () => void;
  canCreate: boolean;
  t: TFunction;
}
export default function CalendarSidebar({ anchor, calendars, visibleCalendarIds, weekStartsOn = 1, locale, onSelectDate, onShiftMonth, onToggleCalendar, onCalendarsChanged, onCreate, canCreate, t }: CalendarSidebarProps) {
  const [openCalendarMenu, setOpenCalendarMenu] = useState<string | null>(null);
  const [calendarEdit, setCalendarEdit] = useState<CalendarEditDraft | null>(null);
  const [editError, setEditError] = useState<string | null>(null);
  const [calendarSaving, setCalendarSaving] = useState(false);
  useBackLayer(openCalendarMenu, () => { if (!calendarSaving) setOpenCalendarMenu(null); }, 4510);
  // Close the "⋮" menu on an outside click. `.calendar-menu-wrap` covers both
  // the trigger button and the open menu, so clicking the button itself (which
  // already toggles the menu via its own onClick) isn't also treated as an
  // outside click — that would close-then-reopen it on every second click.
  useEffect(() => {
    if (!openCalendarMenu) return;
    const handler = (event: MouseEvent) => {
      if (calendarSaving) return;
      if (event.target instanceof Element && event.target.closest('.calendar-menu-wrap')) return;
      setOpenCalendarMenu(null);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [openCalendarMenu, calendarSaving]);
  const cells = useMemo(() => monthCells(anchor, weekStartsOn), [anchor, weekStartsOn]);
  const weekdays = useMemo(() => Array.from({ length: 7 }, (_, index) => new Date(2026, 0, 4 + ((index + weekStartsOn) % 7)).toLocaleDateString(locale, { weekday: 'short' })), [locale, weekStartsOn]);
  const isVisible = (id: string) => visibleCalendarIds == null || visibleCalendarIds.includes(id);
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
    setCalendarSaving(true); setEditError(null);
    try { await api.calendar.deleteCalendar(calendar.id, calendar.name); setOpenCalendarMenu(null); await onCalendarsChanged?.(); }
    catch (error) { setEditError(toAppError(error).message); } finally { setCalendarSaving(false); }
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
      {[...calendars].sort((a, b) => Number(a.source !== 'local') - Number(b.source !== 'local')).map(calendar => <div key={calendar.id} className="cal-row" style={calendarRow}><label style={calendarToggle}><input data-testid="calendar-visibility-toggle" type="checkbox" checked={isVisible(calendar.id)} onChange={() => onToggleCalendar(calendar.id)} /><span style={{ ...colorDot, background: calendar.color || 'var(--accent)' }} />{calendar.name}</label>{<div className="calendar-menu-wrap" style={menuWrap}><button type="button" aria-label={t('calendar.calendarActions', { name: calendar.name })} aria-expanded={openCalendarMenu === calendar.id} onClick={() => setOpenCalendarMenu(openCalendarMenu === calendar.id ? null : calendar.id)} style={menuButton} disabled={calendarSaving}>⋮</button>{openCalendarMenu === calendar.id && <div role="menu" aria-label={t('calendar.calendarActions', { name: calendar.name })} className="app-menu" style={contextMenu}><button type="button" role="menuitem" className="app-menu-item" onClick={() => editCalendar(calendar)}><EditIcon />{t('calendar.rename')}</button><button type="button" role="menuitem" className="app-menu-item" onClick={() => editCalendar(calendar)}><PaletteIcon />{t('calendar.changeColor')}</button>{ownedCalendar(calendar) && <button type="button" role="menuitem" className="app-menu-item danger" onClick={() => deleteCalendar(calendar)}><TrashIcon />{t('calendar.deleteCalendar')}</button>}</div>}</div>}</div>)}
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
  </aside>;
}

// 13px top centres the 30px heading on the sidebar header's 28px line; 16px sides match the mail list.
const panel: CSSProperties = { boxSizing: 'border-box', flexShrink: 0, padding: '13px 16px 14px', borderRight: '1px solid var(--border-subtle)', background: 'var(--bg-primary)', overflow: 'auto' };
const miniMonth: CSSProperties = { display: 'grid', gap: 2, padding: 8, marginBottom: 16, background: 'var(--bg-elevated)', border: '1px solid var(--border-subtle)', borderRadius: 8 };
const miniMonthHeading: CSSProperties = { display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8, fontSize: 12, fontWeight: 600, padding: '2px 4px 6px' }; const miniMonthNavigation: CSSProperties = { display: 'flex', gap: 2 }; const miniMonthButton: CSSProperties = { minWidth: 28, minHeight: 28, padding: 0, border: '1px solid var(--border-subtle)', borderRadius: 6, background: 'transparent', color: 'var(--accent)', cursor: 'pointer', fontSize: 18, fontWeight: 650, lineHeight: 1 };
const weekdayGrid: CSSProperties = { display: 'grid', gridTemplateColumns: 'repeat(7, 1fr)', textAlign: 'center', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10, padding: '3px 0' };
const dayGrid: CSSProperties = { display: 'grid', gridTemplateColumns: 'repeat(7, 1fr)', gap: 1 };
// Sizing lives in CSS (.calendar-mini-day / .calendar-mini-nav) so a sheet can raise
// the touch target without inline styles winning over it.
const dayButton = { margin: '0 auto', padding: 0, border: 0, borderRadius: 5, background: 'transparent', color: 'var(--text-secondary)', cursor: 'pointer', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10.5, display: 'flex', alignItems: 'center', justifyContent: 'center' };
const today = { background: 'var(--accent)', color: 'var(--accent-text)', fontWeight: 600 }; const muted = { opacity: .38 };
const section: CSSProperties = { display: 'grid', gap: 4, paddingTop: 10 };
const calendarRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 4, padding: '6px 8px', borderRadius: 6, fontSize: 12.5, color: 'var(--text-secondary)' }; const calendarToggle: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, flex: 1, minWidth: 0, color: 'var(--text-secondary)', fontSize: 12.5, cursor: 'pointer' }; const colorDot = { width: 10, height: 10, borderRadius: 3 }; const menuWrap: CSSProperties = { position: 'relative' }; const menuButton: CSSProperties = { border: 0, background: 'transparent', color: 'var(--text-secondary)', cursor: 'pointer', fontSize: 18, padding: '0 6px' }; const contextMenu: CSSProperties = { position: 'absolute', right: 0, top: '100%', zIndex: 3, minWidth: 190 }; // look: .app-menu (ui.css)
