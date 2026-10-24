import { safeHttpUrl } from '../utils/contactLinks.ts';
import { calendarDescriptionBody } from '../utils/richText.ts';
import { readStoredCalendarView, storeCalendarView } from '../utils/calendarPreferences.ts';
import { openDeepLinkMessage } from '../utils/gtd.ts';
import type { GtdThread } from '../utils/gtd.ts';
import MobileFloatingAction from './MobileFloatingAction.tsx';
import { localizeContactCalendar, localizeContactEvent } from '../utils/contactDateLabels.ts';
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import type { CalendarViewEvent, CalendarEventForm } from './calendarView.ts';
import { api, isAbortError } from '../utils/api.ts';
import { useStore } from '../store/index.ts';
import { useMobile } from '../hooks/useMobile.ts';
import { allDayEventSegment, allDaySpanDays, calendarVisibleRange, centeredScrollLeft, createDayEventsResolver, eventPayload, withOwnReply, isWritableCalendarSource, layoutTimedEvents, layoutWeekAllDayEvents, monthRange, shiftCalendarAnchor, toDateTimeLocal, toggleAllDayTimes, weekFocusIndex, weekRange, workHoursGeometry } from './calendarView.ts';
import CalendarSidebar from './CalendarSidebar.tsx';
import CalendarDeleteScopeDialog from './CalendarDeleteScopeDialog.tsx';
import CalendarEditScopeDialog from './CalendarEditScopeDialog.tsx';
import { createInvitationOperationController } from './calendarInvitationRetry.ts';
import CalendarContextMenu from './CalendarContextMenu.tsx';
import CalendarAgenda from './CalendarAgenda.tsx';
import MessageBodyRenderer from './MessageBodyRenderer.tsx';
import RichTextEditor from './RichTextEditor.tsx';
import { Button, Dialog, PanelResizeHandle } from './ui.tsx';
import { MobileModuleHeader, HeaderAction } from './MobileModuleHeader.tsx';
import { useCompactLayout } from '../hooks/useCompactLayout.ts';
import { applyAgendaWidth, applyCalendarRailWidth, beginAgendaResize, beginCalendarRailResize, readAgendaWidth, readCalendarRailWidth } from '../utils/panelWidth.ts';
import './calendar.css';
import type { StoreMessageRow, StoreState } from '../store/index.ts';
import { toAppError } from '../utils/errors.ts';
import { useDelayedFlag } from '../hooks/useDelayedFlag.ts';
import ChipInput, { chipAddress } from './ChipInput.tsx';
import type { ContactSuggestion } from './ChipInput.tsx';

/** The add/edit dialog's form state: the payload fields plus the dialog mode and event identity. */
type CalendarEventFormState = CalendarEventForm & {
  mode: 'create' | 'edit';
  id?: string;
  calendarId: string;
  attendees: string[];
};

/** A view event plus the form-facing string fields its index signature would hide. */
interface CalendarFormEvent extends CalendarViewEvent {
  description?: string | null;
  url?: string | null;
  organizer?: string | null;
  invite_account_id?: string | null;
  invite_hide_attendees?: boolean | null;
  source_message_id?: string | null;
  /** The user's own answer to an invitation (ACCEPTED / TENTATIVE / DECLINED / NEEDS-ACTION). */
  my_partstat?: string | null;
}

/** The identity of the event (or occurrence) a delete acts on. */
interface CalendarDeleteTarget {
  id: string;
  calendarId: string;
  recurrenceId?: string | null;
  [key: string]: unknown;
}

function isStoreMessageRow(message: GtdThread): message is GtdThread & StoreMessageRow {
  return typeof message.id === 'string' && typeof message.account_id === 'string';
}

const DATE_LOCALE_OVERRIDES: Record<string, string> = { zhCN: 'zh-CN' };

// The i18n instance always reports a language, so a definite tag goes in and comes out;
// it only needs the override lookup and the `_`→`-` normalisation.
function resolveDateLocale(language: string): string {
  return DATE_LOCALE_OVERRIDES[language] || language.replace('_', '-');
}

const emptyForm = (calendarId = '', date = new Date(), inviteAccountId = '') => ({ calendarId, summary: '', description: '', location: '', url: '', organizer: '', attendees: [], inviteAccountId, hideAttendees: false, allDay: false, startsAt: toDateTimeLocal(date), endsAt: toDateTimeLocal(new Date(date.getTime() + 3600000)) });
const AGENDA_HIDDEN_STORAGE_KEY = 'mailflow_calendar_agenda_hidden';
// Every rendered event — month-view card, all-day pill, timed block, and their move/resize
// ghost previews — blends slightly with what's behind it rather than sitting fully opaque.
const EVENT_OPACITY = 0.55;
// A week-view timed block is drawn see-through (see eventTint's `glass`), lighter still, with
// its text and left bar in the calendar color instead of white.
const EVENT_GLASS_OPACITY = 0.32;
// A real CSS `opacity` blends each element independently with whatever sits *directly* behind
// it — for one segment of a multi-day event that's a solid cell background, but the month
// grid draws its cell borders as a 1px gap with the border color showing through underneath
// (gap: 1, background: var(--border-subtle)), and a spanning event's bridging technique covers
// that gap with the *event's own* box, not the cell's. A translucent box over that gap lets
// the border color show through right at the seam between two days, drawing a visible line
// through what should read as one continuous bar. Pre-blending the color once with
// `color-mix()` instead — an opaque color that merely *looks* translucent — sidesteps this
// entirely: there's nothing left for a seam to show through, on any element this shades.
// `glass` mixes with real transparency instead — only for a week-view timed block, which never
// bridges a seam, so the hour lines and anything overlapped stay visible through it.
function eventTint(color: string | null | undefined, ratio: number = EVENT_OPACITY, glass = false): string {
  return `color-mix(in srgb, ${color || 'var(--accent)'} ${Math.round(ratio * 100)}%, ${glass ? 'transparent' : 'var(--bg-primary)'})`;
}
// Text on a light tint takes the event's own color, pulled toward the theme's text color so it
// stays readable in both light and dark themes.
function eventInk(color: string | null | undefined): string {
  return `color-mix(in srgb, ${color || 'var(--accent)'} 50%, var(--text-primary))`;
}
// The same seam risk applies to dimming an event mid-drag (the original, faded while its ghost
// preview shows the new position) — a real CSS `opacity` on a multi-day segment's wrapper (the
// one with the bridging negative margin) or a week-view all-day pill (which sits over the
// day-cell divider borders drawn *behind* it) lets whatever's underneath show through right at
// the seam, an "ugly black line" for the whole time it's being dragged. Diluting further with
// the same `color-mix()` approach — still opaque, just paler — dims it the same way without
// ever making anything underneath show through.
const EVENT_DIM_RATIO = 0.28;
const EVENT_GLASS_DIM_RATIO = 0.14;
// An invitation the user answered "maybe" to is drawn striped — the event's own tint
// alternating with a paler band — so a tentative slot reads as not-quite-committed.
// Still opaque (both bands are pre-blended like eventTint), for the same seam reason.
function eventFill(event: { calendar_color?: string | null; my_partstat?: unknown }, ratio: number = EVENT_OPACITY, glass = false): string {
  const base = eventTint(event.calendar_color, ratio, glass);
  if (event.my_partstat !== 'TENTATIVE') return base;
  return `repeating-linear-gradient(135deg, ${base} 0 6px, ${eventTint(event.calendar_color, ratio * 0.4, glass)} 6px 12px)`;
}
function iso(date: Date) { return date.toISOString(); }
// A drag/resize computes its new day boundaries as local-midnight Date objects (`new Date(y,
// m, d)`), so reading them back has to use the same local components — `date.toISOString()`
// converts to UTC first, which silently shifts the calendar date by one day for any positive
// UTC offset (dragging to "the 5th" would save as the 4th for a viewer east of UTC).
function dateOnly(date: Date) {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}
// The inverse of dateOnly: a "YYYY-MM-DD" key (as stored/compared throughout calendarView.ts)
// back to the local-midnight Date the rest of the grid represents days as.
function dayFromKey(key: string): Date {
  const [year, month, day] = key.split('-').map(Number);
  return new Date(year, (month || 1) - 1, day || 1);
}
// The days an event covers in month view, as local YYYY-MM-DD keys (end exclusive): an all-day
// event's own dates, or a timed event's local start day through the day its end falls in (an
// end at exactly midnight belongs to the day before). A timed event covering more than one day
// is laid out like a multi-day all-day bar: one lane per week, title once, start/end times.
function monthSpanKeys(event: CalendarViewEvent): { startKey: string; endKey: string } {
  if (event.all_day || event.allDay) {
    return { startKey: String(event.starts_at ?? event.startsAt ?? '').slice(0, 10), endKey: String(event.ends_at ?? event.endsAt ?? '').slice(0, 10) };
  }
  const start = new Date(String(event.starts_at ?? event.startsAt ?? ''));
  const end = new Date(String(event.ends_at ?? event.endsAt ?? ''));
  const last = new Date(Math.max(start.getTime(), end.getTime() - 1));
  return { startKey: dateOnly(start), endKey: dateOnly(new Date(last.getFullYear(), last.getMonth(), last.getDate() + 1)) };
}
// A month-view resize of a timed event: the new first day at its original start time, the new
// last day at its original end time (an end at midnight stays on the day after the last one).
// Null when that would end before it starts.
function resizedTimedRange(event: CalendarViewEvent, startDay: Date, endExclusive: Date): { start: Date; end: Date } | null {
  const start = new Date(String(event.starts_at ?? event.startsAt ?? ''));
  const end = new Date(String(event.ends_at ?? event.endsAt ?? ''));
  if (Number.isNaN(start.getTime()) || Number.isNaN(end.getTime())) return null;
  const endsAtMidnight = end.getHours() === 0 && end.getMinutes() === 0 && end.getSeconds() === 0 && end > start;
  const lastDay = new Date(endExclusive.getFullYear(), endExclusive.getMonth(), endExclusive.getDate() - (endsAtMidnight ? 0 : 1));
  const nextStart = new Date(startDay.getFullYear(), startDay.getMonth(), startDay.getDate(), start.getHours(), start.getMinutes(), start.getSeconds());
  const nextEnd = new Date(lastDay.getFullYear(), lastDay.getMonth(), lastDay.getDate(), end.getHours(), end.getMinutes(), end.getSeconds());
  return nextEnd > nextStart ? { start: nextStart, end: nextEnd } : null;
}
function monthSpanDays(event: CalendarViewEvent): number {
  if (event.all_day || event.allDay) return allDaySpanDays(event);
  const { startKey, endKey } = monthSpanKeys(event);
  const days = Math.round((dayFromKey(endKey).getTime() - dayFromKey(startKey).getTime()) / 86400000);
  return Number.isFinite(days) && days > 0 ? days : 1;
}
// All-day ends are stored exclusive (iCalendar DTEND) but shown inclusive, like every other
// calendar: an event on the 5th reads "5th – 5th", not "5th – 6th".
function shiftDateKey(key: unknown, days: number): string {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(String(key || ''))) return String(key || '');
  const day = dayFromKey(String(key));
  return dateOnly(new Date(day.getFullYear(), day.getMonth(), day.getDate() + days));
}
// Where a start/end instant range (end exclusive) lands in the visible `days` array, as
// grid-column-style indices — the first day-index at/after `start`, and the first at/after
// `end`. Used to place the all-day row's move/resize ghost preview.
function indexRangeInDays(days: Date[], start: Date, end: Date): { startIndex: number; endIndex: number } | null {
  let startIndex = days.findIndex(day => day.getTime() >= start.getTime());
  let endIndex = days.findIndex(day => day.getTime() >= end.getTime());
  if (startIndex === -1) return null; // starts after every visible day — nothing to show
  if (endIndex === -1) endIndex = days.length;
  return { startIndex, endIndex: Math.max(startIndex + 1, endIndex) };
}
function calendarDays(anchor: Date, weekStartsOn = 1) {
  const { start } = monthRange(anchor); const first = new Date(start); first.setDate(first.getDate() - ((first.getDay() - weekStartsOn + 7) % 7));
  return Array.from({ length: 42 }, (_, i) => { const day = new Date(first); day.setDate(first.getDate() + i); return day; });
}
function weekDays(anchor: Date, workWeek: boolean, weekStartsOn = 1, workDays: number[] = [1, 2, 3, 4, 5]) {
  const { start } = weekRange(anchor, weekStartsOn);
  if (!workWeek) return Array.from({ length: 7 }, (_, index) => { const day = new Date(start); day.setDate(day.getDate() + index); return day; });
  return [...workDays].sort((a: number, b: number) => ((a - weekStartsOn + 7) % 7) - ((b - weekStartsOn + 7) % 7)).map(dayOfWeek => { const day = new Date(start); day.setDate(day.getDate() + ((dayOfWeek - weekStartsOn + 7) % 7)); return day; });
}
function isToday(day: Date) { const today = new Date(); return day.toDateString() === today.toDateString(); }
function eventTime(event: { starts_at?: string | number | Date | null; [key: string]: unknown }) { return new Date(String(event.starts_at ?? '')).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }); }
function isWeekend(day: Date) { const weekday = day.getDay(); return weekday === 0 || weekday === 6; }
// "Now" marker for the time grid — read at render time (the line refreshes
// whenever the view re-renders; purely presentational, no timers).
function nowMinutes() { const now = new Date(); return now.getHours() * 60 + now.getMinutes(); }
// The portion of a `start`–`end` instant range that falls on `day` (a local-midnight Date),
// expressed as minutes-of-that-day — used by both the move-drag and resize-drag ghost previews
// to place their (possibly multi-day) preview box on each day column it touches.
function dayPreviewSegment(day: Date, start: Date, end: Date): { start: number; end: number } | null {
  const dayStart = day.getTime();
  const dayEnd = dayStart + 24 * 60 * 60000;
  if (end.getTime() <= dayStart || start.getTime() >= dayEnd) return null;
  return { start: (Math.max(start.getTime(), dayStart) - dayStart) / 60000, end: (Math.min(end.getTime(), dayEnd) - dayStart) / 60000 };
}

