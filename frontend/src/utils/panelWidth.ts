// Side-panel widths for the workspace.
//
// Mail, Contacts and the calendar rail each show a list beside their content,
// but each is its own page with its own independently persisted width
// (`--list-width` for mail, `--contacts-list-width` for contacts,
// `--calendar-rail-width` for the calendar rail): dragging one page's resize
// handle no longer resizes the others.
//
// The calendar day agenda was already independent of the (then-shared) list
// width for the same reason it stays independent of the calendar rail now: it
// is a supplementary right-hand column, not a list of records, so widening it
// must not narrow/stretch any list column.

export const PANEL_WIDTH_MIN = 180;
export const PANEL_WIDTH_MAX = 700;
export const PANEL_WIDTH_DEFAULT = 360;
// Pre-existing key/variable: mail keeps its original name (and every user's
// saved width) since it's also the layout-preset system's own default channel
// (see layouts.ts/App.tsx) — only Contacts and the calendar rail are new.
export const PANEL_WIDTH_STORAGE_KEY = 'mailflow_list_width';

export const CONTACTS_LIST_WIDTH_STORAGE_KEY = 'mailflow_contacts_list_width';
export const CALENDAR_RAIL_WIDTH_DEFAULT = 320;
export const CALENDAR_RAIL_WIDTH_STORAGE_KEY = 'mailflow_calendar_rail_width';

export const AGENDA_WIDTH_DEFAULT = 296;
export const AGENDA_WIDTH_MIN = 200;
export const AGENDA_WIDTH_MAX = 560;
export const AGENDA_WIDTH_STORAGE_KEY = 'mailflow_agenda_width';

export function clampPanelWidth(value: unknown, { min = PANEL_WIDTH_MIN, max = PANEL_WIDTH_MAX }: { min?: number; max?: number } = {}): number | null {
  const width = Number(value);
  if (!Number.isFinite(width) || width <= 0) return null;
  return Math.round(Math.min(max, Math.max(min, width)));
}

// One independently persisted, CSS-variable-backed width.
interface WidthChannelOptions { variable: string; storageKey: string; fallback: number; min?: number; max?: number }

// Pointer event a resize handle receives (React's synthetic MouseEvent satisfies
// this shape) and the callbacks a handle may register.
interface PanelResizeHandleEvent { preventDefault?: () => void; clientX?: number }
interface PanelResizeOptions { edge?: string; onResize?: (width: number) => void; onEnd?: (width: number) => void }

function createWidthChannel({ variable, storageKey, fallback, min = PANEL_WIDTH_MIN, max = PANEL_WIDTH_MAX }: WidthChannelOptions) {
  const clamp = (value: unknown) => clampPanelWidth(value, { min, max });

  const readVariable = () => {
    if (typeof document === 'undefined') return null;
    // The variable is only ever written inline on <html>, so the inline value is
    // authoritative; the computed style is a fallback for a stylesheet default.
    const inline = clamp(parseFloat(document.documentElement.style.getPropertyValue(variable)));
    if (inline != null) return inline;
    if (typeof getComputedStyle !== 'function') return null;
    return clamp(parseFloat(getComputedStyle(document.documentElement).getPropertyValue(variable)));
  };

  const saved = () => {
    try {
      return clamp(localStorage.getItem(storageKey)) ?? undefined;
    } catch {
      return undefined;
    }
  };

  const read = () => readVariable() ?? saved() ?? fallback;

  const apply = (value: unknown) => {
    const clamped = clamp(value);
    if (clamped == null) return read();
    if (typeof document !== 'undefined') document.documentElement.style.setProperty(variable, `${clamped}px`);
    return clamped;
  };

  const persist = (value: unknown) => {
    const clamped = clamp(value);
    if (clamped == null) return read();
    try {
      localStorage.setItem(storageKey, String(clamped));
    } catch { /* a blocked storage must not break resizing */ }
    return clamped;
  };

  // Drag helper for this channel's handle. `edge` names the side of the panel the
  // handle sits on, so a handle on the right of a left-hand panel widens it while
  // a handle on the left of a right-hand panel widens it in the opposite
  // direction. Returns a cleanup function suitable for an effect teardown.
  const beginResize = (event: PanelResizeHandleEvent, { edge = 'right', onResize, onEnd }: PanelResizeOptions = {}) => {
    event?.preventDefault?.();
    const startX = event?.clientX ?? 0;
    const startWidth = read();
    const direction = edge === 'left' ? -1 : 1;
    const previousCursor = document.body.style.cursor;
    const previousUserSelect = document.body.style.userSelect;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';

    const onMouseMove = (move: MouseEvent) => {
      // Apply first, then notify. Writing the width must never depend on an
      // observer being registered — `onResize?.(apply(...))` would short-circuit
      // the whole argument list and silently drop every drag.
      const width = apply(startWidth + direction * (move.clientX - startX));
      onResize?.(width);
    };

    const onMouseUp = () => {
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousUserSelect;
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      // Persist BEFORE notifying. `onEnd?.(persist(...))` would short-circuit the
      // whole argument list when no listener is registered — which is the case for
      // every caller — so the width was applied but never written to storage, and
      // every reload snapped back to the default.
      const width = persist(read());
      onEnd?.(width);
    };

    document.addEventListener('mousemove', onMouseMove);
    document.addEventListener('mouseup', onMouseUp);

    return () => {
      document.removeEventListener('mousemove', onMouseMove);
      document.removeEventListener('mouseup', onMouseUp);
      document.body.style.cursor = previousCursor;
      document.body.style.userSelect = previousUserSelect;
    };
  };

  return { variable, storageKey, fallback, clamp, read, apply, persist, saved, beginResize };
}