export default function CalendarPage({ isActive = true }) {
  const { t, i18n } = useTranslation();
  const locale = resolveDateLocale(i18n.resolvedLanguage || i18n.language);
  // Subscribe to the individual fields this page reads. Selecting the whole store
  // would re-render the calendar on every unrelated change (new mail, unread
  // counts, sidebar state, and so on) — the calendar is expensive to render, so
  // it must not be dragged along by mail activity.
  const accounts = useStore((state: StoreState) => state.accounts);
  const calendarWeekStartsOn = useStore((state: StoreState) => state.calendarWeekStartsOn);
  const calendarWorkDays = useStore((state: StoreState) => state.calendarWorkDays);
  const calendarWorkHoursStart = useStore((state: StoreState) => state.calendarWorkHoursStart);
  const calendarWorkHoursEnd = useStore((state: StoreState) => state.calendarWorkHoursEnd);
  // Sender preselected for invitations (Settings → Calendar).
  const calendarInviteAccountId = useStore((state: StoreState) => state.calendarInviteAccountId);
  const visibleCalendarIds = useStore((state: StoreState) => state.visibleCalendarIds);
  const setSelectedMessage = useStore((state: StoreState) => state.setSelectedMessage);
  const setThreadMessages = useStore((state: StoreState) => state.setThreadMessages);
  const setSelectedAccount = useStore((state: StoreState) => state.setSelectedAccount);
  const setShowCalendar = useStore((state: StoreState) => state.setShowCalendar);
  const setShowContacts = useStore((state: StoreState) => state.setShowContacts);
  const setVisibleCalendarIds = useStore((state: StoreState) => state.setVisibleCalendarIds);
  const isMobile = useMobile();
  const compactViewport = useCompactLayout();
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const railResizeRef = useRef<(() => void) | null>(null);
  const agendaResizeRef = useRef<(() => void) | null>(null);
  // The rail and the day agenda each keep their own persisted width, independent
  // of each other and of the mail/contacts list widths — resizing any one of
  // them never disturbs the others.
  const handleRailResizeMouseDown = useCallback((event: React.MouseEvent) => {
    railResizeRef.current?.();
    railResizeRef.current = beginCalendarRailResize(event, { edge: 'right' });
  }, []);
  const handleAgendaResizeMouseDown = useCallback((event: React.MouseEvent) => {
    agendaResizeRef.current?.();
    agendaResizeRef.current = beginAgendaResize(event, { edge: 'left' });
  }, []);
  // Publish the stored rail/agenda widths on mount so the first paint already
  // uses the user's values instead of the stylesheet defaults.
  useEffect(() => {
    applyCalendarRailWidth(readCalendarRailWidth());
    applyAgendaWidth(readAgendaWidth());
    return () => {
      railResizeRef.current?.();
      agendaResizeRef.current?.();
    };
  }, []);
  const [surfaceWidth, setSurfaceWidth] = useState(Infinity);
  const compact = compactViewport || surfaceWidth < 1100;
  useEffect(() => {
    const observer = new ResizeObserver(() => {
      setSurfaceWidth(surfaceRef.current?.clientWidth || Infinity);
    });
    const surface = surfaceRef.current;
    if (surface) observer.observe(surface);
    return () => observer.disconnect();
  }, []);
  const [dayPanelOpen, setDayPanelOpen] = useState(false);
  // The day-agenda column is a supplementary view, not primary content, so it's
  // hideable independently of the compact-layout collapse (which removes it
  // unconditionally on a narrow window) — this remembers a deliberate choice on
  // a normal-width one.
  const [agendaHidden, setAgendaHidden] = useState(() => {
    try { return localStorage.getItem(AGENDA_HIDDEN_STORAGE_KEY) === '1'; } catch { return false; }
  });
  const toggleAgendaHidden = () => setAgendaHidden(current => {
    const next = !current;
    try { localStorage.setItem(AGENDA_HIDDEN_STORAGE_KEY, next ? '1' : '0'); } catch { /* per-viewer convenience only */ }
    return next;
  });
  // Full-text search across every event, independent of the visible month/week/day range
  // (unlike `events`/`rawEvents` above, which only ever cover that range) — see
  // api.calendar.searchEvents. null = no active search, so the agenda pane keeps rendering
  // its normal date-based view; an empty array is a real zero-result search.
  const [searchQuery, setSearchQuery] = useState('');
  const [searchResults, setSearchResults] = useState<CalendarViewEvent[] | null>(null);
  const [searchError, setSearchError] = useState<string | null>(null);
  const searchAbortRef = useRef<AbortController | null>(null);
  const searchGeneration = useRef(0);
  const [anchor, setAnchor] = useState(() => new Date());
  const loadGeneration = useRef(0);
  // The last view is remembered per device, so leaving the calendar and coming back
  // (the page unmounts) or reloading the app keeps month/week/work-week/agenda.
  const [view, setViewState] = useState(readStoredCalendarView);
  const setView = useCallback((value: string) => setViewState(storeCalendarView(value)), []);
  const [rawCalendars, setCalendars] = useState<Array<{ id: string; name?: string | null; color?: string | null; [key: string]: unknown }>>([]); const [rawEvents, setEvents] = useState<CalendarViewEvent[]>([]);
  // display_visible is a persistent, server-side "turn this calendar off"
  // (currently only reachable from Settings → Calendar, for the contact-dates
  // calendar) — distinct from visibleCalendarIds below, which is a per-session,
  // per-browser show/hide toggle. A calendar disabled this way is dropped from
  // the sidebar and its events entirely, not just left unchecked and toggleable
  // back on from a control that was never meant to override the settings page.
  const calendars = useMemo(() => rawCalendars.filter(calendar => calendar.display_visible !== false).map(calendar => localizeContactCalendar(calendar, t)), [rawCalendars, t]);
  const hiddenCalendarIds = useMemo(() => new Set(rawCalendars.filter(calendar => calendar.display_visible === false).map(calendar => calendar.id)), [rawCalendars]);
  const events = useMemo(() => rawEvents.filter(event => typeof event.calendar_id !== 'string' || !hiddenCalendarIds.has(event.calendar_id)).map(event => localizeContactEvent(event, t)), [rawEvents, hiddenCalendarIds, t]);
  const [error, setError] = useState<string | null>(null); const [loading, setLoading] = useState(true); const [form, setForm] = useState<CalendarEventFormState | null>(null); const [saving, setSaving] = useState(false);
  const loadingNoticeVisible = useDelayedFlag(loading, 500);
  // Set when the server could not expand every series within its budget. The
  // events that are shown stay valid, but the view must say it is incomplete
  // rather than silently presenting a partial month as the whole truth.
  const [incompleteSeries, setIncompleteSeries] = useState(0);
  // One operation controller for the page's lifetime; the lazy state initializer
  // builds it once and keeps it across renders (the same object the ref held before).
  const [invitation] = useState(createInvitationOperationController);
  const [mobilePanelOpen, setMobilePanelOpen] = useState(false);
  const range = useMemo(() => calendarVisibleRange(anchor, view, calendarWeekStartsOn), [anchor, calendarWeekStartsOn, view]);
  const rangeStart = iso(range.start); const rangeEnd = iso(range.end);
  // Requests in flight for this page. Every new load aborts the previous one, and
  // unmounting aborts whatever is still running, so a superseded range cannot
  // keep fetching, and the backend stops expanding work nobody will display.
  const abortRef = useRef<AbortController | null>(null);
  // The selection is sent to the server so unselected series are not expanded at
  // all. `null` keeps the historical "every calendar" semantics.
  const selectionKey = visibleCalendarIds == null ? null : [...visibleCalendarIds].sort().join(',');
  // `silent`: a background refresh — no loading notice and no error banner for a transient
  // failure, so the visible calendar simply stays as it was.
  const load = useCallback(async ({ silent = false }: { silent?: boolean } = {}) => {
    const generation = ++loadGeneration.current;
    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    if (!silent) { setLoading(true); setError(null); }
    try {
      const calendarIds = selectionKey == null ? null : (selectionKey ? selectionKey.split(',') : []);
      const [calendarResult, eventResult] = await Promise.all([
        api.calendar.listCalendars({ signal: controller.signal }),
        api.calendar.listEvents(rangeStart, rangeEnd, { signal: controller.signal, calendarIds }),
      ]);
      if (generation === loadGeneration.current) {
        setCalendars(calendarResult.calendars || []);
        setEvents(eventResult.events || []);
        setIncompleteSeries(Array.isArray(eventResult.incompleteSeries) ? eventResult.incompleteSeries.length : 0);
      }
    } catch (err) {
      // A cancelled load is not a failure: a newer load (or an unmount) replaced
      // it, so it must not overwrite state or raise an error banner.
      if (!silent && !isAbortError(err) && generation === loadGeneration.current) { setError(toAppError(err).message || t('calendar.loadFailed')); setIncompleteSeries(0); }
    } finally { if (generation === loadGeneration.current) setLoading(false); }
  }, [rangeStart, rangeEnd, selectionKey, t]);
  useEffect(() => {
    load();
    // Abort the in-flight load on unmount (leaving the calendar module) so the
    // backend is not left expanding a window the user has already navigated away
    // from, and no stale response is applied to an unmounted tree.
    return () => { loadGeneration.current += 1; abortRef.current?.abort(); abortRef.current = null; };
  }, [load]);
  useEffect(() => {
    const refresh = () => load();
    window.addEventListener('inboxora:calendar-changed', refresh);
    return () => window.removeEventListener('inboxora:calendar-changed', refresh);
  }, [load]);
  // Pick up changes made elsewhere (other devices, CalDAV clients, synced subscriptions) on
  // the user's sync interval while the calendar is visible. Skipped while a pointer button is
  // held (a drag or resize in progress) or a dialog is open, so it never disturbs an edit.
  const syncInterval = useStore((state: StoreState) => state.syncInterval);
  const pointerHeld = useRef(false);
  const interactionOpen = useRef(false);
  useEffect(() => {
    const down = () => { pointerHeld.current = true; };
    const up = () => { pointerHeld.current = false; };
    window.addEventListener('pointerdown', down, true);
    window.addEventListener('pointerup', up, true);
    window.addEventListener('pointercancel', up, true);
    const id = setInterval(() => {
      if (document.visibilityState !== 'visible' || pointerHeld.current || interactionOpen.current) return;
      void load({ silent: true });
    }, Math.max(15, syncInterval || 15) * 1000);
    return () => {
      clearInterval(id);
      window.removeEventListener('pointerdown', down, true);
      window.removeEventListener('pointerup', up, true);
      window.removeEventListener('pointercancel', up, true);
    };
  }, [load, syncInterval]);
  // Debounced so every keystroke doesn't fire its own request — only the latest query still
  // being typed 300ms later actually goes out. `searchGeneration` (not just the abort) guards
  // the response too: an abort landing as a resolved-not-rejected promise (rare, but not ruled
  // out by every fetch polyfill) must still never overwrite a newer search's results.
  useEffect(() => {
    const trimmed = searchQuery.trim();
    if (!trimmed) {
      searchAbortRef.current?.abort();
      ++searchGeneration.current;
      setSearchResults(null);
      setSearchError(null);
      return;
    }
    // Results have nowhere else to land — reveal the agenda pane the same way the "+N more"
    // month-cell overflow already does (openDayAgenda), the instant a search begins rather
    // than waiting for the first response.
    if (compact) setDayPanelOpen(true); else setAgendaHidden(false);
    const generation = ++searchGeneration.current;
    const timer = setTimeout(async () => {
      searchAbortRef.current?.abort();
      const controller = new AbortController();
      searchAbortRef.current = controller;
      try {
        const result = await api.calendar.searchEvents(trimmed, { signal: controller.signal });
        if (generation !== searchGeneration.current) return;
        setSearchResults(Array.isArray(result?.events) ? result.events : []);
        setSearchError(null);
      } catch (err) {
        if (isAbortError(err) || generation !== searchGeneration.current) return;
        setSearchResults([]);
        setSearchError(toAppError(err).message || t('calendar.loadFailed'));
      }
    }, 300);
    return () => clearTimeout(timer);
  }, [searchQuery, compact, t]);
  const clearSearch = () => { setSearchQuery(''); searchAbortRef.current?.abort(); setSearchResults(null); setSearchError(null); };
  useEffect(() => {
    if (!isMobile || !isActive) return;
    invitation.reset();
    setForm(null);
    setMobilePanelOpen(false);
    setDayPanelOpen(false);
  }, [invitation, isActive, isMobile]);
  const writable = calendars.filter(calendar => !calendar.read_only && isWritableCalendarSource(calendar.source));
  const senderAccounts = accounts.filter(account => account.enabled && account.smtp_host);
  // The sender chosen in Settings → Calendar is preselected, or the only account that can send.
  // A default whose account can no longer send is ignored rather than carried as a dead value.
  const defaultInviteAccountId = senderAccounts.some(account => account.id === calendarInviteAccountId)
    ? calendarInviteAccountId
    : (senderAccounts.length === 1 ? senderAccounts[0].id : '');
  // Invitations come from the organizer: an event someone else organised (imported from their
  // invitation) keeps its attendees but never sends.
  const inviteLockedFor = (event: CalendarFormEvent) => {
    if (event.invite_account_id) return false;
    if (event.source_message_id) return true;
    const organizer = chipAddress(String(event.organizer || '')).replace(/^mailto:/i, '').toLowerCase();
    return Boolean(organizer) && !senderAccounts.some(account => String(account.email_address || '').toLowerCase() === organizer);
  };
  // Whether the form's own calendar is one local/writable-CalDAV write access
  // covers — the single source of truth EventDialog uses to disable every field.
  const formReadOnly = form ? !writable.some(calendar => calendar.id === form.calendarId) : false;
  // `endDate` is the last day the user dragged over, inclusive — a plain click or
  // double-click passes only `date`, unchanged from before. When the two differ,
  // the draft opens as an all-day event spanning the dragged days (the all-day
  // date range is exclusive at the end, so the stored end is one day past `endDate`).
  // `allDay` opens even a single day as an all-day draft (double-click on a month cell).
  const openCreate = (date = anchor, endDate = date, allDay = false) => {
    if (!writable.length) return;
    invitation.reset();
    setEditScope(null); setEditScopeOpen(false);
    const base = emptyForm(writable[0]?.id || '', date, defaultInviteAccountId);
    const draft = endDate.getTime() === date.getTime() && !allDay ? base : (() => {
      const exclusiveEnd = new Date(endDate.getFullYear(), endDate.getMonth(), endDate.getDate() + 1);
      const allDayTimes = toggleAllDayTimes({ ...base, startsAt: toDateTimeLocal(date), endsAt: toDateTimeLocal(exclusiveEnd) }, true);
      return { ...base, allDay: true, startsAt: String(allDayTimes.startsAt ?? base.startsAt), endsAt: String(allDayTimes.endsAt ?? base.endsAt) };
    })();
    setForm({ ...draft, mode: 'create' });
  };
  // A vertical drag inside a single hourly column (week/workweek view) creates a timed
  // event spanning exactly the dragged range — unlike `openCreate`, `end` here is always
  // a precise time, never promoted to an all-day span.
  const openTimedRangeCreate = (start: Date, end: Date) => {
    if (!writable.length) return;
    invitation.reset();
    setEditScope(null); setEditScopeOpen(false);
    const base = emptyForm(writable[0]?.id || '', start, defaultInviteAccountId);
    setForm({ ...base, endsAt: toDateTimeLocal(end), mode: 'create' });
  };
  const openEdit = (event: CalendarFormEvent) => { invitation.reset(); setEditScope(null); setEditScopeOpen(false); setForm({ mode: 'edit', ...event, id: event.series_id || event.id, recurrenceId: event.recurring ? event.recurrence_id : undefined, calendarId: event.calendar_id || '', summary: event.summary || '', description: event.description || '', location: event.location || '', url: event.url || '', organizer: event.organizer || '', attendees: Array.isArray(event.attendees) ? event.attendees : [], inviteAccountId: event.invite_account_id || defaultInviteAccountId, hideAttendees: Boolean(event.invite_hide_attendees), inviteLocked: inviteLockedFor(event), allDay: Boolean(event.all_day), startsAt: event.all_day ? String(event.starts_at).slice(0, 10) : toDateTimeLocal(event.starts_at), endsAt: event.all_day ? String(event.ends_at).slice(0, 10) : toDateTimeLocal(event.ends_at) }); };
  // Drag-to-reschedule writes the new start/end directly, without opening the edit dialog —
  // same underlying PATCH `save()` uses, built from the event's own current fields rather than
  // a live `form`. A moved recurring occurrence keeps its recurrenceId, same default as a plain
  // click (this occurrence only) — asking single-vs-series mid-drag would be worse UX than just
  // picking the same default the rest of the interface already uses for a bare click.
  // A moved/resized event keeps its original all-day-ness — a month-view move (day-only)
  // always carries one, and an hourly-grid move/resize never does, but both funnel through
  // here. `scope` mirrors save/delete's: 'single' keeps the occurrence's recurrenceId (an
  // exception on just that one), 'all' strips it (the request lands on the series itself).
  const commitMove = async (event: CalendarFormEvent, start: Date, end: Date, scope: 'single' | 'all') => {
    const id = event.series_id || event.id;
    if (!id) return;
    const allDay = Boolean(event.all_day);
    const payload = eventPayload({
      recurrenceId: scope === 'all' ? undefined : (event.recurring ? event.recurrence_id : undefined),
      calendarId: event.calendar_id || '',
      summary: event.summary || '',
      description: event.description || '',
      location: event.location || '',
      url: event.url || '',
      organizer: event.organizer || '',
      attendees: Array.isArray(event.attendees) ? event.attendees : [],
      // A moved invited event stays invited: its attendees receive the new time. Sending
      // without invitations here used to cancel every attendee's copy. An event never
      // invited from here stays uninvited.
      inviteLocked: !event.invite_account_id,
      inviteAccountId: event.invite_account_id || '',
      hideAttendees: Boolean(event.invite_hide_attendees),
      allDay,
      startsAt: allDay ? dateOnly(start) : toDateTimeLocal(start),
      endsAt: allDay ? dateOnly(end) : toDateTimeLocal(end),
    });
    if (!payload) return;
    try {
      await api.calendar.updateEvent(String(id), payload);
      await load();
    } catch (err) {
      setError(toAppError(err).message || t('calendar.saveFailed'));
    }
  };
  const [pendingMove, setPendingMove] = useState<{ event: CalendarFormEvent; start: Date; end: Date } | null>(null);
  // The drag hooks keep their preview/dimmed-original visible until `moveEvent`'s returned
  // promise settles, so a recurring drag's promise can't resolve the moment it's created — it
  // has to wait for whichever of choosePendingMoveScope/the dialog's cancel actually runs, so
  // the resolver they call is stashed here in the meantime.
  const pendingMoveResolveRef = useRef<(() => void) | null>(null);
  const resolvePendingMove = () => { pendingMoveResolveRef.current?.(); pendingMoveResolveRef.current = null; };
  // Drag-to-reschedule (move or resize) writes the new start/end directly, without opening the
  // edit dialog — but a series occurrence is still the same three-different-meanings problem
  // dragging doesn't get to skip past: asking which part of the series to reschedule is the
  // same scope choice save/delete already ask, just reached from a drag instead of a click.
  const moveEvent = (event: CalendarFormEvent, start: Date, end: Date): Promise<void> => {
    if (event.recurring && event.recurrence_id) {
      return new Promise<void>(resolve => {
        pendingMoveResolveRef.current = resolve;
        setPendingMove({ event, start, end });
      });
    }
    return commitMove(event, start, end, 'single');
  };
  const choosePendingMoveScope = (scope: 'single' | 'all') => {
    if (!pendingMove) return;
    const { event, start, end } = pendingMove;
    setPendingMove(null);
    void commitMove(event, start, end, scope).finally(resolvePendingMove);
  };
  // `scope` mirrors delete's: 'single' addresses just the occurrence being edited (the
  // payload keeps its recurrenceId), 'all' edits the series itself (recurrenceId stripped
  // so the request lands on the base event rather than its `/occurrence` endpoint).
  const save = async (scope?: 'single' | 'all') => {
    if (!form) return;
    const payload = eventPayload(form);
    if (!payload) { setError(t('calendar.invalidEvent')); return; }
    if (scope === 'all') delete payload.recurrenceId;
    setSaving(true); setError(null);
    try {
      // `api.calendar.updateEvent` takes a definite id while the retry controller may
      // pass `undefined`; `String(id)` serialises exactly as the API's own template does.
      const { result, retryable } = await invitation.save(form, payload, {
        createEvent: (data, key) => api.calendar.createEvent(data, key),
        updateEvent: (id, data, key) => api.calendar.updateEvent(String(id), data, key),
      });
      if (retryable) {
        const message = result?.invitationError || t('calendar.invitationPending', 'Invitation delivery is still pending; retry to check its status.');
        setForm(current => (current ? { ...current, invitationError: message } : current));
        setError(message);
      } else {
        setEditScope(null); setForm(null); await load();
      }
    } catch (err) {
      const message = toAppError(err).message || t('calendar.saveFailed');
      if (payload.sendInvites) setForm(current => (current ? { ...current, invitationError: message } : current));
      setError(message);
    } finally { setSaving(false); }
  };
  const [editScopeOpen, setEditScopeOpen] = useState(false);
  // Remembers the scope chosen for the event currently open, so a retry after a pending
  // invitation (which reuses this same handler) doesn't ask again.
  const [editScope, setEditScope] = useState<'single' | 'all' | null>(null);
  // A series occurrence is one of three different edits depending on scope — asking is the
  // only honest option here too, same as deleteEvent below.
  const requestSave = () => {
    if (form?.recurrenceId && !editScope) { setEditScopeOpen(true); return; }
    save(editScope ?? undefined);
  };
  const chooseEditScope = (scope: 'single' | 'all') => {
    setEditScope(scope);
    setEditScopeOpen(false);
    save(scope);
  };
  // `scope` decides the request shape: 'single' and 'following' address one occurrence, 'all'
  // removes the event outright — the path that also notifies invited attendees.
  const performDelete = async (target: CalendarDeleteTarget, scope: string) => {
    try {
      await api.calendar.deleteEvent(
        target.id,
        target.calendarId,
        scope === 'all' ? undefined : target.recurrenceId,
        scope === 'following' ? 'following' : undefined,
      );
      invitation.reset();
      setDeleteTarget(null);
      setForm(null);
      await load();
    } catch (err) {
      setError(toAppError(err).message || t('calendar.deleteFailed'));
      setDeleteTarget(null);
    }
  };
  const remove = async () => {
    if (!form?.id) return;
    const target = { id: form.id, calendarId: form.calendarId, recurrenceId: form.recurrenceId };
    // An event opened from a series is one occurrence, so which of the three things to remove is
    // the user's choice rather than something to assume.
    if (form.recurrenceId) { setDeleteTarget(target); return; }
    if (!window.confirm(t('calendar.confirmDelete'))) return;
    setSaving(true);
    try { await performDelete(target, 'all'); } finally { setSaving(false); }
  };
  // Changing the answer to an invitation already in the calendar — the same reply the mail's
  // invitation card sends. Declining removes the local copy, so the dialog closes either way.
  const respondToInvitation = async (response: InvitationAnswer, notify: boolean) => {
    if (!form || typeof form.source_message_id !== 'string') return;
    setSaving(true); setError(null);
    try {
      await api.calendar.respondInvitation(form.source_message_id, response, form.calendarId || undefined, notify);
      setForm(null);
      await load();
    } catch (err) {
      setError(toAppError(err).message || t('calendar.saveFailed'));
    } finally { setSaving(false); }
  };
  const changeForm = (key: string, value: unknown) => { invitation.reset(); setForm(current => (current ? { ...current, [key]: value, invitationError: null } : current)); };
  const deleteEvent = async (event: CalendarViewEvent) => {
    const id = event.series_id || event.id;
    if (!id) return;
    const target: CalendarDeleteTarget = { id, calendarId: event.calendar_id || '', recurrenceId: event.recurrence_id };
    // A series can be removed from here on, entirely, or just at this occurrence. Asking is the
    // only honest option: the three answers produce three different calendars.
    if (event.recurring && event.recurrence_id) { setDeleteTarget(target); return; }
    if (!window.confirm(t('calendar.confirmDelete'))) return;
    await performDelete(target, 'all');
  };
  const [deleteTarget, setDeleteTarget] = useState<CalendarDeleteTarget | null>(null);
  // Attendees' answers for the event open in the dialog (from its iCalendar ATTENDEE lines).
  const [attendeeStatuses, setAttendeeStatuses] = useState<Map<string, string> | undefined>(undefined);
  const statusEventId = form?.mode === 'edit' && Array.isArray(form.attendees) && form.attendees.length ? String(form.id || '') : '';
  useEffect(() => {
    setAttendeeStatuses(undefined);
    if (!statusEventId) return undefined;
    let current = true;
    api.calendar.attendeeStatuses(statusEventId)
      .then((result: { attendees?: Array<{ email: string; partstat: string }> }) => {
        if (current) setAttendeeStatuses(new Map((result.attendees || []).map(attendee => [attendee.email.toLowerCase(), attendee.partstat])));
      })
      .catch(() => {});
    return () => { current = false; };
  }, [statusEventId]);
  // Read by the background refresh above: never reload under an open dialog or a save.
  interactionOpen.current = Boolean(form || editScopeOpen || deleteTarget || saving);
  const [contextMenu, setContextMenu] = useState<{ event: CalendarViewEvent; x: number; y: number; triggerRef: { current: unknown } } | null>(null);
  const days = view === 'month' ? calendarDays(anchor, calendarWeekStartsOn) : weekDays(anchor, view === 'workweek', calendarWeekStartsOn, calendarWorkDays);
  // The user's own answer to an invitation, found by matching any of their account addresses
  // among the event's attendee answers — drives the tentative stripes and the dialog's reply row.
  const ownAddresses = new Set(accounts.map(account => String(account.email_address || '').toLowerCase()).filter(Boolean));
  const visibleEvents = (visibleCalendarIds == null ? events : events.filter(event => typeof event.calendar_id === 'string' && visibleCalendarIds.includes(event.calendar_id)))
    .map(event => withOwnReply(event, ownAddresses));
  // One parse-and-bucket pass per event list, reused by every day cell and every
  // render, instead of re-filtering and re-sorting the whole array per day.
  const dayEventsFor = useMemo(() => createDayEventsResolver(visibleEvents), [visibleEvents]);
  const toggleCalendar = (id: string) => {
    const current = visibleCalendarIds == null ? calendars.map((calendar: { id: string }) => calendar.id) : visibleCalendarIds;
    setVisibleCalendarIds(current.includes(id) ? current.filter(value => value !== id) : [...current, id]);
  };
  const title = view === 'month' || view === 'agenda'
    ? anchor.toLocaleDateString(locale, { month: 'long', year: 'numeric' })
    : `${days[0].toLocaleDateString(locale, { month: 'short', day: 'numeric' })} – ${days[days.length - 1].toLocaleDateString(locale, { month: 'short', day: 'numeric', year: 'numeric' })}`;
  const step = (direction: number) => setAnchor(current => shiftCalendarAnchor(current, view, direction));
  const shiftMiniMonth = (direction: number) => setAnchor(current => shiftCalendarAnchor(current, 'month', direction));
  // The message an invitation was accepted from may sit in another account or folder
  // and is therefore not in the loaded list: fetching it by id and publishing it as a
  // one-message thread is what keeps the reader from opening blank.
  const openSourceMessage = async () => {
    const messageId = typeof form?.source_message_id === 'string' ? form.source_message_id : null;
    if (!messageId) return;
    const sourceAccountId = typeof form?.source_account_id === 'string' ? form.source_account_id : null;
    const sourceFolder = typeof form?.source_folder === 'string' ? form.source_folder : null;
    setForm(null);
    setShowCalendar(false);
    setShowContacts(false);
    if (sourceAccountId) setSelectedAccount(sourceAccountId, sourceFolder || 'INBOX');
    await openDeepLinkMessage(messageId, {
      getMessage: async (id) => {
        const message = await api.getMessage(id);
        return message && isStoreMessageRow(message) ? message : null;
      },
      setThreadMessages: (key, messages) => {
        const storeMessages: StoreMessageRow[] = [];
        for (const message of messages) {
          if (!isStoreMessageRow(message)) throw new Error('Deep link returned a non-store message');
          storeMessages.push(message);
        }
        setThreadMessages(key, storeMessages);
      },
      setSelectedMessage,
    });
  };
  const selectDay = (day: Date) => { setAnchor(day); if (compact) setDayPanelOpen(true); };
  // "+N more" on a month-view cell — selects the day exactly like clicking its date chip does,
  // plus (desktop only; compact already opens its own day-panel dialog via selectDay above)
  // reveals the side agenda rail if the viewer had hidden it, since that's what actually shows
  // every event on that day past the per-cell cap.
  const openDayAgenda = (day: Date) => { selectDay(day); if (!compact && agendaHidden) setAgendaHidden(false); };
  const sidebarProps = { anchor, calendars, visibleCalendarIds, weekStartsOn: calendarWeekStartsOn, locale,
    onSelectDate: setAnchor, onShiftMonth: shiftMiniMonth, onToggleCalendar: toggleCalendar,
    onCalendarsChanged: load, onCreate: () => openCreate(), canCreate: writable.length > 0, t };
  const agendaProps = { events: visibleEvents, anchor, locale, onOpen: openEdit, t, searchQuery: searchQuery.trim(), searchResults, searchError, onClearSearch: clearSearch };
  return <div ref={surfaceRef} data-testid="calendar-page" className={`calendar-page calendar-v3${compact ? ' calendar-compact' : ''}${isMobile ? ' calendar-mobile' : ''}`}>
    {isActive && <MobileFloatingAction label={t('calendar.newEvent')} onClick={() => openCreate()} disabled={!writable.length || Boolean(form)} />}
    {isMobile && <MobileModuleHeader title={t('calendar.title')} subtitle={title}>
      <HeaderAction icon="calendars" label={t('calendar.calendars')} data-testid="calendar-mobile-panel" onClick={() => setMobilePanelOpen(true)} />
      <HeaderAction icon="agenda" label={t('calendar.dayAgenda')} data-testid="calendar-open-day" onClick={() => setDayPanelOpen(true)} />
      <HeaderAction icon="add" label={t('calendar.newEvent')} data-testid="calendar-header-new" disabled={!writable.length || Boolean(form)} onClick={() => openCreate()} />
    </MobileModuleHeader>}
    {!isMobile && <CalendarSidebar {...sidebarProps} />}
    {!isMobile && <PanelResizeHandle testId="calendar-rail-resize" onMouseDown={handleRailResizeMouseDown} />}
    <main className="calendar-main">
      <header className="calendar-header">
        {!isMobile && <h1>{title}</h1>}
        <div className="calendar-toolbar">
          {!isMobile && <div className="calendar-search">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" aria-hidden="true"><circle cx="11" cy="11" r="8" /><path d="m21 21-4.4-4.4" /></svg>
            <input type="search" data-testid="calendar-search-input" value={searchQuery} onChange={event => setSearchQuery(event.target.value)} aria-label={t('calendar.searchEvents')} placeholder={t('calendar.searchEvents')} />
            {searchQuery && <button type="button" data-testid="calendar-search-clear" aria-label={t('calendar.clearSearch')} title={t('calendar.clearSearch')} onClick={clearSearch}>
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true"><path d="M18 6 6 18M6 6l12 12" /></svg>
            </button>}
          </div>}
          {isMobile ? <select data-testid="calendar-view-select" aria-label={t('calendar.view')} value={view} onChange={event => setView(event.target.value)} className="calendar-view-select">
            {['month', 'week', 'workweek', 'agenda'].map(value => <option key={value} value={value}>{t(value === 'workweek' ? 'calendar.workWeek' : `calendar.${value}`)}</option>)}
          </select> : <div role="group" className="calendar-segments" aria-label={t('calendar.view')}>
            {[['month', t('calendar.month')], ['week', t('calendar.week')], ['workweek', t('calendar.workWeek')], ['agenda', t('calendar.agenda')]].map(([value, label]) => <button type="button" key={value} data-testid={`calendar-view-${value}`} onClick={() => setView(value)} aria-pressed={view === value}>{label}</button>)}
          </div>}
          <div className="calendar-date-controls">
            <Button variant="ghost" onClick={() => step(-1)} aria-label={t('calendar.previous')}>‹</Button>
            <Button variant="ghost" className="calendar-today-btn" onClick={() => setAnchor(new Date())}>{t('calendar.today')}</Button>
            <Button variant="ghost" onClick={() => step(1)} aria-label={t('calendar.next')}>›</Button>
            {compact && !isMobile && <Button data-testid="calendar-open-day" onClick={() => setDayPanelOpen(true)}>{t('calendar.dayAgenda')}</Button>}
            {!compact && !isMobile && <Button variant="ghost" data-testid="calendar-toggle-agenda" aria-pressed={!agendaHidden} aria-label={t(agendaHidden ? 'calendar.showAgenda' : 'calendar.hideAgenda')} title={t(agendaHidden ? 'calendar.showAgenda' : 'calendar.hideAgenda')} onClick={toggleAgendaHidden}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" aria-hidden="true">
                <rect x="3" y="4" width="18" height="16" rx="2" />
                <path d="M15 4v16" />
                {!agendaHidden && <path d="M15 4h4a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-4Z" fill="currentColor" stroke="none" />}
              </svg>
            </Button>}
          </div>
        </div>
      </header>
      {error && !form && <div role="alert" className="ui-alert">{error}<Button variant="ghost" onClick={() => { void load(); }}>{t('calendar.retry')}</Button></div>}
      {incompleteSeries > 0 && <div role="status" data-testid="calendar-incomplete" className="calendar-notice">{t('calendar.incompleteSeries', { n: incompleteSeries })}</div>}
      {!writable.length && !loading && <div className="calendar-notice">{t('calendar.noWritable')}</div>}
      {/* Only for requests slower than 500 ms; a quick fetch or post-save reload never flashes it. */}
      {loadingNoticeVisible && <div role="status" className="calendar-notice">{t('calendar.loading')}</div>}
      <div className="calendar-body" aria-busy={loading}>
        {view === 'agenda' ? <CalendarAgenda {...agendaProps} monthly /> : <CalendarGrid days={days} dayEventsFor={dayEventsFor} view={view} anchor={anchor} isMobile={isMobile} locale={locale} onSelectDay={selectDay} onOpenDayAgenda={openDayAgenda} openCreate={openCreate} openTimedRangeCreate={openTimedRangeCreate} moveEvent={moveEvent} openEdit={openEdit} openContextMenu={(event, x, y, trigger) => setContextMenu({ event, x, y, triggerRef: { current: trigger } })} t={t} calendarWorkHoursStart={calendarWorkHoursStart} calendarWorkHoursEnd={calendarWorkHoursEnd} />}
      </div>
    </main>
    {!compact && !agendaHidden && <PanelResizeHandle testId="calendar-agenda-resize" onMouseDown={handleAgendaResizeMouseDown} />}
    {!compact && !agendaHidden && <aside className="calendar-agenda" aria-label={t('calendar.dayAgenda')}><CalendarAgenda {...agendaProps} /></aside>}
    {/* Narrow screens show both calendar panels as the same bottom sheet the
        contact and mail lists use, instead of two differently placed drawers. */}
    {compact && dayPanelOpen && <Dialog title={t('calendar.dayAgenda')} closeLabel={t('calendar.close')} onClose={() => setDayPanelOpen(false)} testId="calendar-day-sheet" className="calendar-day-dialog ui-sheet"><CalendarAgenda {...agendaProps} /></Dialog>}
    {isMobile && mobilePanelOpen && <Dialog title={t('calendar.panel')} closeLabel={t('calendar.close')} onClose={() => setMobilePanelOpen(false)} testId="calendar-mobile-dock" className="calendar-panel-dialog ui-sheet">
      <CalendarSidebar {...sidebarProps} onSelectDate={day => { setAnchor(day); setMobilePanelOpen(false); }} />
    </Dialog>}
    {form && <EventDialog attendeeStatuses={attendeeStatuses} ownAddresses={ownAddresses} form={form} error={error} calendars={formReadOnly ? calendars : writable} accounts={senderAccounts} saving={saving} readOnly={formReadOnly} onOpenSourceMessage={openSourceMessage} fullScreen={isMobile} onChange={changeForm} onAllDayChange={allDay => { invitation.reset(); setForm(current => current && { ...toggleAllDayTimes(current, allDay), mode: current.mode, calendarId: current.calendarId, attendees: current.attendees }); }} onSave={requestSave} onDelete={remove} onRespond={respondToInvitation} onClose={() => { invitation.reset(); setEditScope(null); setEditScopeOpen(false); setForm(null); setError(null); }} t={t} />}
    {editScopeOpen && <CalendarEditScopeDialog busy={saving} onSelect={chooseEditScope} onClose={() => setEditScopeOpen(false)} t={t} />}
    {pendingMove && <CalendarEditScopeDialog onSelect={choosePendingMoveScope} onClose={() => { setPendingMove(null); resolvePendingMove(); }} t={t} />}
    {contextMenu && <CalendarContextMenu {...contextMenu} isMobile={isMobile} onEdit={() => openEdit(contextMenu.event)} onDelete={() => deleteEvent(contextMenu.event)} onClose={() => setContextMenu(null)} t={t} />}
    {deleteTarget && <CalendarDeleteScopeDialog event={deleteTarget} busy={saving} onSelect={scope => performDelete(deleteTarget, scope)} onClose={() => setDeleteTarget(null)} t={t} />}
  </div>;
}

/** Shared props for the month and time grids. */
interface CalendarGridProps {
  days: Date[];
  dayEventsFor: (day: Date) => CalendarViewEvent[];
  view: string;
  anchor: Date;
  isMobile: boolean;
  locale: string;
  onSelectDay: (day: Date) => void;
  // Only used by the month grid's "+N more" chip — TimeGrid has no equivalent cap to open from.
  onOpenDayAgenda?: (day: Date) => void;
  openCreate: (date?: Date, endDate?: Date, allDay?: boolean) => void;
  openTimedRangeCreate: (start: Date, end: Date) => void;
  moveEvent: (event: CalendarFormEvent, start: Date, end: Date) => void | Promise<void>;
  openEdit: (event: CalendarViewEvent) => void;
  openContextMenu: (event: CalendarViewEvent, x: number, y: number, trigger?: unknown) => void;
  t: TFunction;
  calendarWorkHoursStart: string;
  calendarWorkHoursEnd: string;
}

/** The add/edit event dialog. */
type InvitationAnswer = 'ACCEPTED' | 'TENTATIVE' | 'DECLINED';

interface EventDialogProps {
  form: CalendarEventFormState;
  /** The user's own addresses (lower case), to find their entry among the attendees. */
  ownAddresses: Set<string>;
  error: string | null;
  calendars: Array<{ id: string; name?: string | null; color?: string | null; [key: string]: unknown }>;
  accounts: Array<{ id: string; email_address?: string | null; name?: string | null; [key: string]: unknown }>;
  saving: boolean;
  // A form opened on an event whose calendar isn't writable: every field is
  // disabled and Save/Delete are replaced with just Close — one dialog for
  // viewing and editing instead of a separate read-only preview.
  readOnly: boolean;
  onOpenSourceMessage?: () => void;
  onRespond?: (response: InvitationAnswer, notify: boolean) => void;
  onChange: (field: string, value: unknown) => void;
  onAllDayChange: (allDay: boolean) => void;
  onSave: () => void;
  onDelete: () => void;
  onClose: () => void;
  t: TFunction;
  fullScreen?: boolean;
  /** Each invited address's answer (ACCEPTED / TENTATIVE / DECLINED / NEEDS-ACTION). */
  attendeeStatuses?: Map<string, string>;
}

// Address suggestions for attendees: contacts and people mailed with (same source as the composer).
async function suggestAttendees(query: string): Promise<ContactSuggestion[]> {
  try { return (await api.suggestContacts(query)).contacts || []; } catch { return []; }
}

const ATTENDEE_STATUS_STYLE: Record<string, { color: string; symbol: string }> = {
  ACCEPTED: { color: 'var(--green)', symbol: '✓' },
  TENTATIVE: { color: 'var(--amber)', symbol: '?' },
  DECLINED: { color: 'var(--red)', symbol: '✕' },
  'NEEDS-ACTION': { color: 'var(--text-tertiary)', symbol: '…' },
};

function AttendeeStatus({ status, t }: { status?: string; t: TFunction }) {
  if (!status) return null;
  const style = ATTENDEE_STATUS_STYLE[status] || ATTENDEE_STATUS_STYLE['NEEDS-ACTION'];
  const label = status === 'ACCEPTED' ? t('calendar.attendeeAccepted')
    : status === 'TENTATIVE' ? t('calendar.attendeeTentative')
      : status === 'DECLINED' ? t('calendar.attendeeDeclined')
        : t('calendar.attendeePending');
  return <span data-testid="calendar-attendee-status" data-status={status} title={label} aria-label={label} style={{ color: style.color, fontWeight: 600, marginLeft: 2 }}>{style.symbol}</span>;
}
// Click-drag over several day cells (month grid) or several day columns in the
// all-day row (week grid) to create an event spanning exactly the days dragged
// over. A plain click or double-click is unaffected: `onComplete` only fires once
// the pointer has actually crossed into a different day before release, so a
// same-cell mousedown+mouseup keeps producing the native click the existing
// onClick/onDoubleClick handlers already act on.
function useDayRangeDrag(onComplete: (start: Date, end: Date) => void) {
  const [range, setRange] = useState<{ start: Date; end: Date } | null>(null);
  // Mirrors `range`, read synchronously in `finish` — calling `onComplete` (which sets state on
  // a *different* component, several levels up) from inside a `setRange` updater is unreliable:
  // React treats updaters as pure functions of previous state, and a setState triggered from
  // within one can be silently dropped or reordered instead of always both clearing the
  // highlight and firing the callback. Reading the pending range from a ref and calling
  // `setRange(null)` and `onComplete` as two separate, ordinary statements avoids that.
  const rangeRef = useRef<{ start: Date; end: Date } | null>(null);
  const draggingRef = useRef(false);
  const movedRef = useRef(false);
  const onCompleteRef = useRef(onComplete);
  onCompleteRef.current = onComplete;
  useEffect(() => {
    const finish = () => {
      const pending = draggingRef.current && movedRef.current ? rangeRef.current : null;
      draggingRef.current = false;
      movedRef.current = false;
      rangeRef.current = null;
      setRange(null);
      if (pending) {
        const [start, end] = pending.start <= pending.end ? [pending.start, pending.end] : [pending.end, pending.start];
        onCompleteRef.current(start, end);
      }
    };
    // Neither `mouseup` nor `blur` fires for every way a drag can end — a browser context
    // menu, an OS-level interrupt, or a native modal stealing the mouseup can all swallow it
    // silently, leaving `draggingRef` stuck true with nothing left to ever call `finish`. The
    // self-correcting fallback: any `mousemove` where the mouse button is no longer actually
    // held (`event.buttons === 0`) means the drag has already ended one way or another, so
    // treat it as the release we missed. This is what actually explained the highlight
    // occasionally surviving a cancelled drag — not a bug in clearing the state, but nothing
    // ever being left to trigger the clear.
    const onMouseMove = (event: MouseEvent) => {
      if (draggingRef.current && event.buttons === 0) finish();
    };
    document.addEventListener('mouseup', finish);
    document.addEventListener('mousemove', onMouseMove);
    window.addEventListener('blur', finish);
    return () => {
      document.removeEventListener('mouseup', finish);
      document.removeEventListener('mousemove', onMouseMove);
      window.removeEventListener('blur', finish);
    };
  }, []);
  const startDrag = (day: Date) => {
    draggingRef.current = true; movedRef.current = false;
    // Recorded in the ref only, not published to state — a plain click (mousedown+mouseup
    // with no movement in between, which is what most clicks and every double-click are) never
    // moves anywhere, so there's nothing to highlight; publishing a same-cell range here just
    // to immediately clear it on mouseup risked a visible flash that didn't always clear
    // cleanly. `extendDrag` is the first point that publishes, once a real drag is underway.
    rangeRef.current = { start: day, end: day };
  };
  const extendDrag = (day: Date) => {
    if (!draggingRef.current) return;
    movedRef.current = true;
    rangeRef.current = rangeRef.current ? { start: rangeRef.current.start, end: day } : { start: day, end: day };
    setRange(rangeRef.current);
  };
  const inRange = (day: Date) => {
    if (!range) return false;
    const [start, end] = range.start <= range.end ? [range.start, range.end] : [range.end, range.start];
    return day >= start && day <= end;
  };
  // A hard clear with no completion — used when the visible period itself changes (navigating
  // month/week), since any drag in progress against the *old* period no longer means anything
  // against the new one, and letting stale state carry across a navigation is never correct
  // regardless of whatever else might also be keeping it from clearing on its own.
  const reset = () => {
    draggingRef.current = false;
    movedRef.current = false;
    rangeRef.current = null;
    setRange(null);
  };
  return { startDrag, extendDrag, inRange, dragging: range !== null, reset };
}

// Click-drag inside the hourly grid (week/workweek view) creates a timed event at exact
// clock times, snapped to 15-minute steps — vertically within one day column ("05:00–06:30"),
// or across day columns for a multi-day timed span ("Jan 1 05:15 – Jan 2 04:30", each end
// keeping its own dragged time rather than being forced onto the same time of day). Both axes
// track independently and combine: minutes-of-day come from the pointer's Y position against
// the anchor column's bounding rect (every column in the row shares the same top, so any one
// column's rect works regardless of which the pointer ends up over), and the day comes from
// whichever column's onMouseEnter last fired. `onComplete` receives the two exact instants
// directly, so it plugs straight into `openTimedRangeCreate`. Same as `useDayRangeDrag`: a
// plain click (no movement on either axis) never fires `onComplete`, and a `blur` on the
// window is a fallback for a mouseup that lands outside the browser entirely, so the drag
// highlight can never get stuck on regardless of how the drag ends.
function useHourGridDrag(onComplete: (start: Date, end: Date) => void, topPadding: number, stepMinutes = 15) {
  const [drag, setDrag] = useState<{ anchorDay: Date; anchorMinutes: number; day: Date; minutes: number } | null>(null);
  // Mirrors `drag`, read synchronously in `finish` — see useDayRangeDrag's rangeRef for why
  // `onComplete` must never be called from inside a `setDrag` updater.
  const dragRef = useRef<{ anchorDay: Date; anchorMinutes: number; day: Date; minutes: number } | null>(null);
  const draggingRef = useRef(false);
  const movedRef = useRef(false);
  const rectRef = useRef<DOMRect | null>(null);
  const onCompleteRef = useRef(onComplete);
  onCompleteRef.current = onComplete;
  const minutesAt = useCallback((clientY: number) => {
    const rect = rectRef.current;
    if (!rect) return 0;
    const raw = clientY - rect.top - topPadding;
    return Math.min(1440, Math.max(0, Math.floor(raw / stepMinutes) * stepMinutes));
  }, [topPadding, stepMinutes]);
  const commit = useCallback((next: typeof drag) => { dragRef.current = next; setDrag(next); }, []);
  useEffect(() => {
    const onMouseMove = (event: MouseEvent) => {
      if (!draggingRef.current || !dragRef.current) return;
      // Neither `mouseup` nor `blur` fires for every way a drag can end (a context menu or
      // native modal can swallow the mouseup silently) — self-correct on the next movement
      // once the button is no longer actually held, same reasoning as useDayRangeDrag.
      if (event.buttons === 0) { finish(); return; }
      const minutes = minutesAt(event.clientY);
      if (minutes === dragRef.current.minutes) return;
      movedRef.current = true;
      commit({ ...dragRef.current, minutes });
    };
    const finish = () => {
      const pending = draggingRef.current && movedRef.current ? dragRef.current : null;
      draggingRef.current = false;
      movedRef.current = false;
      rectRef.current = null;
      commit(null);
      if (pending) {
        const anchorInstant = pending.anchorDay.getTime() + pending.anchorMinutes * 60000;
        const currentInstant = pending.day.getTime() + pending.minutes * 60000;
        const startInstant = Math.min(anchorInstant, currentInstant);
        const endInstant = Math.max(anchorInstant, currentInstant) + stepMinutes * 60000;
        onCompleteRef.current(new Date(startInstant), new Date(endInstant));
      }
    };
    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', finish);
    window.addEventListener('blur', finish);
    return () => {
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', finish);
      window.removeEventListener('blur', finish);
    };
  }, [minutesAt, stepMinutes, commit]);
  const startDrag = (day: Date, event: React.MouseEvent<HTMLDivElement>) => {
    rectRef.current = event.currentTarget.getBoundingClientRect();
    draggingRef.current = true;
    movedRef.current = false;
    const minutes = minutesAt(event.clientY);
    commit({ anchorDay: day, anchorMinutes: minutes, day, minutes });
  };
  const enterDay = (day: Date) => {
    if (!draggingRef.current || !dragRef.current || dragRef.current.day.toDateString() === day.toDateString()) return;
    movedRef.current = true;
    commit({ ...dragRef.current, day });
  };
  // The highlighted segment of `day`, in minutes-of-that-day — spanning its full height for a
  // day strictly between the drag's two ends, and just the dragged portion on the two end days.
  const rangeFor = (day: Date) => {
    if (!drag) return null;
    const anchorInstant = drag.anchorDay.getTime() + drag.anchorMinutes * 60000;
    const currentInstant = drag.day.getTime() + drag.minutes * 60000;
    const startInstant = Math.min(anchorInstant, currentInstant);
    const endInstant = Math.max(anchorInstant, currentInstant) + stepMinutes * 60000;
    const dayStart = day.getTime();
    const dayEnd = dayStart + 24 * 60 * 60000;
    if (endInstant <= dayStart || startInstant >= dayEnd) return null;
    return { start: (Math.max(startInstant, dayStart) - dayStart) / 60000, end: (Math.min(endInstant, dayEnd) - dayStart) / 60000 };
  };
  // A hard clear with no completion — see useDayRangeDrag's reset for why.
  const reset = () => {
    draggingRef.current = false;
    movedRef.current = false;
    rectRef.current = null;
    commit(null);
  };
  return { startDrag, enterDay, rangeFor, reset };
}