// The mail list's own width — also the layout-preset system's channel.
export const listPanelWidth = createWidthChannel({
  variable: '--list-width',
  storageKey: PANEL_WIDTH_STORAGE_KEY,
  fallback: PANEL_WIDTH_DEFAULT,
});

// The contacts page's address list — independent of mail's.
export const contactsListPanelWidth = createWidthChannel({
  variable: '--contacts-list-width',
  storageKey: CONTACTS_LIST_WIDTH_STORAGE_KEY,
  fallback: PANEL_WIDTH_DEFAULT,
});

// The calendar rail (mini-month + calendar list) — independent of both mail's
// list and the day agenda (agendaPanelWidth, below).
export const calendarRailPanelWidth = createWidthChannel({
  variable: '--calendar-rail-width',
  storageKey: CALENDAR_RAIL_WIDTH_STORAGE_KEY,
  fallback: CALENDAR_RAIL_WIDTH_DEFAULT,
});

// Dedicated width for the calendar day agenda.
export const agendaPanelWidth = createWidthChannel({
  variable: '--agenda-width',
  storageKey: AGENDA_WIDTH_STORAGE_KEY,
  fallback: AGENDA_WIDTH_DEFAULT,
  min: AGENDA_WIDTH_MIN,
  max: AGENDA_WIDTH_MAX,
});

// ── List-panel width (the historical API: one shared list column) ────────────

export const savedPanelWidth = () => listPanelWidth.saved();
export const readPanelWidth = () => listPanelWidth.read();
export const applyPanelWidth = (width: Parameters<typeof listPanelWidth.apply>[0]) => listPanelWidth.apply(width);
export const persistPanelWidth = (width: Parameters<typeof listPanelWidth.persist>[0]) => listPanelWidth.persist(width);
export const beginPanelResize = (event: PanelResizeHandleEvent, options?: PanelResizeOptions) => listPanelWidth.beginResize(event, options);

// ── Contacts list width ───────────────────────────────────────────────────────

export const savedContactsListWidth = () => contactsListPanelWidth.saved();
export const readContactsListWidth = () => contactsListPanelWidth.read();
export const applyContactsListWidth = (width: Parameters<typeof contactsListPanelWidth.apply>[0]) => contactsListPanelWidth.apply(width);
export const persistContactsListWidth = (width: Parameters<typeof contactsListPanelWidth.persist>[0]) => contactsListPanelWidth.persist(width);
export const beginContactsListResize = (event: PanelResizeHandleEvent, options?: PanelResizeOptions) => contactsListPanelWidth.beginResize(event, options);

// ── Calendar rail width ─────────────────────────────────────────────────────

export const savedCalendarRailWidth = () => calendarRailPanelWidth.saved();
export const readCalendarRailWidth = () => calendarRailPanelWidth.read();
export const applyCalendarRailWidth = (width: Parameters<typeof calendarRailPanelWidth.apply>[0]) => calendarRailPanelWidth.apply(width);
export const persistCalendarRailWidth = (width: Parameters<typeof calendarRailPanelWidth.persist>[0]) => calendarRailPanelWidth.persist(width);
export const beginCalendarRailResize = (event: PanelResizeHandleEvent, options?: PanelResizeOptions) => calendarRailPanelWidth.beginResize(event, options);

// ── Day-agenda width (independent of the shared list column) ─────────────────

export const savedAgendaWidth = () => agendaPanelWidth.saved();
export const readAgendaWidth = () => agendaPanelWidth.read();
export const applyAgendaWidth = (width: Parameters<typeof agendaPanelWidth.apply>[0]) => agendaPanelWidth.apply(width);
export const persistAgendaWidth = (width: Parameters<typeof agendaPanelWidth.persist>[0]) => agendaPanelWidth.persist(width);
export const beginAgendaResize = (event: PanelResizeHandleEvent, options?: PanelResizeOptions) => agendaPanelWidth.beginResize(event, options);