// Dragging an existing event moves it — vertically in 15-minute steps (grabbed anywhere on the
// event, the whole event shifts by the same snapped delta, it doesn't jump to sit under the
// pointer), horizontally in whole days. Both deltas are tracked from the same drag and applied
// together, same as the create-drag above. `onComplete` fires only once the event has actually
// moved on some axis, so a plain click still reaches the event's own onClick (open edit)
// untouched — the caller must call `event.stopPropagation()` in its own onMouseDown so this
// doesn't also start the day/hour column's create-drag underneath it.
//
// `vertical: false` (month view, which has no time-of-day axis) skips the mousemove-driven
// minutes tracking entirely — only `enterDay` (day-to-day, at whatever granularity the caller's
// cells are) drives the move, preserving each event's original time-of-day exactly.
function useEventDragMove(onComplete: (event: CalendarFormEvent, start: Date, end: Date) => void | Promise<void>, options: { vertical?: boolean } = {}) {
  const vertical = options.vertical !== false;
  const [moving, setMoving] = useState<{ event: CalendarFormEvent; deltaDays: number; deltaMinutes: number } | null>(null);
  // Mirrors `moving`, read synchronously in `finish` — see useDayRangeDrag's rangeRef for why
  // `onComplete` must never be called from inside a `setMoving` updater.
  const movingRef = useRef<{ event: CalendarFormEvent; deltaDays: number; deltaMinutes: number } | null>(null);
  const draggingRef = useRef(false);
  const movedRef = useRef(false);
  const anchorClientYRef = useRef(0);
  const anchorDayRef = useRef<Date | null>(null);
  const onCompleteRef = useRef(onComplete);
  onCompleteRef.current = onComplete;
  const commit = useCallback((next: typeof moving) => { movingRef.current = next; setMoving(next); }, []);
  useEffect(() => {
    const onMouseMove = (event: MouseEvent) => {
      if (!draggingRef.current || !movingRef.current) return;
      // Neither `mouseup` nor `blur` fires for every way a drag can end (a context menu or
      // native modal can swallow the mouseup silently) — self-correct on the next movement
      // once the button is no longer actually held, same reasoning as useDayRangeDrag. This
      // matters even more here than elsewhere: with `vertical: false` (month view), this
      // handler is otherwise a no-op, so without this check nothing at all would ever notice
      // a swallowed mouseup.
      if (event.buttons === 0) { finish(); return; }
      if (!vertical) return;
      const deltaMinutes = Math.round((event.clientY - anchorClientYRef.current) / 15) * 15;
      if (deltaMinutes === movingRef.current.deltaMinutes) return;
      movedRef.current = true;
      commit({ ...movingRef.current, deltaMinutes });
    };
    const finish = () => {
      const pending = draggingRef.current && movedRef.current ? movingRef.current : null;
      draggingRef.current = false;
      movedRef.current = false;
      anchorDayRef.current = null;
      if (!pending) { commit(null); return; }
      // Keeps `moving` set — the preview and the dimmed original both stay exactly where they
      // were, in place — instead of clearing immediately and letting the real event flicker
      // back to its old spot for however long the save takes, only to jump to the new spot once
      // it lands. `pending` (not `movingRef.current`, which a second drag started before this
      // one's save finishes would have since overwritten) guards the clear so an in-flight save
      // from an earlier drag can never clear a newer one's still-active preview.
      const offset = pending.deltaDays * 86400000 + pending.deltaMinutes * 60000;
      const start = new Date(new Date(pending.event.starts_at ?? 0).getTime() + offset);
      const end = new Date(new Date(pending.event.ends_at ?? 0).getTime() + offset);
      Promise.resolve(onCompleteRef.current(pending.event, start, end)).finally(() => {
        if (movingRef.current === pending) commit(null);
      });
    };
    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', finish);
    window.addEventListener('blur', finish);
    return () => {
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', finish);
      window.removeEventListener('blur', finish);
    };
  }, [vertical, commit]);
  const startMove = (calendarEvent: CalendarFormEvent, day: Date, event: React.MouseEvent) => {
    draggingRef.current = true;
    movedRef.current = false;
    anchorDayRef.current = day;
    anchorClientYRef.current = event.clientY;
    commit({ event: calendarEvent, deltaDays: 0, deltaMinutes: 0 });
  };
  const enterDay = (day: Date) => {
    if (!draggingRef.current || !movingRef.current || !anchorDayRef.current) return;
    const deltaDays = Math.round((day.getTime() - anchorDayRef.current.getTime()) / 86400000);
    if (deltaDays === movingRef.current.deltaDays) return;
    movedRef.current = true;
    commit({ ...movingRef.current, deltaDays });
  };
  // A hard clear with no completion — see useDayRangeDrag's reset for why.
  const reset = () => {
    draggingRef.current = false;
    movedRef.current = false;
    anchorDayRef.current = null;
    commit(null);
  };
  return { startMove, enterDay, moving, reset };
}

// Dragging an event's top or bottom edge resizes it — only the dragged edge moves, the other
// stays exactly where it was. Unlike a move, an edge is tracked at the exact day+time under the
// pointer (like the create-drag above), not as a delta from an anchor: there's no meaningful
// "grab offset" for an edge, since you grab the edge itself and it should just follow the
// cursor. Works both vertically (15-minute steps) and horizontally — dragging past a day
// boundary extends the event across days, same mechanism as useHourGridDrag's day tracking.
function useEventResizeDrag(onComplete: (event: CalendarFormEvent, start: Date, end: Date) => void | Promise<void>, topPadding: number, stepMinutes = 15) {
  type ResizeState = { event: CalendarFormEvent; edge: 'start' | 'end'; fixed: Date; day: Date; minutes: number };
  const [resize, setResize] = useState<ResizeState | null>(null);
  // Mirrors `resize`, read synchronously in `finish` — see useDayRangeDrag's rangeRef for why
  // `onComplete` must never be called from inside a `setResize` updater.
  const resizeRef = useRef<ResizeState | null>(null);
  const draggingRef = useRef(false);
  const movedRef = useRef(false);
  const rectRef = useRef<DOMRect | null>(null);
  const onCompleteRef = useRef(onComplete);
  onCompleteRef.current = onComplete;
  const minutesAt = useCallback((clientY: number) => {
    const rect = rectRef.current;
    if (!rect) return 0;
    const raw = clientY - rect.top - topPadding;
    return Math.min(1440, Math.max(0, Math.floor(raw / stepMinutes) * stepMinutes));
  }, [topPadding, stepMinutes]);
  const commit = useCallback((next: ResizeState | null) => { resizeRef.current = next; setResize(next); }, []);
  useEffect(() => {
    const onMouseMove = (event: MouseEvent) => {
      if (!draggingRef.current || !resizeRef.current) return;
      // Neither `mouseup` nor `blur` fires for every way a drag can end (a context menu or
      // native modal can swallow the mouseup silently) — self-correct on the next movement
      // once the button is no longer actually held, same reasoning as useDayRangeDrag.
      if (event.buttons === 0) { finish(); return; }
      const minutes = minutesAt(event.clientY);
      if (minutes === resizeRef.current.minutes) return;
      movedRef.current = true;
      commit({ ...resizeRef.current, minutes });
    };
    const finish = () => {
      const pending = draggingRef.current && movedRef.current ? resizeRef.current : null;
      draggingRef.current = false;
      movedRef.current = false;
      rectRef.current = null;
      if (!pending) { commit(null); return; }
      // See useEventDragMove's finish() for why the preview stays up (not cleared) until the
      // save actually completes, and why the clear is guarded by identity against a second,
      // newer drag having already started.
      const draggedInstant = pending.day.getTime() + pending.minutes * 60000;
      const fixedInstant = pending.fixed.getTime();
      const minSpan = stepMinutes * 60000;
      const start = pending.edge === 'start' ? new Date(Math.min(draggedInstant, fixedInstant - minSpan)) : pending.fixed;
      const end = pending.edge === 'end' ? new Date(Math.max(draggedInstant, fixedInstant + minSpan)) : pending.fixed;
      Promise.resolve(onCompleteRef.current(pending.event, start, end)).finally(() => {
        if (resizeRef.current === pending) commit(null);
      });
    };
    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', finish);
    window.addEventListener('blur', finish);
    return () => {
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', finish);
      window.removeEventListener('blur', finish);
    };
  }, [minutesAt, stepMinutes, commit]);
  // `event.currentTarget` is the resize handle itself — the Y-position reference has to be the
  // hour column's own rect (every column shares the same top), found via the marker attribute
  // TimeGrid puts on each one, same reasoning as useHourGridDrag's rect capture.
  const startResize = (calendarEvent: CalendarFormEvent, edge: 'start' | 'end', fixed: Date, day: Date, event: React.MouseEvent) => {
    const column = (event.currentTarget as HTMLElement).closest('[data-hour-column="true"]');
    rectRef.current = column ? column.getBoundingClientRect() : null;
    draggingRef.current = true;
    movedRef.current = false;
    const minutes = minutesAt(event.clientY);
    // Recorded in the ref only, not published to state — the drag hasn't moved anywhere yet,
    // so there's nothing new to preview. Publishing it here would render the preview stacked
    // on top of the (still fully visible) original for the instant between mousedown and the
    // first real movement, reading as two copies of the same event.
    resizeRef.current = { event: calendarEvent, edge, fixed, day, minutes };
  };
  const enterDay = (day: Date) => {
    if (!draggingRef.current || !resizeRef.current || resizeRef.current.day.toDateString() === day.toDateString()) return;
    movedRef.current = true;
    commit({ ...resizeRef.current, day });
  };
  // A hard clear with no completion — see useDayRangeDrag's reset for why.
  const reset = () => {
    draggingRef.current = false;
    movedRef.current = false;
    rectRef.current = null;
    commit(null);
  };
  return { startResize, enterDay, resize, reset };
}

// Dragging an all-day event's left or right edge resizes it — day-granularity only, no time
// axis. Same "dragged edge tracks the exact day under the pointer, the other edge stays fixed"
// logic as useEventResizeDrag, simplified down to one axis. `fixed` and the dragged `day` are
// both always resolved to [start, endInclusive] by plain min/max regardless of which edge is
// which, then the endInclusive day is bumped one past to match the all-day model's exclusive
// end (`ends_at` is always one day past the last visible day).
function useAllDayResizeDrag(onComplete: (event: CalendarFormEvent, start: Date, end: Date) => void | Promise<void>) {
  type ResizeState = { event: CalendarFormEvent; fixed: Date; day: Date };
  const [resize, setResize] = useState<ResizeState | null>(null);
  const resizeRef = useRef<ResizeState | null>(null);
  const draggingRef = useRef(false);
  const movedRef = useRef(false);
  const onCompleteRef = useRef(onComplete);
  onCompleteRef.current = onComplete;
  const commit = useCallback((next: ResizeState | null) => { resizeRef.current = next; setResize(next); }, []);
  useEffect(() => {
    const finish = () => {
      const pending = draggingRef.current && movedRef.current ? resizeRef.current : null;
      draggingRef.current = false;
      movedRef.current = false;
      if (!pending) { commit(null); return; }
      // See useEventDragMove's finish() for why the preview stays up (not cleared) until the
      // save actually completes, and why the clear is guarded by identity against a second,
      // newer drag having already started.
      const startDay = pending.day <= pending.fixed ? pending.day : pending.fixed;
      const lastInclusiveDay = pending.day <= pending.fixed ? pending.fixed : pending.day;
      const endExclusive = new Date(lastInclusiveDay.getFullYear(), lastInclusiveDay.getMonth(), lastInclusiveDay.getDate() + 1);
      Promise.resolve(onCompleteRef.current(pending.event, startDay, endExclusive)).finally(() => {
        if (resizeRef.current === pending) commit(null);
      });
    };
    // Neither `mouseup` nor `blur` fires for every way a drag can end (a context menu or
    // native modal can swallow the mouseup silently) — self-correct on the next movement once
    // the button is no longer actually held, same reasoning as useDayRangeDrag. This hook has
    // no other mousemove listener (day-crossing alone drives it via enterDay), so this is the
    // only thing that would ever notice a swallowed mouseup here.
    const onMouseMove = (event: MouseEvent) => {
      if (draggingRef.current && event.buttons === 0) finish();
    };
    document.addEventListener('mouseup', finish);
    document.addEventListener('mousemove', onMouseMove);
    window.addEventListener('blur', finish);
    return () => {
      document.removeEventListener('mouseup', finish);
      document.removeEventListener('mousemove', onMouseMove);
      window.removeEventListener('blur', finish);
    };
  }, [commit]);
  const startResize = (calendarEvent: CalendarFormEvent, fixed: Date, day: Date) => {
    draggingRef.current = true;
    movedRef.current = false;
    // Recorded in the ref only, not published to state — see useEventResizeDrag's startResize
    // for why: nothing to preview yet, and publishing here would double the original.
    resizeRef.current = { event: calendarEvent, fixed, day };
  };
  const enterDay = (day: Date) => {
    if (!draggingRef.current || !resizeRef.current || resizeRef.current.day.toDateString() === day.toDateString()) return;
    movedRef.current = true;
    commit({ ...resizeRef.current, day });
  };
  // A hard clear with no completion — see useDayRangeDrag's reset for why.
  const reset = () => {
    draggingRef.current = false;
    movedRef.current = false;
    commit(null);
  };
  return { startResize, enterDay, resize, reset };
}

// The title/time/location content of a timed event box — shared between the real event button
// and the move/resize ghost preview, so a preview being dragged looks exactly like the real
// thing it's a preview of, not a simplified stand-in.
function TimedEventBody({ event, t }: { event: { summary?: string | null; starts_at?: string | number | Date | null; location?: string | null }; t: TFunction }) {
  return <>
    <span style={timedEventTitle}>{event.summary || t('calendar.untitled')}</span>
    <span style={timedEventTimeRow}><svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" aria-hidden="true"><circle cx="12" cy="12" r="10" /><polyline points="12 6 12 12 16 14" /></svg>{eventTime(event)}</span>
    {event.location && <span style={timedEventLoc}>{event.location}</span>}
  </>;
}

// Mirrors calendarView.ts's `sortedDayEvents` comparator exactly (all-day first, longer spans
// first, earlier start first, id as a final tiebreak) — used to find where a synthetic preview
// "would" sort into a day's already-sorted `others` list (single-day all-day events and timed
// events — a multi-day spanner uses its own shared weekly lane instead, see monthWeekLanes),
// using the *dragged* event's own, unshifted properties rather than the preview's shifted ones.
function compareAllDayOrder(a: CalendarFormEvent, b: CalendarFormEvent): number {
  const allDayA = Boolean(a.all_day || a.allDay); const allDayB = Boolean(b.all_day || b.allDay);
  if (allDayA !== allDayB) return allDayA ? -1 : 1;
  if (allDayA) {
    const spanDiff = allDaySpanDays(b) - allDaySpanDays(a);
    if (spanDiff !== 0) return spanDiff;
    const startDiff = new Date(String(a.starts_at ?? a.startsAt ?? 0)).getTime() - new Date(String(b.starts_at ?? b.startsAt ?? 0)).getTime();
    if (startDiff !== 0) return startDiff;
    return String(a.id).localeCompare(String(b.id));
  }
  // Both timed: `sortedDayEvents`'s own comparator (which real events are already sorted by)
  // compares full start instants, which works there because every real event it compares is
  // already sitting on the day it's actually being rendered for. The event this comparator is
  // finding an insertion point for is being dragged onto a *different* day than the one its own
  // `starts_at` still names — comparing full instants would compare against that stale, no-
  // longer-relevant original day instead of the target cell's other events' times, e.g. an event
  // dragged forward from the 10th would always sort before anything on the 17th regardless of
  // either one's actual clock time. Comparing minutes-since-midnight instead orders purely by
  // time of day, which is the only part of `starts_at` still meaningful once it's landed on a
  // new day.
  const minutesOfDay = (event: CalendarFormEvent) => { const d = new Date(String(event.starts_at ?? event.startsAt ?? 0)); return d.getHours() * 60 + d.getMinutes(); };
  const timeDiff = minutesOfDay(a) - minutesOfDay(b);
  if (timeDiff !== 0) return timeDiff;
  return String(a.id).localeCompare(String(b.id));
}

// The month-view drag preview — where a moved/resized event is going, rendered with the same
// look as a real event card but non-interactive. Its position in the stack comes from where the
// caller inserts it into that day's render list (see `renderList` below), not from anything
// here.
// `titleVisible` is deliberately its own prop, not derived from `segment.continuesFrom` here —
// the two answer different questions. `segment` says whether *this day's edge* touches another
// segment of the same bar (ghost, preview, or a genuine multi-day continuation) and drives the
// border-radius/padding a squared edge needs regardless of which of those it's touching for.
// `titleVisible` says whether this is the one segment that actually gets to show the event's
// name — normally "not a continuation", but a day the ghost's own segment is suppressed on (in
// favor of the preview) can leave the *only* real day that wasn't squared off with nothing to
// show a title on at all, which read as the whole preview having lost its name.
function MonthDragRow({ segment, event, testId, t, titleVisible }: { segment: { continuesFrom: boolean; continuesTo: boolean }; event: CalendarFormEvent; testId?: string; t: TFunction; titleVisible: boolean }) {
  return <div aria-hidden="true" data-testid={testId} style={{ ...eventRow, position: 'relative', marginLeft: segment.continuesFrom ? -7 : 0, marginRight: segment.continuesTo ? -7 : 0 }}>
    <div style={{ ...eventCard, background: eventTint(event.calendar_color), borderTopLeftRadius: segment.continuesFrom ? 0 : 4, borderBottomLeftRadius: segment.continuesFrom ? 0 : 4, borderTopRightRadius: segment.continuesTo ? 0 : 4, borderBottomRightRadius: segment.continuesTo ? 0 : 4, paddingLeft: segment.continuesFrom ? 10 : 6, paddingRight: segment.continuesTo ? 10 : 6 }}>
      <span style={titleVisible ? eventCardTitle : { ...eventCardTitle, visibility: 'hidden' }}>{event.summary || t('calendar.untitled')}</span>
      {!(event.all_day || event.allDay) && <strong style={eventCardTime}>{eventTime(event)}</strong>}
    </div>
  </div>;
}

function CalendarGrid({ days, dayEventsFor, view, anchor, isMobile, locale, onSelectDay, onOpenDayAgenda, openCreate, openTimedRangeCreate, moveEvent, openEdit, openContextMenu, t, calendarWorkHoursStart, calendarWorkHoursEnd }: CalendarGridProps) {
  const month = view === 'month';
  // Declared unconditionally (Rules of Hooks) even though it's only wired up below —
  // the early return for non-month views happens after every hook in this component.
  const dayDrag = useDayRangeDrag((start, end) => openCreate(start, end));
  // Dragging an event card moves it to the day dropped on, day-granularity only — month view
  // has no time-of-day axis to drag on. See useEventDragMove above.
  const monthEventDrag = useEventDragMove(moveEvent, { vertical: false });
  // Dragging a multi-day event's left or right edge resizes it — same hook the all-day row
  // uses, unmodified: it's already day-only with no week-view-specific internals.
  // A timed event resizes by whole days too, keeping its own start and end times.
  const monthResize = useAllDayResizeDrag((event, start, end) => {
    if (event.all_day || event.allDay) return moveEvent(event, start, end);
    const timed = resizedTimedRange(event, start, end);
    return timed ? moveEvent(event, timed.start, timed.end) : undefined;
  });
  // Hovering any one segment of a multi-day event highlights every one of its segments, not
  // just the one under the cursor — each day's cell stacks its own events as separate DOM
  // elements (there's no single spanning box the way week view's all-day pill is one CSS-grid
  // column-span element), so a plain CSS `:hover` only ever reaches the segment actually under
  // the pointer. Tracked by id in JS instead, so every segment can react together. Declared
  // unconditionally alongside the drag hooks above, before the non-month early return.
  const [hoveredEventId, setHoveredEventId] = useState<string | null>(null);
  // Month rows get a fixed height from the space the grid is given — never from their content —
  // and each cell shows only as many events as fit that height. A row sized by its content grew
  // whenever a drag preview added one more event to a full cell, which shifted the grid under the
  // pointer (and added a scrollbar), moving the drag to another cell and back: flicker.
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const headerRef = useRef<HTMLDivElement | null>(null);
  const [gridSpace, setGridSpace] = useState(0);
  useLayoutEffect(() => {
    const surface = surfaceRef.current;
    if (!month || !surface) return;
    const measure = () => setGridSpace(Math.max(0, surface.clientHeight - (headerRef.current?.offsetHeight ?? 0) - 1));
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(surface);
    return () => observer.disconnect();
  }, [month]);
  // Navigating to a different period (month/week) makes any drag against the *old* one
  // meaningless — hard-clear all three regardless of whether anything else already would
  // have. `days[0]`/`days[days.length - 1]` (not the `days` array itself, a new reference on
  // every render) only actually changes value when the visible range really does.
  const rangeKey = `${days[0]?.toDateString()}|${days[days.length - 1]?.toDateString()}`;
  useEffect(() => {
    dayDrag.reset();
    monthEventDrag.reset();
    monthResize.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reset()s are stable per mount, not per render
  }, [rangeKey]);
  // Also hard-clear on every fresh events fetch, not just on navigating — `dayEventsFor` is a
  // brand new function reference exactly when the underlying event data actually changed (a
  // completed edit's refetch, a source sync landing, a calendar visibility toggle), and stays
  // referentially stable across every render in between. A drag/resize whose `onComplete`
  // promise never actually settles (a request that hangs, or a recurring-scope dialog the user
  // navigated away from without answering) would otherwise leave its preview and its "hide the
  // real segment here" filter stuck forever — reappearing, misaligned, on whatever the *next*
  // fetch's data happens to be, since nothing else ever calls `commit(null)` for it.
  useEffect(() => {
    dayDrag.reset();
    monthEventDrag.reset();
    monthResize.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reset()s are stable per mount, not per render
  }, [dayEventsFor]);
  if (!month) return <TimeGrid days={days} dayEventsFor={dayEventsFor} view={view} isMobile={isMobile} locale={locale} openCreate={openCreate} openTimedRangeCreate={openTimedRangeCreate} moveEvent={moveEvent} openEdit={openEdit} openContextMenu={openContextMenu} onSelectDay={onSelectDay} anchor={anchor} t={t} calendarWorkHoursStart={calendarWorkHoursStart} calendarWorkHoursEnd={calendarWorkHoursEnd} />;
  // Only once the drag has actually crossed into a different day is there anywhere new to
  // preview — while it's still hovering its own origin cell (deltaDays 0, the moment right
  // after mousedown), treating that as "moved here" rendered the dimmed original *and* a
  // preview stacked in the same cell, reading as two copies of the same event. (Resizing
  // doesn't need the same filter: useAllDayResizeDrag never publishes state until real
  // movement in the first place.)
  const moving = monthEventDrag.moving && monthEventDrag.moving.deltaDays !== 0 ? monthEventDrag.moving : null;
  const resizing = monthResize.resize;
  // A hypothetical event carrying the move/resize's new start/end — reusing
  // `allDayEventSegment` (the same function real events use) to place its ghost, so a
  // multi-day preview bridges cells and squares off edges exactly like the real thing.
  const previewEvent = moving ? (() => {
    const shift = (key: string) => { const d = dayFromKey(key); return dateOnly(new Date(d.getFullYear(), d.getMonth(), d.getDate() + moving.deltaDays)); };
    const isAllDay = Boolean(moving.event.all_day || moving.event.allDay);
    const startKey = String(moving.event.starts_at ?? moving.event.startsAt ?? '').slice(0, 10);
    const rawEndKey = String(moving.event.ends_at ?? moving.event.endsAt ?? '').slice(0, 10);
    // Month view has no time axis, and a plain (non all-day) event's `ends_at` is just a later
    // *time*, usually on the very same calendar day as `starts_at` — reusing that date directly
    // as the exclusive end below made the preview's day-membership check (`dayKey <
    // previewEvent.ends_at`) never match any day at all for a same-day event, since start and
    // end then carried the identical date. An all-day event's `ends_at` is already the correct
    // exclusive end (one day past its last visible day) and multi-day timed events are rare
    // enough here not to special-case: a timed event always previews as exactly the one day
    // it's being dragged to.
    // A timed spanner keeps its whole local day range (`shift()` below moves it by the drag).
    const timedSpan = !isAllDay && monthSpanDays(moving.event) > 1 ? monthSpanKeys(moving.event) : null;
    const endKey = timedSpan ? timedSpan.endKey
      : isAllDay && rawEndKey > startKey ? rawEndKey : dateOnly(new Date(dayFromKey(startKey).getFullYear(), dayFromKey(startKey).getMonth(), dayFromKey(startKey).getDate() + 1));
    // `shift()` above is date-only — correct for an all-day event's `starts_at`, which never
    // carries a meaningful time, but for a *timed* event it discarded the clock time entirely,
    // so the preview's displayed time (eventTime() reads it straight off `starts_at`) fell back
    // to midnight — reinterpreted in the viewer's own timezone by `new Date("2026-09-17")`
    // parsing a date-only string as UTC midnight, which showed up as e.g. 02:00 in CEST. A timed
    // event's start keeps its real hour/minute/second, shifted by day only, and built as a
    // timezone-less local string (no trailing `Z`) so both `new Date(...)` for display and this
    // same string's own `.slice(0, 10)` for day-membership agree with each other and with the
    // viewer's own calendar days — the same reasoning `dateOnly()` above exists for at all.
    const startsAt = isAllDay ? shift(startKey) : (() => {
      const original = new Date(String(moving.event.starts_at ?? moving.event.startsAt ?? 0));
      const shifted = new Date(original.getFullYear(), original.getMonth(), original.getDate() + moving.deltaDays, original.getHours(), original.getMinutes(), original.getSeconds());
      const pad = (value: number) => String(value).padStart(2, '0');
      return `${shifted.getFullYear()}-${pad(shifted.getMonth() + 1)}-${pad(shifted.getDate())}T${pad(shifted.getHours())}:${pad(shifted.getMinutes())}:${pad(shifted.getSeconds())}`;
    })();
    return { ...moving.event, starts_at: startsAt, ends_at: shift(endKey) };
  })() : resizing ? (() => {
    const startDay = resizing.day <= resizing.fixed ? resizing.day : resizing.fixed;
    const lastInclusiveDay = resizing.day <= resizing.fixed ? resizing.fixed : resizing.day;
    const endExclusive = new Date(lastInclusiveDay.getFullYear(), lastInclusiveDay.getMonth(), lastInclusiveDay.getDate() + 1);
    // A timed event previews from its start time on the new first day (see resizedTimedRange).
    const timed = resizing.event.all_day || resizing.event.allDay ? null : resizedTimedRange(resizing.event, startDay, endExclusive);
    return { ...resizing.event, starts_at: timed ? toDateTimeLocal(timed.start) : dateOnly(startDay), ends_at: dateOnly(endExclusive) };
  })() : null;
  // The event actually being dragged, in its real (unmoved) form — its own sort-relevant
  // properties (not the shifted preview's) decide where the preview inserts into a day's
  // render list below, so the preview lands in the exact slot the real event naturally would.
  const draggedEvent = moving ? moving.event : resizing ? resizing.event : null;
  // The ghost and the preview are two visually distinct things (dimmed original vs. bright new
  // position) — they're never merged into one seamless bar that pretends to be a single element.
  // Each rounds off at *its own* true edges independently, using only its own start/end: the
  // ghost is simply the real event, rendered through the exact same logic as when nothing is
  // being dragged (segment, corners, title — none of it special-cased for dragging any more),
  // just dimmed. It is never removed from a day just because the preview also reaches there —
  // shortening the ghost down to whatever the preview *didn't* cover read as the event itself
  // shrinking mid-drag, not a preview of a new position. Wherever the two do share a day, the
  // preview renders as an absolutely-positioned overlay directly on top of that day's real
  // (dimmed) ghost — same lane, same row, stacked instead of competing for it — so a busier
  // neighboring day never has anything to knock either one out of step with its own other days.
  // A multi-day (spanDays > 1) event gets a *lane* assigned once per calendar week — the same
  // greedy interval-partitioning `layoutWeekAllDayEvents` already uses for week view's all-day
  // band — instead of relying on each of its 7 day cells independently sorting its own,
  // potentially differently-shaped event list and hoping they all happen to land the spanning
  // event on the same relative row. They usually do, but nothing guarantees it: a neighboring
  // cell having one more (or fewer) same-day event, or the dragged event's own real segment
  // being filtered out of just the days the preview covers, shifts every later item's index in
  // that cell alone — enough to knock a spanning event's row out of step with its neighbors,
  // which reads as the bar "breaking" even though every individual segment is, in isolation,
  // rendered correctly. A lane shared across the whole week makes a multi-day event's row a
  // property of the week, not an accident of per-cell sort order, so it can't drift.
  const monthWeeks: Date[][] = [];
  if (month) for (let index = 0; index < days.length; index += 7) monthWeeks.push(days.slice(index, index + 7));
  const monthWeekLanes = new Map<number, { laneOf: Map<string, number>; laneCount: number }>();
  monthWeeks.forEach((week, weekIndex) => {
    const spanningById = new Map<string, CalendarFormEvent>();
    week.forEach(day => dayEventsFor(day).forEach(event => {
      // A timed spanner is packed by its local day range, as if it were all-day.
      if (monthSpanDays(event) > 1) spanningById.set(String(event.id), (event.all_day || event.allDay) ? event : { ...event, all_day: true, starts_at: monthSpanKeys(event).startKey, ends_at: monthSpanKeys(event).endKey });
    }));
    // While a multi-day event is being dragged, the lane it needs to avoid colliding with other
    // real events isn't decided by its own real (unmoved) range alone — the preview's shifted
    // range has to be considered too, since dragging it onto a week that already has a different
    // multi-day event sitting in the ghost's old lane number, or into a week the ghost never
    // touched at all, otherwise left the preview either overlapping that other event's own bar
    // (never actually checked against it) or without any lane reservation in that week whatsoever
    // (falling back to a single-cell insertion that can't stay aligned across the days it spans).
    // Folding the ghost's and the preview's ranges into one combined item — the union of
    // wherever this event still visually is and wherever it's about to be — before handing this
    // week's items to the greedy packer routes it around anything either range would collide
    // with, and keeps the ghost and the preview reporting the *same* lane for this week (needed
    // for them to stack together, not compete for separate rows, on whatever days they overlap).
    if (draggedEvent && previewEvent) {
      const draggedId = String(draggedEvent.id);
      const real = spanningById.get(draggedId);
      const weekStartKey = dateOnly(week[0]);
      const weekEndKeyExclusive = dateOnly(new Date(week[6].getFullYear(), week[6].getMonth(), week[6].getDate() + 1));
      const previewStartKey = String(previewEvent.starts_at).slice(0, 10);
      const previewEndKey = String(previewEvent.ends_at).slice(0, 10);
      const previewTouchesWeek = previewEndKey > weekStartKey && previewStartKey < weekEndKeyExclusive;
      if (real || previewTouchesWeek) {
        const startKeys = [real ? String(real.starts_at ?? real.startsAt ?? '').slice(0, 10) : null, previewTouchesWeek ? previewStartKey : null].filter((key): key is string => key != null);
        const endKeys = [real ? String(real.ends_at ?? real.endsAt ?? '').slice(0, 10) : null, previewTouchesWeek ? previewEndKey : null].filter((key): key is string => key != null);
        // Packed as all-day (a timed spanner too, or the packer drops it) and ranked by its own
        // span, not the wider union, so bars keep ordering by days spanned, longest first.
        spanningById.set(draggedId, { ...draggedEvent, all_day: true, starts_at: startKeys.sort()[0], ends_at: endKeys.sort().at(-1), span_days: monthSpanDays(draggedEvent) });
      }
    }
    const placements = layoutWeekAllDayEvents([...spanningById.values()], week);
    // The packer places longer events first and only tracks where each lane *ends*, so a
    // multi-day event later in the week makes its lane look taken on earlier days too. Lift
    // only the dragged event's lane to the lowest one that is actually free across its days;
    // every other event keeps exactly the lane the packer gave it.
    if (draggedEvent && previewEvent) {
      const dragged = placements.find(placement => String(placement.event.id) === String(draggedEvent.id));
      if (dragged) {
        const laneFree = (lane: number) => placements.every(other => other === dragged || other.lane !== lane
          || other.endIndex <= dragged.startIndex || other.startIndex >= dragged.endIndex);
        for (let lane = 0; lane < dragged.lane; lane += 1) {
          if (laneFree(lane)) { dragged.lane = lane; break; }
        }
      }
    }
    const laneOf = new Map(placements.map(placement => [String(placement.event.id), placement.lane]));
    const laneCount = placements.reduce((max, placement) => Math.max(max, placement.lane + 1), 0);
    monthWeekLanes.set(weekIndex, { laneOf, laneCount });
  });
  const monthRows = Math.max(1, Math.ceil(days.length / 7));
  const monthRowHeight = gridSpace > 0 ? Math.max(MONTH_ROW_MIN, Math.floor((gridSpace - (monthRows - 1)) / monthRows)) : 0;
  // Cell padding 6+6, date chip 20 + 6 margin, 2px gap to the stack; then one event row each
  // (an 11px card at line-height 1.5 plus 2+2 padding, 24 on a phone) with a 2px gap between.
  const eventRowHeight = isMobile ? 24 : 21;
  const cellCapacity = Math.max(1, Math.floor(((monthRowHeight || MONTH_ROW_MIN) - 34 + 2) / (eventRowHeight + 2)));
  return <div ref={surfaceRef} data-testid="calendar-grid" style={{ ...calendarSurface, flex: 1, minWidth: 0 }}>
    <div ref={headerRef} style={monthDow}>{days.slice(0, 7).map(day => <div key={`header-${day.toISOString()}`} style={monthDowCell}><span data-testid="calendar-weekday" style={monthDowLabel}>{day.toLocaleDateString(locale, { weekday: 'long' })}</span></div>)}</div>
    <div data-testid="calendar-month-grid" style={{ ...dayGrid, gridTemplateColumns: `repeat(7, minmax(${isMobile && !month ? 112 : 0}px, 1fr))`, gridAutoRows: monthRowHeight ? `${monthRowHeight}px` : 'minmax(108px, 1fr)', gap: 1, background: 'var(--border-subtle)', borderTop: '1px solid var(--border-subtle)' }}>
      {days.map((day, dayIndex) => {
        const inMonth = day.getMonth() === anchor.getMonth(); const dayEvents = dayEventsFor(day);
        const dayKey = dateOnly(day);
        // The grid wraps every 7 days — a calendar row has no cell to its right past the last
        // column (or to its left before the first), so a segment that conceptually continues
        // past either edge has nowhere to visually bridge into within *this* row. Squaring that
        // edge off anyway (or bleeding the bar's negative margin past it, per MonthDragRow/the
        // real event's own rendering below) pokes the bar out past the grid's own border instead
        // of reading as "continues on the next row" — every row-edge cell rounds off and shows
        // its own title, same as a genuine first/last day, regardless of what the event's own
        // date range says.
        const isRowStart = dayIndex % 7 === 0;
        const isRowEnd = dayIndex % 7 === 6;
        const previewSegmentRaw = previewEvent && dayKey >= String(previewEvent.starts_at).slice(0, 10) && dayKey < String(previewEvent.ends_at).slice(0, 10) ? allDayEventSegment(previewEvent, day) : null;
        // The preview's own edges, clamped back to rounded at a row edge (per the comment
        // above) — never squared just because the ghost happens to be sitting right next to it.
        const previewSegment = previewSegmentRaw ? {
          continuesFrom: !isRowStart && previewSegmentRaw.continuesFrom,
          continuesTo: !isRowEnd && previewSegmentRaw.continuesTo,
        } : null;
        // Multi-day events (and their move/resize preview) are placed by the *lane* their event
        // id was assigned for this whole week (see monthWeekLanes above) — a shared, week-wide
        // assignment, not this cell's own independent sort — so the exact same event always
        // lands on the exact same row in every day cell it touches, with no way for it to drift
        // out of step with its neighbors. Everything else (single-day all-day events, timed
        // events) keeps sorting and stacking below the lanes exactly as it always has; `dayEvents`
        // already arrives in that sorted order from `dayEventsFor`, so simply filtering the
        // spanning ones out of it leaves the rest still correctly ordered relative to each other.
        const { laneOf, laneCount } = monthWeekLanes.get(Math.floor(dayIndex / 7)) ?? { laneOf: new Map<string, number>(), laneCount: 0 };
        // A lane can hold a ghost, a preview, or (while a drag's new position overlaps its own
        // old one) both at once — never a choice between them. See the comment above
        // monthWeekLanes for why removing one in favor of the other was wrong.
        const laneSlots: Array<{ ghost?: CalendarFormEvent; preview?: { event: CalendarFormEvent; segment: { continuesFrom: boolean; continuesTo: boolean }; titleVisible: boolean } } | undefined> = new Array(laneCount);
        const others: Array<{ event: CalendarFormEvent; preview: boolean; segment?: { continuesFrom: boolean; continuesTo: boolean }; titleVisible?: boolean }> = [];
        dayEvents.forEach(event => {
          const lane = monthSpanDays(event) > 1 ? laneOf.get(String(event.id)) : undefined;
          if (lane != null) laneSlots[lane] = { ...laneSlots[lane], ghost: event };
          else others.push({ event, preview: false });
        });
        if (previewSegment && previewEvent && draggedEvent) {
          const existingLane = laneOf.get(String(draggedEvent.id));
          // The preview's title shows on its own true first day, and nowhere else — never a
          // fallback to some other day, and never affected by the ghost's position, since the
          // two are never merged into one bar and never compete for the same slot.
          const previewEntry = { event: previewEvent, segment: previewSegment, titleVisible: !previewSegment.continuesFrom };
          if (existingLane != null) {
            // The dragged event is itself a multi-day spanner — the preview takes over its own
            // lane (the same row it would already be occupying if it weren't being dragged),
            // stacking on top of that lane's ghost (still there, dimmed, if this day is also
            // within the ghost's own real range) rather than replacing it.
            laneSlots[existingLane] = { ...laneSlots[existingLane], preview: previewEntry };
          } else {
            // The dragged event was never a multi-day spanner (a plain timed event, or a
            // single-day all-day one) — it only ever occupies the one cell it's on, so there's
            // no cross-cell alignment to guarantee here, and no ghost of its own to ever share a
            // day with (a timed event moves to a genuinely different day). It inserts into this
            // day's already-sorted `others` list at the position its own (unshifted) sort key
            // would occupy, the same comparator `sortedDayEvents` uses, so it lands in the same
            // relative slot the real event naturally would.
            const insertAt = others.findIndex(({ event }) => compareAllDayOrder(draggedEvent, event) < 0);
            others.splice(insertAt === -1 ? others.length : insertAt, 0, { event: previewEvent, preview: true, segment: previewSegment, titleVisible: previewEntry.titleVisible });
          }
        }
        // Simply dropping empty lanes (rather than leaving a gap for them) would compact this
        // cell's own rows back together whenever a lower lane happens to be unoccupied *here* —
        // e.g. a lane a spanning event shares with another one that doesn't reach this
        // particular day. That shifts the occupied lane up to a lower visual row on exactly this
        // cell, which is the same "drifts out of step with its neighbors" problem lanes exist to
        // prevent, just moved one level down. A blank spacer row for any skipped lane below the
        // highest one actually occupied here keeps every occupied lane at its own fixed visual
        // row — trailing lanes nothing in this cell reaches at all are left off the end entirely
        // rather than padding out cells that don't need the extra height.
        let highestLane = -1;
        laneSlots.forEach((slot, lane) => { if (slot) highestLane = lane; });
        type LaneRenderItem = { ghost?: CalendarFormEvent; preview?: { event: CalendarFormEvent; segment: { continuesFrom: boolean; continuesTo: boolean }; titleVisible: boolean } };
        type OtherRenderItem = { event: CalendarFormEvent; preview: boolean; segment?: { continuesFrom: boolean; continuesTo: boolean }; titleVisible?: boolean };
        const laneRows: Array<LaneRenderItem | { empty: true }> = [];
        for (let lane = 0; lane <= highestLane; lane += 1) laneRows.push(laneSlots[lane] ?? { empty: true });
        const renderList: Array<LaneRenderItem | OtherRenderItem | { empty: true }> = [...laneRows, ...others];
        // A cell shows every event that fits its fixed height (cellCapacity above); once more
        // than that, the last fitting row becomes a "+N more" chip instead.
        const visibleList = renderList.length <= cellCapacity ? renderList : renderList.slice(0, cellCapacity - 1);
        const hiddenCount = renderList.length - visibleList.length;
        // The one place a month-view event card is built — a real event and the dragged event's
        // own ghost are the exact same rendering, right down to the same segment/corner/title
        // logic, and differ only in opacity (dimmed while being moved/resized). There is no
        // ghost-specific code path left to drift out of sync with how a plain event renders.
        const renderRealEventCard = (event: CalendarFormEvent) => {
          const showMenu = (target: React.MouseEvent<HTMLButtonElement>) => openContextMenu(event, target.clientX, target.clientY, target.currentTarget);
          const invokeMenu = (keyboardEvent: React.KeyboardEvent<HTMLButtonElement>) => {
            // Opening now needs a double-click from a mouse, but a button's native click
            // still fires on Enter/Space — that's the keyboard path's only way in, so it
            // still opens straight to edit rather than requiring a "double-press".
            if (keyboardEvent.key === 'Enter' || keyboardEvent.key === ' ') { keyboardEvent.preventDefault(); openEdit(event); return; }
            if (keyboardEvent.key !== 'ContextMenu' && !(keyboardEvent.shiftKey && keyboardEvent.key === 'F10')) return;
            keyboardEvent.preventDefault();
            openContextMenu(event, keyboardEvent.currentTarget.getBoundingClientRect().right, keyboardEvent.currentTarget.getBoundingClientRect().bottom, keyboardEvent.currentTarget);
          };
          const movable = !event.read_only && isWritableCalendarSource(event.source);
          const beingMoved = moving != null && String(moving.event.id) === String(event.id);
          const beingResized = resizing != null && String(resizing.event.id) === String(event.id);
          // A multi-day all-day event bleeds into this cell's own padding on the side(s) it
          // continues, and squares off that edge instead of rounding it, so the same event reads
          // as one bar stretching across its days rather than separate chips — the same technique
          // the week view's all-day band already uses. Exactly the same whether or not this is
          // the event actually being dragged: its own ghost is just this same event, dimmed.
          const allDay = Boolean(event.all_day || event.allDay);
          const span = monthSpanKeys(event);
          const timedSpanner = !allDay && monthSpanDays(event) > 1;
          const segmentRaw = allDay ? allDayEventSegment(event, day)
            : timedSpanner ? { continuesFrom: span.startKey < dayKey, continuesTo: span.endKey > dateOnly(new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1)) } : null;
          // Clamped to rounded at a row edge (see isRowStart/isRowEnd above) — there's no cell
          // to bridge into past the last (or before the first) column of this calendar row,
          // so bleeding/squaring off there pokes the bar out past the grid's own border instead
          // of reading as "continues on the next row".
          const segment = segmentRaw ? { continuesFrom: !isRowStart && segmentRaw.continuesFrom, continuesTo: !isRowEnd && segmentRaw.continuesTo } : null;
          const spanning = Boolean(segment?.continuesFrom || segment?.continuesTo);
          // Resize handles aren't gated on `spanning` — a single-day all-day event can grow into
          // a multi-day one exactly the same way week view's all-day band already lets it (that
          // band never required spanning either; see its own resize handles below). `segmentRaw`
          // (not `spanning`) is the real gate: it's truthy only for an all-day event, so a timed
          // event — month view has no time axis to resize by day-granularity — still never gets
          // one, while a single-day all-day event's segment (continuesFrom/To both false) now
          // does. monthWeekLanes already handles the lane alignment this needs mid-drag: it folds
          // the resize preview's range into spanningById even when the real event isn't a spanner
          // yet (see its `previewTouchesWeek` fallback above), so the growing bar still lands on
          // the same row across every day it comes to cover.
          const resizable = movable;
          const timedCard = !segmentRaw;
          const titleVisible = !segment?.continuesFrom;
          // A bar's title may run on over the rest of its days in this row instead of being cut at
          // its first cell: `n` cells of `100% + 22px` pitch (cell padding 6 + bleed 7 + 1px gap,
          // minus this button's own 6/10 padding), stopping short of a last cell that shows an end time.
          const rowDays = Math.min(7 - (dayIndex % 7), Math.round((dayFromKey(span.endKey).getTime() - dayFromKey(dayKey).getTime()) / 86400000));
          const endsHere = timedSpanner && span.endKey === dateOnly(new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1));
          const endsInRow = Math.round((dayFromKey(span.endKey).getTime() - dayFromKey(dayKey).getTime()) / 86400000) <= 7 - (dayIndex % 7);
          const runCells = timedSpanner && endsInRow ? rowDays - 1 : rowDays;
          const titleRun = titleVisible && segment?.continuesTo && runCells > 1 ? `calc(${runCells} * (100% + 22px) - 25px)` : null;
          const timeLabel = !timedSpanner ? (allDay ? null : eventTime(event))
            : span.startKey === dayKey ? eventTime(event)
              : endsHere ? t('calendar.endsAt', { time: new Date(String(event.ends_at ?? event.endsAt ?? '')).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) })
                : null;
          // Hovering any one segment of a multi-day event highlights every one of its segments
          // (see `hoveredEventId` above) — single-day events already get the same effect for
          // free from plain CSS `:hover`, so this only actually changes anything visible for a
          // spanning one.
          const highlighted = spanning && hoveredEventId != null && hoveredEventId === String(event.id);
          const startKey = span.startKey;
          const exclusiveEndDay = dayFromKey(span.endKey);
          const eventLastInclusiveDay = new Date(exclusiveEndDay.getFullYear(), exclusiveEndDay.getMonth(), exclusiveEndDay.getDate() - 1);
          // The month grid draws its cell borders as a 1px gap with the border color showing
          // through underneath (gap: 1, background: var(--border-subtle)), so bridging it takes
          // covering the cell's own padding *and* that gap — not just the padding — or a 1px
          // seam remains between two segments of one event.
          return <div key={event.id} data-testid={spanning ? 'calendar-month-span' : undefined} style={{ ...eventRow, position: 'relative', zIndex: spanning ? (titleRun ? 2 : 1) : undefined, marginLeft: segment?.continuesFrom ? -7 : 0, marginRight: segment?.continuesTo ? -7 : 0 }}>
            <button className="cal-ev" data-timed={timedCard || undefined} onMouseDown={mouseDownEvent => { if (movable) { mouseDownEvent.stopPropagation(); monthEventDrag.startMove(event, day, mouseDownEvent); } }} onDoubleClick={() => openEdit(event)} onContextMenu={event => { event.preventDefault(); showMenu(event); }} onKeyDown={invokeMenu} onMouseEnter={() => setHoveredEventId(String(event.id))} onMouseLeave={() => setHoveredEventId(current => current === String(event.id) ? null : current)} title={event.read_only ? t('calendar.readOnly') : t('calendar.edit')} style={{ ...eventCard, ...(timedCard ? eventCardTimed : {}), background: timedCard ? 'transparent' : eventFill(event, beingMoved || beingResized ? EVENT_DIM_RATIO : EVENT_OPACITY), color: timedCard ? 'var(--text-primary)' : 'white', opacity: timedCard && (beingMoved || beingResized) ? 0.45 : 1, cursor: movable ? 'grab' : 'pointer', filter: highlighted ? 'brightness(1.18)' : undefined,
              // `borderRadius` (the shorthand key) above always sets a value for every corner —
              // these longhands must too, unconditionally, even when nothing needs to differ
              // from the shorthand's own 4px. The same CSSOM corruption this file already
              // documents for `background`/`backgroundColor` applies here too: once a longhand
              // like `borderTopLeftRadius` has been set inline even once, the browser has lost
              // the shorthand's own contribution to that corner — a later render that stops
              // including that longhand (e.g. `spanning` going from true back to false on this
              // same, persisted-across-renders button, mid-drag as continuesFrom/To flip) clears
              // it to nothing left to fall back to, not back to the shorthand's 4px, so the
              // corner sticks at the browser default of a hard 0 instead of re-rounding.
              borderTopLeftRadius: segment?.continuesFrom ? 0 : 4, borderBottomLeftRadius: segment?.continuesFrom ? 0 : 4, borderTopRightRadius: segment?.continuesTo ? 0 : 4, borderBottomRightRadius: segment?.continuesTo ? 0 : 4, paddingLeft: segment?.continuesFrom ? 10 : 6, paddingRight: segment?.continuesTo ? 10 : 6, overflow: 'visible' }}>{/* A multi-day event's title shows once, on its first visible day — every later segment is
                just the bar continuing, so repeating it read as separate events instead of one.
                Still rendered (not omitted) on a continuation, just visually hidden — an
                empty button has no line box to size itself by and collapses to near-zero
                height, which stopped the bar from reserving its row's space on days after
                the first, letting that day's other events overlap into where it should sit.
                The title comes first and shrinks (ellipsis) so the time — fixed width, never
                shrinks — always stays fully visible at the end of the line instead of getting
                clipped along with an overly long title. A timed event has no fill (month view has no
                time axis to give it a block) — just its calendar-colored bar ahead of the title. */}{timedCard && <span aria-hidden="true" style={{ ...eventCardBar, background: eventTint(event.calendar_color, 1) }} />}{titleRun
                ? <span style={{ display: 'flex', alignItems: 'baseline', gap: 4, flex: 'none', minWidth: '100%', maxWidth: titleRun, pointerEvents: 'none' }}><span style={eventCardTitle}>{event.summary || t('calendar.untitled')}</span>{timeLabel && <strong style={eventCardTime}>{timeLabel}</strong>}</span>
                : <><span style={titleVisible ? eventCardTitle : { ...eventCardTitle, visibility: 'hidden' }}>{event.summary || t('calendar.untitled')}</span>{timeLabel && <strong style={timedCard ? eventCardTimedTime : eventCardTime}>{timeLabel}</strong>}</>}</button>
            {resizable && !segment?.continuesFrom && <div className="calendar-event-resize-handle calendar-allday-resize-handle" data-testid="calendar-month-resize-start" onMouseDown={mouseDownEvent => { mouseDownEvent.stopPropagation(); monthResize.startResize(event, eventLastInclusiveDay, dayFromKey(startKey)); }} />}
            {resizable && !segment?.continuesTo && <div className="calendar-event-resize-handle calendar-allday-resize-handle" data-testid="calendar-month-resize-end" style={{ left: 'auto', right: 0 }} onMouseDown={mouseDownEvent => { mouseDownEvent.stopPropagation(); monthResize.startResize(event, dayFromKey(startKey), eventLastInclusiveDay); }} />}
          </div>;
        };
        // Unlike week view's all-day-row drag target (an otherwise-empty div with no children
        // to guard against), a month cell has children of its own — the date-chip button and
        // the event stack — so `onClick`/`onDoubleClick` still only fire onSelectDay/openCreate
        // when the section itself was the actual target, not a child with its own click/
        // double-click behavior. `onMouseDown` no longer carries that same guard: it's the one
        // week view's target never needed it on either, and month view's own drag lifecycle
        // (startDrag records the anchor in a ref, doesn't publish anything until a real
        // extendDrag follows) already makes a mousedown that lands on a child — the date chip,
        // an event card without its own stopPropagation — harmless on its own.
        return <section key={day.toDateString()} className="cal-cell" data-selected={day.toDateString() === anchor.toDateString()} onClick={event => { if (event.target === event.currentTarget) onSelectDay(day); }} onDoubleClick={event => { if (!(event.target as HTMLElement).closest('button, .calendar-event-resize-handle')) openCreate(day, day, true); }} onMouseDown={() => dayDrag.startDrag(day)} onMouseEnter={() => { dayDrag.extendDrag(day); monthEventDrag.enterDay(day); monthResize.enterDay(day); }} style={{ ...monthCell, ...(isWeekend(day) ? weekendCell : {}), ...(inMonth ? {} : outCell), ...(dayDrag.inRange(day) ? monthCellDragging : {}) }}>
          <button type="button" className="calendar-day-select" aria-label={day.toLocaleDateString(locale, { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' })} aria-pressed={day.toDateString() === anchor.toDateString()} onClick={() => onSelectDay(day)} style={{ ...dateChip, ...(isToday(day) ? dateChipToday : {}) }}>{day.getDate()}</button>
          <div style={eventStack}>{visibleList.map((item, itemIndex) => {
            // A blank spacer for a lane skipped on this particular day (see laneRows above) —
            // keeps every occupied lane at the same fixed visual row as its neighbors instead of
            // the empty one silently compacting away and shifting everything below it up.
            // `eventRow` alone has no height of its own (an empty flex container with nothing
            // inside collapses to 0px) — mirroring the real/preview rows' actual structure
            // (`eventRow` wrapping an `eventCard`), just invisible, guarantees this spacer takes
            // up exactly the same height a real one-line event row does, not a rounding-error
            // sliver of it.
            if ('empty' in item) return <div aria-hidden="true" key={`empty-lane-${itemIndex}`} style={eventRow}><div style={{ ...eventCard, visibility: 'hidden' }}>&nbsp;</div></div>;
            // A plain (non-spanning) `others` entry standing in for the preview of a dragged
            // timed/single-day event — it never shares a day with a ghost of its own (a timed
            // event moves to a genuinely different day), so it renders exactly like the lane
            // preview overlay below, just never stacked over anything.
            if ('preview' in item && item.preview === true) {
              return <MonthDragRow key={`preview-${item.event.id}`} segment={item.segment!} event={item.event} testId={moving ? 'calendar-month-move-preview' : 'calendar-month-resize-preview'} t={t} titleVisible={item.titleVisible!} />;
            }
            if ('event' in item) return renderRealEventCard(item.event);
            // A lane entry: the real (dimmed, if this is the dragged event) ghost, the preview,
            // or both stacked directly on top of each other — same lane, same row, instead of
            // the ghost losing whatever days the preview also reaches (which read as the event
            // itself shrinking) or the two competing for separate rows (which read as the bar
            // breaking in two).
            const previewNode = item.preview && <MonthDragRow key={`preview-${item.preview.event.id}`} segment={item.preview.segment} event={item.preview.event} testId={moving ? 'calendar-month-move-preview' : 'calendar-month-resize-preview'} t={t} titleVisible={item.preview.titleVisible} />;
            if (item.ghost && item.preview) {
              return <div key={`lane-${itemIndex}`} style={{ position: 'relative' }}>
                {renderRealEventCard(item.ghost)}
                <div aria-hidden="true" style={{ position: 'absolute', inset: 0, zIndex: 2 }}>{previewNode}</div>
              </div>;
            }
            if (item.ghost) return renderRealEventCard(item.ghost);
            return previewNode;
          })}{hiddenCount > 0 && <button type="button" className="calendar-more" onClick={() => (onOpenDayAgenda ?? onSelectDay)(day)} style={eventMoreChip}>{t('calendar.moreEvents', { n: hiddenCount })}</button>}</div>
        </section>;
      })}
    </div>
  </div>;
}

function timeToMinutes(value: unknown) {
  const [hours, minutes] = String(value || '09:00').split(':').map(Number);
  return (Number.isFinite(hours) ? hours : 9) * 60 + (Number.isFinite(minutes) ? minutes : 0);
}

function TimeGrid({ days, dayEventsFor, view, isMobile, locale, openCreate, openTimedRangeCreate, moveEvent, openEdit, openContextMenu, onSelectDay, anchor, t, calendarWorkHoursStart, calendarWorkHoursEnd }: CalendarGridProps) {
  const scroller = useRef<HTMLDivElement | null>(null);
  const centeredGridKey = useRef<string | null>(null);
  // Headroom above the 00:00 line, in minutes-equivalent px (1px = 1 minute in this
  // grid). It's added to the column's own height and to every top offset within it,
  // instead of padding on the row that contains the columns — padding there painted
  // in the row's own background, not each column's (time axis vs. a weekend tint),
  // leaving a mismatched strip above 00:00. The hour-line background gradient is
  // shifted down by the same amount (see timeColumn's backgroundPosition below) so
  // the drawn lines still land exactly on each hour boundary.
  const TOP_PADDING = 9;
  // Dragging across the all-day row's day columns creates a multi-day event
  // spanning exactly the days dragged over — see useDayRangeDrag above.
  const dayDrag = useDayRangeDrag((start, end) => openCreate(start, end));
  // Dragging inside the hourly grid — vertically within a day or across day columns —
  // creates a timed event at exact clock times, snapped to 15-minute steps. See
  // useHourGridDrag above.
  const hourDrag = useHourGridDrag(openTimedRangeCreate, TOP_PADDING, 15);
  // Dragging an existing timed event reschedules it — vertically in 15-minute steps,
  // horizontally in whole days. See useEventDragMove above.
  const eventDrag = useEventDragMove(moveEvent);
  // Dragging an event's top or bottom edge resizes it, same two axes. See
  // useEventResizeDrag above.
  const eventResize = useEventResizeDrag(moveEvent, TOP_PADDING, 15);
  // All-day pills move (day-granularity, no time axis) and resize (left/right edge extends
  // the span) the same way the hourly grid's timed events do. See useEventDragMove and
  // useAllDayResizeDrag above.
  const allDayDrag = useEventDragMove(moveEvent, { vertical: false });
  const allDayResize = useAllDayResizeDrag(moveEvent);
  // Navigating to a different period makes any drag against the *old* one meaningless — hard
  // clear all five regardless of whether anything else already would have. See CalendarGrid's
  // identical rangeKey effect for the reasoning (and why it isn't `days` itself).
  const rangeKey = `${days[0]?.toDateString()}|${days[days.length - 1]?.toDateString()}`;
  useEffect(() => {
    dayDrag.reset();
    hourDrag.reset();
    eventDrag.reset();
    eventResize.reset();
    allDayDrag.reset();
    allDayResize.reset();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reset()s are stable per mount, not per render
  }, [rangeKey]);
  useEffect(() => {
    if (scroller.current) scroller.current.scrollTop = Math.max(0, Math.min(timeToMinutes(calendarWorkHoursStart), timeToMinutes(calendarWorkHoursEnd)) - 120);
  }, [calendarWorkHoursEnd, calendarWorkHoursStart, view]);
  const columns = `52px repeat(${days.length}, minmax(${isMobile ? 150 : 0}px, 1fr))`;
  const workHours = workHoursGeometry(calendarWorkHoursStart, calendarWorkHoursEnd);
  // All-day events get one compact row above the hour grid — same idea as the
  // month grid's spanning bars, but laid out once for the whole visible week
  // instead of per day, since a genuine CSS-grid column span (not a per-day chip
  // squared off at its edges) is what actually reads as one continuous block.
  // `dayEventsFor` is a per-day resolver, so the week's distinct events are
  // collected by id across the days they cover before laying them out.
  const weekAllDayEventsById = new Map<string, CalendarViewEvent>();
  for (const day of days) {
    for (const event of dayEventsFor(day)) {
      if ((event.all_day || event.allDay) && event.id != null) weekAllDayEventsById.set(String(event.id), event);
    }
  }
  const weekAllDay = layoutWeekAllDayEvents([...weekAllDayEventsById.values()], days);
  const weekAllDayLanes = Math.max(1, ...weekAllDay.map(placement => placement.lane + 1));
  // One scroll container owns both axes. Splitting them across nested elements (an outer
  // horizontal scroller around an inner vertical one) made touch panning stutter: a
  // gesture locks to a single container and axis, so every sideways drag had to be handed
  // off between the two, which read as the grid "catching" mid-swipe. A single container
  // pans in both directions natively.
  const gridKey = `${days.map(day => day.toDateString()).join('|')}|${view}|${isMobile}`;
  useLayoutEffect(() => {
    // Selecting a day inside the existing grid keeps its scroll position.
    if (centeredGridKey.current === gridKey) return;
    const container = scroller.current;
    if (!container?.clientWidth) return;
    const index = weekFocusIndex(days, anchor);
    if (index < 0) return;
    const column = container.querySelector(`[data-calendar-day-index="${index}"]`);
    if (!column) return;
    // Measure the column as laid out rather than deriving its width, so a wide screen
    // (where the columns share the available space) lands on 0 and is left untouched.
    const columnRect = column.getBoundingClientRect();
    const containerRect = container.getBoundingClientRect();
    container.scrollLeft = centeredScrollLeft({
      columnStart: columnRect.left - containerRect.left + container.scrollLeft,
      columnWidth: columnRect.width,
      viewportWidth: container.clientWidth,
      contentWidth: container.scrollWidth,
    });
    centeredGridKey.current = gridKey;
  }, [anchor, days, gridKey]);
  const showMenu = (event: CalendarViewEvent, target: React.MouseEvent<HTMLButtonElement>) => openContextMenu(event, target.clientX, target.clientY, target.currentTarget);
  const invokeMenu = (event: CalendarViewEvent, keyboardEvent: React.KeyboardEvent<HTMLButtonElement>) => {
    // Opening now needs a double-click from a mouse, but a button's native click still fires
    // on Enter/Space — that's the keyboard path's only way in, so it still opens straight to
    // edit rather than requiring a "double-press".
    if (keyboardEvent.key === 'Enter' || keyboardEvent.key === ' ') { keyboardEvent.preventDefault(); openEdit(event); return; }
    if (keyboardEvent.key !== 'ContextMenu' && !(keyboardEvent.shiftKey && keyboardEvent.key === 'F10')) return;
    keyboardEvent.preventDefault();
    const target = keyboardEvent.currentTarget;
    const rect = target.getBoundingClientRect();
    openContextMenu(event, rect.right, rect.bottom, target);
  };
  // Double-clicking a day column creates a one-hour event at the half-hour slot the
  // pointer landed in — the upper half of an hour (:00–:29) starts it at :00, the
  // lower half (:30–:59) at :30, not always at midnight. 1px in the column below the
  // headroom above corresponds to 1 minute (top: hour * 60).
  const openTimedCreate = (day: Date, event: React.MouseEvent<HTMLDivElement>) => {
    const rect = event.currentTarget.getBoundingClientRect();
    const rawMinutes = event.clientY - rect.top - TOP_PADDING;
    const minutes = Math.min(1410, Math.max(0, Math.floor(rawMinutes / 30) * 30));
    openCreate(new Date(day.getFullYear(), day.getMonth(), day.getDate(), 0, minutes));
  };
  return <div data-testid="calendar-grid" style={{ ...calendarSurface, flex: 1, minWidth: 0, minHeight: 0, display: 'flex', overflow: 'hidden' }}>
    <div
      data-testid="calendar-time-grid-scroll"
      ref={scroller}
      style={{ flex: 1, minWidth: 0, minHeight: 0, width: '100%', overflow: 'auto', overflowAnchor: 'none', overscrollBehavior: 'contain' }}
    >
      <div style={{ width: isMobile ? 52 + days.length * 150 : '100%', minWidth: isMobile ? 52 + days.length * 150 : 0 }}>
        {/* zIndex 5 — strictly above every absolutely-positioned thing that can scroll underneath
            it in the timed grid below (move/resize previews at 3, the now-line at 4). Sticky
            positioning alone doesn't guarantee staying on top: a sticky element and a later,
            equal-or-higher z-index sibling scrolled beneath it are just two boxes in the same
            stacking context, and CSS paints the later sibling on top wherever they overlap on
            screen — which a tall multi-day resize preview reaches every time it's dragged across
            a day boundary, since the days strictly between where the drag started and the pointer
            now sits each get a full 24-hour-tall preview block. Without a z-index clearly above
            every one of those, that block painted over this header and the all-day band on top of
            it instead of staying tucked beneath them once scrolled past. */}
        <div style={{ position: 'sticky', top: 0, zIndex: 5, background: 'var(--bg-secondary)' }}>
          <div style={{ display: 'grid', gridTemplateColumns: columns }}>
            <div style={timeAxisHeader} />{days.map((day, index) => <button type="button" key={day.toDateString()} data-calendar-day-index={index} data-calendar-today={isToday(day) ? 'true' : undefined} aria-label={day.toLocaleDateString(locale, { dateStyle: 'full' })} aria-pressed={day.toDateString() === anchor.toDateString()} onClick={() => onSelectDay(day)} style={{ ...dayHeader, borderTop: 0, borderLeft: 0, borderRight: 0, cursor: 'pointer' }}><span style={dayHeaderWeekday}>{day.toLocaleDateString(locale, { weekday: 'short' })}</span><strong style={{ ...dayHeaderDay, ...(isToday(day) ? todayDayChip : {}) }}>{day.getDate()}</strong></button>)}
          </div>
          {weekAllDay.length > 0 && (() => {
            // Only once a drag has actually crossed into a different day is there anywhere
            // new to preview — see the identical fix on month view's move-drag.
            const moving = allDayDrag.moving && allDayDrag.moving.deltaDays !== 0 ? allDayDrag.moving : null;
            const resizing = allDayResize.resize;
            const movePreview = moving ? (() => {
              const shift = (key: string) => { const d = dayFromKey(key); return new Date(d.getFullYear(), d.getMonth(), d.getDate() + moving.deltaDays); };
              const startKey = String(moving.event.starts_at ?? moving.event.startsAt ?? '').slice(0, 10);
              const endKey = String(moving.event.ends_at ?? moving.event.endsAt ?? '').slice(0, 10);
              const range = indexRangeInDays(days, shift(startKey), shift(endKey));
              return range && { ...range, summary: moving.event.summary, color: moving.event.calendar_color, lane: weekAllDay.find(placement => String(placement.event.id) === String(moving.event.id))?.lane ?? 0 };
            })() : null;
            const resizePreview = resizing ? (() => {
              const startDay = resizing.day <= resizing.fixed ? resizing.day : resizing.fixed;
              const lastInclusiveDay = resizing.day <= resizing.fixed ? resizing.fixed : resizing.day;
              const endExclusive = new Date(lastInclusiveDay.getFullYear(), lastInclusiveDay.getMonth(), lastInclusiveDay.getDate() + 1);
              const range = indexRangeInDays(days, startDay, endExclusive);
              return range && { ...range, summary: resizing.event.summary, color: resizing.event.calendar_color, lane: weekAllDay.find(placement => String(placement.event.id) === String(resizing.event.id))?.lane ?? 0 };
            })() : null;
            return <div style={{ display: 'grid', gridTemplateColumns: columns, gridAutoRows: 22, rowGap: 2, background: 'var(--bg-primary)', borderTop: '1px solid var(--border-subtle)', borderBottom: '1px solid var(--border-subtle)' }}>
              {/* Empty spacer, not a label — it just keeps this row's first column
                  aligned with the time axis below it, styled the same way. */}
              <div aria-hidden="true" style={{ gridColumn: '1 / 2', gridRow: `1 / ${weekAllDayLanes + 1}`, ...allDayRowLabel }} />
              {/* One drag target per day, under the pills (DOM order, no z-index needed) —
                  click-drag across these creates a multi-day all-day event; a plain click
                  or double-click on an empty spot still reaches the day header behind it
                  only via drag, so it also opens a quick single-day create on double-click.
                  Styled like a normal hour-row day column (same background/border), just
                  compact, so the row reads as part of the grid rather than separate chrome. */}
              {days.map((day, index) => <div key={day.toDateString()} onDoubleClick={() => openCreate(day)} onMouseDown={() => dayDrag.startDrag(day)} onMouseEnter={() => { dayDrag.extendDrag(day); allDayDrag.enterDay(day); allDayResize.enterDay(day); }} style={{ gridColumn: `${index + 2} / ${index + 3}`, gridRow: `1 / ${weekAllDayLanes + 1}`, borderRight: '1px solid var(--border-subtle)', ...(isWeekend(day) ? weekendColumn : {}), ...(dayDrag.inRange(day) ? monthCellDragging : {}) }} />)}
              {weekAllDay.map(({ event, startIndex, endIndex, lane, continuesFrom, continuesTo }) => {
                const movable = !event.read_only && isWritableCalendarSource(event.source);
                const beingMoved = moving != null && String(moving.event.id) === String(event.id);
                const beingResized = resizing != null && String(resizing.event.id) === String(event.id);
                const startKey = String(event.starts_at ?? event.startsAt ?? '').slice(0, 10);
                const endKey = String(event.ends_at ?? event.endsAt ?? '').slice(0, 10);
                const exclusiveEndDay = dayFromKey(endKey);
                const lastInclusiveDay = new Date(exclusiveEndDay.getFullYear(), exclusiveEndDay.getMonth(), exclusiveEndDay.getDate() - 1);
                return <button
                  key={event.id}
                  type="button"
                  className="cal-ev"
                  data-testid="calendar-allday-pill"
                  onMouseDown={mouseDownEvent => { if (movable) { mouseDownEvent.stopPropagation(); allDayDrag.startMove(event, dayFromKey(startKey), mouseDownEvent); } }}
                  onDoubleClick={doubleClickEvent => { doubleClickEvent.stopPropagation(); openEdit(event); }}
                  onContextMenu={keyboardEvent => { keyboardEvent.preventDefault(); showMenu(event, keyboardEvent); }}
                  onKeyDown={keyboardEvent => invokeMenu(event, keyboardEvent)}
                  title={event.read_only ? t('calendar.readOnly') : t('calendar.edit')}
                  style={{ ...allDayPill, position: 'relative', zIndex: 1, gridColumn: `${startIndex + 2} / ${endIndex + 2}`, gridRow: lane + 1, background: eventFill(event, beingMoved || beingResized ? EVENT_DIM_RATIO : EVENT_OPACITY), cursor: movable ? 'grab' : 'pointer', borderTopLeftRadius: continuesFrom ? 0 : 4, borderBottomLeftRadius: continuesFrom ? 0 : 4, borderTopRightRadius: continuesTo ? 0 : 4, borderBottomRightRadius: continuesTo ? 0 : 4 }}
                >{movable && !continuesFrom && <div className="calendar-event-resize-handle calendar-allday-resize-handle" data-testid="calendar-allday-resize-start" onMouseDown={mouseDownEvent => { mouseDownEvent.stopPropagation(); allDayResize.startResize(event, lastInclusiveDay, dayFromKey(startKey)); }} />}{event.summary || t('calendar.untitled')}{movable && !continuesTo && <div className="calendar-event-resize-handle calendar-allday-resize-handle" data-testid="calendar-allday-resize-end" style={{ left: 'auto', right: 0 }} onMouseDown={mouseDownEvent => { mouseDownEvent.stopPropagation(); allDayResize.startResize(event, dayFromKey(startKey), lastInclusiveDay); }} />}</button>;
              })}
              {/* The real pill above is `position: relative` (so its own z-index takes effect),
                  but the preview below it in DOM order was left `position: static` — a
                  non-positioned element paints *below* any positioned one in the same stacking
                  context regardless of DOM order, so the dimmed real pill (positioned) was
                  rendering on top of the preview (not) the whole time a drag overlapped both.
                  Explicit `position: relative` plus a higher z-index than the real pill's fixes
                  it for both reasons at once, rather than relying on DOM order alone. */}
              {movePreview && <div aria-hidden="true" data-testid="calendar-allday-move-preview" style={{ ...allDayPill, position: 'relative', zIndex: 2, gridColumn: `${movePreview.startIndex + 2} / ${movePreview.endIndex + 2}`, gridRow: movePreview.lane + 1, background: eventTint(movePreview.color), pointerEvents: 'none' }}>{movePreview.summary || t('calendar.untitled')}</div>}
              {resizePreview && <div aria-hidden="true" data-testid="calendar-allday-resize-preview" style={{ ...allDayPill, position: 'relative', zIndex: 2, gridColumn: `${resizePreview.startIndex + 2} / ${resizePreview.endIndex + 2}`, gridRow: resizePreview.lane + 1, background: eventTint(resizePreview.color), pointerEvents: 'none' }}>{resizePreview.summary || t('calendar.untitled')}</div>}
            </div>;
          })()}
        </div>
        <div style={{ display: 'grid', gridTemplateColumns: columns }}>
          <div style={{ ...timeAxis, height: 1440 + TOP_PADDING }}>{Array.from({ length: 24 }, (_, hour) => <span key={hour} style={{ ...timeAxisSpan, top: hour * 60 + TOP_PADDING }}>{`${String(hour).padStart(2, '0')}:00`}</span>)}</div>
          {(() => {
            // The ghosts shown at an in-progress move's or resize's new position — computed
            // once outside the per-day loop as plain start/end instants, then matched against
            // each day as it renders (a multi-day drag needs a segment on every day it touches,
            // not just the one where it started).
            // Only once the drag has actually moved somewhere (vertically in minutes or
            // horizontally across a day) is there a new position to preview or an original to
            // dim — `startMove` publishes state immediately on mousedown (deltaDays: 0,
            // deltaMinutes: 0) so a hover cursor can already read as "grab", but treating that
            // zero-movement instant as a real move dimmed the event the moment it was pressed,
            // before any drag happened at all. Same reasoning as CalendarGrid's `moving` filter
            // above, just on two axes instead of one.
            const moving = eventDrag.moving && (eventDrag.moving.deltaDays !== 0 || eventDrag.moving.deltaMinutes !== 0) ? eventDrag.moving : null;
            const movePreview = moving ? (() => {
              const offset = moving.deltaDays * 86400000 + moving.deltaMinutes * 60000;
              return {
                start: new Date(new Date(moving.event.starts_at ?? 0).getTime() + offset),
                end: new Date(new Date(moving.event.ends_at ?? 0).getTime() + offset),
                summary: moving.event.summary,
                location: moving.event.location,
                color: moving.event.calendar_color,
              };
            })() : null;
            const resizing = eventResize.resize;
            const resizePreview = resizing ? (() => {
              const draggedInstant = resizing.day.getTime() + resizing.minutes * 60000;
              const fixedInstant = resizing.fixed.getTime();
              const minSpan = 15 * 60000;
              return {
                start: resizing.edge === 'start' ? new Date(Math.min(draggedInstant, fixedInstant - minSpan)) : resizing.fixed,
                end: resizing.edge === 'end' ? new Date(Math.max(draggedInstant, fixedInstant + minSpan)) : resizing.fixed,
                summary: resizing.event.summary,
                location: resizing.event.location,
                color: resizing.event.calendar_color,
              };
            })() : null;
            // A hypothetical version of the dragged/resized event at its preview instant, run
            // back through the exact same `layoutTimedEvents` a day's real events use (against
            // that day's real events, with the dragged one itself swapped out for this
            // hypothetical) — a fixed inset here instead rendered the preview at very close to
            // the full column width regardless of how many other events it actually overlaps,
            // which could be dramatically wider than the real event it's a preview of whenever
            // that real event shares its day with anything else.
            const moveLayoutEvent = moving && movePreview ? { ...moving.event, starts_at: iso(movePreview.start), ends_at: iso(movePreview.end) } : null;
            const resizeLayoutEvent = resizing && resizePreview ? { ...resizing.event, starts_at: iso(resizePreview.start), ends_at: iso(resizePreview.end) } : null;
            return days.map(day => {
              const dayEvents = dayEventsFor(day);
              const timed = layoutTimedEvents(dayEvents, day);
              const dragRange = hourDrag.rangeFor(day);
              const moveHere = movePreview && dayPreviewSegment(day, movePreview.start, movePreview.end);
              const resizeHere = resizePreview && dayPreviewSegment(day, resizePreview.start, resizePreview.end);
              const moveHereLayout = moveHere && moveLayoutEvent ? layoutTimedEvents([...dayEvents.filter(candidate => String(candidate.id) !== String(moveLayoutEvent.id)), moveLayoutEvent], day).find(item => String(item.event.id) === String(moveLayoutEvent.id)) : null;
              const resizeHereLayout = resizeHere && resizeLayoutEvent ? layoutTimedEvents([...dayEvents.filter(candidate => String(candidate.id) !== String(resizeLayoutEvent.id)), resizeLayoutEvent], day).find(item => String(item.event.id) === String(resizeLayoutEvent.id)) : null;
              return <div key={day.toDateString()} data-hour-column="true" onDoubleClick={event => openTimedCreate(day, event)} onMouseDown={event => hourDrag.startDrag(day, event)} onMouseEnter={() => { hourDrag.enterDay(day); eventDrag.enterDay(day); eventResize.enterDay(day); }} style={{ ...timeColumn, height: 1440 + TOP_PADDING, backgroundPosition: `0 ${TOP_PADDING}px`, ...(isWeekend(day) ? weekendColumn : {}) }}>{Array.from({ length: 24 }, (_, hour) => <i key={hour} style={{ top: hour * 60 + TOP_PADDING }} />)}<div aria-label={`${t('calendar.workHoursStart', 'Working hours start')} ${calendarWorkHoursStart} – ${t('calendar.workHoursEnd', 'Working hours end')} ${calendarWorkHoursEnd}`} data-testid="calendar-work-hours-boundary" style={{ ...workHoursBoundary, ...(isToday(day) ? workHoursBoundaryToday : {}), top: workHours.start + TOP_PADDING, height: Math.max(0, workHours.end - workHours.start) }} />{isToday(day) && <div aria-hidden="true" style={{ ...nowLine, top: nowMinutes() + TOP_PADDING }}><span style={nowLineDot} /></div>}{dragRange && <div aria-hidden="true" data-testid="calendar-time-drag-preview" style={{ ...timeDragPreview, top: dragRange.start + TOP_PADDING, height: Math.max(18, dragRange.end - dragRange.start) }} />}{moveHere && movePreview && <div aria-hidden="true" data-testid="calendar-event-move-preview" style={{ ...timedEvent, position: 'absolute', left: `calc(${(moveHereLayout?.column ?? 0) * 100 / (moveHereLayout?.columns ?? 1)}% + 3px)`, width: `calc(${100 / (moveHereLayout?.columns ?? 1)}% - 6px)`, zIndex: 3, top: moveHere.start + TOP_PADDING, height: Math.max(36, moveHere.end - moveHere.start), background: eventTint(movePreview.color, EVENT_GLASS_OPACITY, true), color: eventInk(movePreview.color), borderLeft: `3px solid ${movePreview.color || 'var(--accent)'}`, display: 'flex', flexDirection: 'column', pointerEvents: 'none' }}><TimedEventBody event={{ summary: movePreview.summary, starts_at: movePreview.start, location: movePreview.location }} t={t} /></div>}{resizeHere && resizePreview && <div aria-hidden="true" data-testid="calendar-event-resize-preview" style={{ ...timedEvent, position: 'absolute', left: `calc(${(resizeHereLayout?.column ?? 0) * 100 / (resizeHereLayout?.columns ?? 1)}% + 3px)`, width: `calc(${100 / (resizeHereLayout?.columns ?? 1)}% - 6px)`, zIndex: 3, top: resizeHere.start + TOP_PADDING, height: Math.max(36, resizeHere.end - resizeHere.start), background: eventTint(resizePreview.color, EVENT_GLASS_OPACITY, true), color: eventInk(resizePreview.color), borderLeft: `3px solid ${resizePreview.color || 'var(--accent)'}`, display: 'flex', flexDirection: 'column', pointerEvents: 'none' }}><TimedEventBody event={{ summary: resizePreview.summary, starts_at: resizePreview.start, location: resizePreview.location }} t={t} /></div>}{timed.map(({ event, geometry, column, columns: count }) => {
                const movable = !event.read_only && isWritableCalendarSource(event.source);
                // Reads the filtered `moving`/`resizing` above (zero-movement filtered out for
                // `moving`), not the hooks' raw state directly — the raw `eventDrag.moving` is
                // already non-null the instant mousedown fires, which dimmed this button before
                // any actual drag movement happened.
                const beingMoved = moving != null && String(moving.event.id) === String(event.id);
                const beingResized = resizing != null && String(resizing.event.id) === String(event.id);
                return <div key={event.id} style={{ ...timedEvent, top: geometry.start + TOP_PADDING, height: Math.max(36, geometry.end - geometry.start), left: `calc(${column * 100 / count}% + 3px)`, width: `calc(${100 / count}% - 6px)`, padding: 0, display: 'flex', overflow: 'visible' }}><button className="cal-ev" onMouseDown={mouseDownEvent => { if (movable) { mouseDownEvent.stopPropagation(); eventDrag.startMove(event, day, mouseDownEvent); } }} onDoubleClick={doubleClickEvent => { doubleClickEvent.stopPropagation(); openEdit(event); }} onContextMenu={keyboardEvent => { keyboardEvent.preventDefault(); showMenu(event, keyboardEvent); }} onKeyDown={keyboardEvent => invokeMenu(event, keyboardEvent)} title={event.read_only ? t('calendar.readOnly') : t('calendar.edit')} style={{ ...timedEvent, position: 'absolute', inset: 0, width: '100%', height: '100%', background: eventFill(event, beingMoved || beingResized ? EVENT_GLASS_DIM_RATIO : EVENT_GLASS_OPACITY, true), color: eventInk(event.calendar_color), borderLeft: `3px solid ${event.calendar_color || 'var(--accent)'}`, cursor: movable ? 'grab' : 'default', display: 'flex', flexDirection: 'column' }}>{movable && <div className="calendar-event-resize-handle" data-testid="calendar-event-resize-top" onMouseDown={mouseDownEvent => { mouseDownEvent.stopPropagation(); eventResize.startResize(event, 'start', new Date(event.ends_at ?? 0), day, mouseDownEvent); }} />}<TimedEventBody event={event} t={t} />{movable && <div className="calendar-event-resize-handle" data-testid="calendar-event-resize-bottom" style={{ top: 'auto', bottom: 0 }} onMouseDown={mouseDownEvent => { mouseDownEvent.stopPropagation(); eventResize.startResize(event, 'end', new Date(event.starts_at ?? 0), day, mouseDownEvent); }} />}</button></div>;
              })}</div>;
            });
          })()}
        </div>
      </div>
    </div>
  </div>;
}

function EventDialog({ form, ownAddresses, error, calendars, accounts, saving, readOnly, onOpenSourceMessage, onRespond, onChange, onAllDayChange, onSave, onDelete, onClose, t, fullScreen = false, attendeeStatuses }: EventDialogProps) {
  const [attendeeDraft, setAttendeeDraft] = useState('');
  const statusOf = (chip: string) => <AttendeeStatus status={attendeeStatuses?.get(chipAddress(chip).toLowerCase())} t={t} />;
  const setAttendees = (chips: string[]) => onChange('attendees', [...new Set(chips.map(chipAddress).filter(Boolean))]);
  const locked = saving || readOnly;
  // Read-only events came from a source Inboxora never writes to (or a calendar
  // the user hasn't opted into writing to), so the description is rendered exactly
  // like a message body — the same sanitized, script-free iframe RichTextEditor
  // can't reproduce — instead of an editor nobody can actually use.
  const descriptionBody = useMemo(() => (readOnly ? calendarDescriptionBody(form.description) : null), [readOnly, form.description]);
  const sourceUrl = readOnly ? safeHttpUrl(typeof form.url === 'string' ? form.url : null) : null;
  const organizer = typeof form.organizer === 'string' ? form.organizer : '';
  const sourceMessageId = typeof form.source_message_id === 'string' ? form.source_message_id : null;
  // The user's own attendee entry is listed first and can't be removed. On an invitation
  // imported from a mail, clicking it opens Accept / Maybe / Decline; the chosen answer then
  // waits on whether to tell the organizer, like the mail's invitation card asks.
  const isOwn = (chip: string) => ownAddresses.has(chipAddress(chip).toLowerCase());
  const ownChip = form.attendees.find(isOwn) ?? null;
  const attendeeChips = ownChip ? [ownChip, ...form.attendees.filter(chip => chip !== ownChip)] : form.attendees;
  const canReply = form.mode === 'edit' && Boolean(form.inviteLocked && sourceMessageId && onRespond && ownChip);
  const myReply = attendeeStatuses?.get(chipAddress(ownChip ?? '').toLowerCase()) ?? (typeof form.my_partstat === 'string' ? form.my_partstat : null);
  const [replyMenu, setReplyMenu] = useState<{ x: number; y: number } | null>(null);
  const [replying, setReplying] = useState<InvitationAnswer | null>(null);
  useEffect(() => {
    if (!replyMenu) return;
    const close = (event: Event) => { if (!(event.target instanceof Element && event.target.closest('[data-testid="calendar-reply-menu"]'))) setReplyMenu(null); };
    const onKey = (event: KeyboardEvent) => { if (event.key === 'Escape') setReplyMenu(null); };
    document.addEventListener('pointerdown', close);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('pointerdown', close); document.removeEventListener('keydown', onKey); };
  }, [replyMenu]);
  const openReplyMenu = canReply && !saving ? (chip: string, x: number, y: number) => { if (isOwn(chip)) setReplyMenu({ x: Math.min(x, window.innerWidth - 200), y: Math.min(y, window.innerHeight - 140) }); } : undefined;
  const attendeeProps = { lockedChip: isOwn, onChipClick: openReplyMenu };
  const keepOwn = (chips: string[]) => setAttendees(ownChip && !chips.some(isOwn) ? [ownChip, ...chips] : chips);
  return <Dialog title={form.mode === 'edit' ? t('calendar.editEvent') : t('calendar.newEvent')} closeLabel={t('calendar.close')} onClose={onClose} busy={saving} testId="calendar-event-dialog" className={fullScreen ? 'calendar-event-dialog-full ui-fullscreen' : ''} footer={readOnly ? <>
    {/* No separate "Close" button here — the dialog's own × already closes it,
        exactly like the read-only preview this replaced never had one either. */}
    <div>{sourceMessageId && <Button data-testid="calendar-open-source-message" onClick={onOpenSourceMessage}>{t('calendar.openSourceMessage')}</Button>}</div>
    <div />
  </> : <>
    <div>{form.mode === 'edit' && <Button variant="danger" disabled={saving} onClick={onDelete}>{t('calendar.delete')}</Button>}</div>
    <div style={{ display: 'flex', gap: 8 }}><Button disabled={saving} onClick={onClose}>{t('calendar.cancel')}</Button><Button variant="primary" disabled={saving} onClick={onSave}>{saving ? t('calendar.saving') : form.invitationError ? t('calendar.retrySave') : t('calendar.save')}</Button></div>
  </>}>
    <div className="ui-form">
      {error && <div role="alert" className="ui-alert">{error}</div>}
      {readOnly && <span className="calendar-readonly">{t('calendar.readOnly')}</span>}
      <label>{t('calendar.titleField')}<input autoFocus={!readOnly} disabled={locked} value={form.summary} onChange={e => onChange('summary', e.target.value)} /></label>
      <label className="ui-check"><input type="checkbox" disabled={locked} checked={form.allDay} onChange={e => onAllDayChange(e.target.checked)} />{t('calendar.allDay')}</label>
      <div className="ui-form-columns"><label>{t('calendar.starts')}<input disabled={locked} type={form.allDay ? 'date' : 'datetime-local'} value={form.startsAt} onChange={e => onChange('startsAt', e.target.value)} /></label><label>{t('calendar.ends')}<input disabled={locked} type={form.allDay ? 'date' : 'datetime-local'} min={form.allDay ? form.startsAt : undefined} value={form.allDay ? shiftDateKey(form.endsAt, -1) : form.endsAt} onChange={e => onChange('endsAt', form.allDay ? shiftDateKey(e.target.value, 1) : e.target.value)} /></label></div>
      <label>{t('calendar.calendar')}<select disabled={locked} value={form.calendarId} onChange={e => onChange('calendarId', e.target.value)}>{calendars.map(calendar => <option key={calendar.id} value={calendar.id}>{calendar.name}</option>)}</select></label>
      <label>{t('calendar.location')}<input disabled={locked} value={form.location} onChange={e => onChange('location', e.target.value)} /></label>
      {readOnly ? <>
        {(descriptionBody?.html || descriptionBody?.text) && <div className="calendar-description"><span className="calendar-description-label">{t('calendar.description')}</span><div className="calendar-event-description" data-testid="calendar-event-description-body"><MessageBodyRenderer {...descriptionBody} title={t('calendar.description')} showQuotedTextLabel={t('conversation.showQuotedText')} hideQuotedTextLabel={t('conversation.hideQuotedText')} /></div></div>}
        {sourceUrl && <p><a href={sourceUrl} target="_blank" rel="noopener noreferrer">{form.url}</a></p>}
        {form.attendees.length > 0 && <div className="calendar-attendees"><span className="calendar-description-label">{t('calendar.attendees')}</span>
          <ChipInput chips={attendeeChips} onChipsChange={() => {}} value="" onChange={() => {}} disabled chipAdornment={statusOf} testId="calendar-attendees" {...attendeeProps} />
        </div>}
        {organizer && <p>{t('calendar.organizer')}: {organizer}</p>}
      </> : <>
        <div className="calendar-description"><span className="calendar-description-label">{t('calendar.description')}</span><RichTextEditor value={form.description} onChange={(html: string) => onChange('description', html)} placeholder={t('calendar.descriptionPlaceholder')} label={t('calendar.description')} testId="calendar-event-description" /></div>
        <div className="calendar-invites ui-form">
          <div className="calendar-attendees"><span className="calendar-description-label">{t('calendar.attendees')}</span>
            <ChipInput chips={attendeeChips} onChipsChange={keepOwn} value={attendeeDraft} onChange={setAttendeeDraft} placeholder={t('calendar.attendeesPlaceholder')} getSuggestions={suggestAttendees} chipAdornment={statusOf} disabled={locked} testId="calendar-attendees" containerStyle={attendeeFieldStyle} inputStyle={attendeeInputStyle} {...attendeeProps} />
          </div>
          {!form.inviteLocked && <>
            <label>{t('calendar.senderAccount')}<select disabled={locked} value={form.inviteAccountId} onChange={e => onChange('inviteAccountId', e.target.value)}><option value="">{t('calendar.chooseSender')}</option>{accounts.map(account => <option key={account.id} value={account.id}>{account.name || account.email_address} · {account.email_address}</option>)}</select></label>
            {!accounts.length && <p>{t('calendar.noSenderAccounts')}</p>}
            <label className="ui-check"><input type="checkbox" data-testid="calendar-guests-see-list" disabled={locked} checked={!form.hideAttendees} onChange={e => onChange('hideAttendees', !e.target.checked)} />{t('calendar.guestsSeeList')}</label>
          </>}
        </div>
      </>}
    </div>
    {replyMenu && <div role="menu" data-testid="calendar-reply-menu" aria-label={t('calendar.yourReply')} className="app-menu" style={{ position: 'fixed', left: replyMenu.x, top: replyMenu.y, zIndex: 9900, minWidth: 180 }}>
      {([['ACCEPTED', t('calendar.invitationAccept')], ['TENTATIVE', t('calendar.invitationTentative')], ['DECLINED', t('calendar.invitationDecline')]] as const).map(([answer, label]) => <button key={answer} type="button" role="menuitemradio" aria-checked={myReply === answer} data-testid={`calendar-reply-${answer.toLowerCase()}`} className="app-menu-item" onClick={() => { setReplyMenu(null); setReplying(answer); }} style={myReply === answer ? { color: 'var(--accent)', fontWeight: 600 } : undefined}>
        <span aria-hidden="true" style={{ width: 14, display: 'inline-flex', justifyContent: 'center' }}>{myReply === answer ? '✓' : ''}</span>{label}
      </button>)}
    </div>}
    {replying && <Dialog title={t('calendar.invitationNotifyTitle')} closeLabel={t('common.close')} onClose={() => setReplying(null)} busy={saving} testId="calendar-reply-notify" footer={<>
      <Button disabled={saving} onClick={() => setReplying(null)}>{t('common.cancel')}</Button>
      <div style={{ display: 'flex', gap: 8 }}>
        <Button disabled={saving} data-testid="calendar-reply-notify-no" onClick={() => onRespond?.(replying, false)}>{t('calendar.invitationNotifyNo')}</Button>
        <Button variant="primary" autoFocus disabled={saving} data-testid="calendar-reply-notify-yes" onClick={() => onRespond?.(replying, true)}>{saving ? t('calendar.saving') : t('calendar.invitationNotifyYes')}</Button>
      </div>
    </>}><p style={{ margin: 0 }}>{t('calendar.invitationNotifyBody', { answer: ({ ACCEPTED: t('calendar.invitationAccept'), TENTATIVE: t('calendar.invitationTentative'), DECLINED: t('calendar.invitationDecline') } as const)[replying] })}</p></Dialog>}
  </Dialog>;
}


 // Nothing in the grid is prose meant to be selected and copied — every cell and event is a
 // click/double-click/drag target instead, and without this, dragging across day cells or
 // events (to create, move, or resize) also drags the browser's own native text selection
 // across the date numbers and event titles in between. That selection has nothing to do with
 // React state, so it doesn't clear when a drag's own highlight correctly does — it just sits
 // there, highlighted, until something else collapses it, reading as "the highlight didn't
 // reset" even once the drag state genuinely has.
 const calendarSurface: CSSProperties = { overflow: 'auto', display: 'flex', flexDirection: 'column', minHeight: 0, background: 'var(--bg-secondary)', userSelect: 'none', WebkitUserSelect: 'none' }; const dayGrid: CSSProperties = { display: 'grid', flex: 1, minWidth: 0 };
const MONTH_ROW_MIN = 108; const dayHeader: CSSProperties = { position: 'sticky', top: 0, zIndex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6, minWidth: 0, padding: '7px 6px', background: 'var(--bg-secondary)' }; const dayHeaderWeekday: CSSProperties = { fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 9.5, letterSpacing: '0.07em', textTransform: 'uppercase', color: 'var(--text-tertiary)' }; const dayHeaderDay: CSSProperties = { fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 14, fontWeight: 600, color: 'var(--text-primary)', lineHeight: 1 }; const todayDayChip: CSSProperties = { background: 'var(--accent)', color: 'var(--accent-text)', width: 26, height: 26, borderRadius: '50%', display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0 }; const monthCell: CSSProperties = { minWidth: 0, padding: 6, background: 'var(--bg-primary)', display: 'flex', flexDirection: 'column', gap: 2, minHeight: 108, boxSizing: 'border-box', cursor: 'pointer' }; const outCell: CSSProperties = { opacity: .5 }; const weekendCell: CSSProperties = { background: 'color-mix(in srgb, var(--bg-secondary) 55%, var(--bg-primary))' }; // `background` (the shorthand key), not `backgroundColor` — `monthCell` above always sets a
// `background` on the same element, and mixing the shorthand with a separate `backgroundColor`
// longhand across renders corrupts the DOM's inline style once the highlight is removed: setting
// `style.backgroundColor` overwrites just that longhand's internal CSSOM slot, discarding the
// value `background` had decomposed into it, and clearing it afterward (React removes a key no
// longer present in the style object) has nothing left to fall back to — the element ends up with
// no background at all (transparent) instead of reverting to `monthCell`'s. This was the actual
// cause of the drag-to-create highlight "never resetting": the cell wasn't stuck highlighted, its
// background was gone entirely, and the grid's own gap color showing through read as a leftover
// tint. Using the same key for both avoids ever having two different background properties fight
// over the same element.
const monthCellDragging: CSSProperties = { background: 'color-mix(in srgb, var(--accent) 16%, var(--bg-primary))' }; const dateChip: CSSProperties = { fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 11, color: 'var(--text-secondary)', marginBottom: 6, width: 20, height: 20, borderRadius: '50%', display: 'flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0 }; const dateChipToday: CSSProperties = { background: 'var(--accent)', color: 'var(--accent-text)', fontWeight: 600 }; const eventStack: CSSProperties = { display: 'grid', minWidth: 0, gap: 2 }; const eventRow: CSSProperties = { display: 'flex', minWidth: 0, gap: 2 }; const eventCard: CSSProperties = { display: 'flex', alignItems: 'baseline', gap: 4, minWidth: 0, flex: 1, width: '100%', textAlign: 'left', border: 0, borderRadius: 4, padding: '2px 6px', color: 'white', fontSize: 11, fontWeight: 700, lineHeight: 1.5 }; const eventCardTitle: CSSProperties = { flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }; const eventCardTimed: CSSProperties = { alignItems: 'center', gap: 6, color: 'var(--text-primary)', fontWeight: 500, paddingLeft: 2 }; const eventCardTimedTime: CSSProperties = { fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10, fontWeight: 400, color: 'var(--text-secondary)', flexShrink: 0 }; const eventCardBar: CSSProperties = { width: 4, alignSelf: 'stretch', minHeight: 12, borderRadius: 2, flexShrink: 0 }; const eventCardTime: CSSProperties = { fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 9.5, fontWeight: 400, opacity: .85, flexShrink: 0 };
const timeAxisHeader: CSSProperties = { borderRight: '1px solid var(--border-subtle)' }; const timeAxis: CSSProperties = { position: 'relative', height: 1440, borderRight: '1px solid var(--border-subtle)', background: 'var(--bg-tertiary)', color: 'var(--text-tertiary)', fontSize: 10 }; const timeAxisSpan: CSSProperties = { position: 'absolute', right: 8, transform: 'translateY(-50%)', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 9.5, color: 'var(--text-tertiary)' }; const timeColumn: CSSProperties = { position: 'relative', height: 1440, background: 'var(--bg-primary)', borderRight: '1px solid var(--border-subtle)', backgroundImage: 'repeating-linear-gradient(to bottom, transparent 0, transparent 59px, var(--border-subtle) 59px, var(--border-subtle) 60px)' }; // `background`, not `backgroundColor` — merged onto `timeColumn` above, which already sets a
// `background` of its own; see `monthCellDragging`'s comment for why mixing the two keys on the
// same element is fragile even when (as here) it isn't currently observable.
const weekendColumn: CSSProperties = { background: 'color-mix(in srgb, var(--bg-secondary) 55%, var(--bg-primary))' }; const timeDragPreview: CSSProperties = { position: 'absolute', left: 2, right: 2, zIndex: 2, borderRadius: 4, border: '1px dashed var(--accent)', background: 'color-mix(in srgb, var(--accent) 22%, transparent)', pointerEvents: 'none', boxSizing: 'border-box' };
const timedEvent: CSSProperties = { position: 'absolute', zIndex: 1, margin: 0, overflow: 'hidden', border: 0, borderRadius: 4, padding: '2px 6px', color: 'white', textAlign: 'left', fontSize: 11, lineHeight: 1.45, boxSizing: 'border-box' }; const timedEventTitle: CSSProperties = { display: 'block', fontWeight: 700, whiteSpace: 'normal', overflowWrap: 'break-word' }; const timedEventTimeRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 3, fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 9.5, fontWeight: 400, opacity: .85 }; const timedEventLoc: CSSProperties = { display: 'block', fontSize: 10, opacity: .8, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' };
// The attendee field reads like the composer's recipient row: one framed field with the
// chips and a borderless text input inside — not a second input box nested in the first.
const attendeeFieldStyle: CSSProperties = { border: '1px solid var(--border-subtle)', borderRadius: 6, padding: '2px 8px', background: 'var(--bg-primary)', minHeight: 34 };
const attendeeInputStyle: CSSProperties = { width: 'auto', padding: '5px 0', border: 'none', borderRadius: 0, background: 'transparent', outline: 'none', boxShadow: 'none', color: 'var(--text-primary)', fontSize: 13 };
const allDayRowLabel: CSSProperties = { background: 'var(--bg-tertiary)', borderRight: '1px solid var(--border-subtle)' };
const allDayPill: CSSProperties = { margin: '1px 2px', border: 0, borderRadius: 4, padding: '2px 6px', color: 'white', textAlign: 'left', fontSize: 11, fontWeight: 600, lineHeight: 1.45, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis', minWidth: 0 };
const workHoursBoundary: CSSProperties = { position: 'absolute', left: 0, right: 0, zIndex: 0, borderTop: '1px solid color-mix(in srgb, var(--accent) 25%, transparent)', borderBottom: '1px solid color-mix(in srgb, var(--accent) 25%, transparent)', background: 'color-mix(in srgb, var(--accent) 5%, transparent)', pointerEvents: 'none' }; const workHoursBoundaryToday: CSSProperties = { background: 'color-mix(in srgb, var(--accent) 7%, transparent)' };
const nowLine: CSSProperties = { position: 'absolute', left: 0, right: 0, height: 2, background: 'var(--red)', zIndex: 4, pointerEvents: 'none' }; const nowLineDot: CSSProperties = { position: 'absolute', left: -1, top: -3, width: 8, height: 8, borderRadius: '50%', background: 'var(--red)' };

const monthDow: CSSProperties = { display: 'grid', gridTemplateColumns: 'repeat(7, 1fr)', borderBottom: '1px solid var(--border-subtle)', background: 'var(--bg-secondary)' };
const monthDowCell: CSSProperties = { minWidth: 0, overflow: 'hidden' };
const monthDowLabel: CSSProperties = { display: 'block', padding: '7px 8px', fontFamily: 'var(--font-mono, ui-monospace, monospace)', fontSize: 10, letterSpacing: '0.08em', textTransform: 'uppercase', color: 'var(--text-tertiary)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' };
const eventMoreChip: CSSProperties = { display: 'block', width: '100%', textAlign: 'left', border: 0, borderRadius: 4, padding: '2px 6px', fontSize: 10, fontFamily: 'var(--font-mono, ui-monospace, monospace)', color: 'var(--text-secondary)', background: 'var(--bg-tertiary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', cursor: 'pointer' };
