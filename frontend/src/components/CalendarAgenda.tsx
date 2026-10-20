import { agendaDays, sortedDayEvents, sortEventsByRecency } from './calendarView.ts';
import type { TFunction } from 'i18next';
import type { CalendarViewEvent } from './calendarView';
import { EmptyState } from './ui.tsx';

function formatEventTime(value: CalendarViewEvent['starts_at'], locale: string): string {
  if (value instanceof Date) return value.toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit' });
  if (typeof value === 'string' || typeof value === 'number') {
    return new Date(value).toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit' });
  }
  return '';
}

// Search results span many different days, unlike a single day/month agenda where the date
// is already implied by the section it's grouped under — so a search entry additionally
// shows its own date, short enough to sit above the time in the same narrow column.
function formatEventDate(value: CalendarViewEvent['starts_at'], locale: string): string {
  if (value instanceof Date) return value.toLocaleDateString(locale, { day: 'numeric', month: 'short' });
  if (typeof value === 'string' || typeof value === 'number') {
    return new Date(value).toLocaleDateString(locale, { day: 'numeric', month: 'short' });
  }
  return '';
}

function isReadOnly(value: unknown): boolean {
  return value === true;
}

function AgendaEntries({ events, locale, onOpen, t, showDate = false }: {
  events: CalendarViewEvent[];
  locale: string;
  onOpen: (event: CalendarViewEvent) => void;
  t: TFunction;
  showDate?: boolean;
}) {
  return events.map(event => <button key={event.id} type="button" className="calendar-agenda-entry" onClick={() => onOpen(event)}>
    <span className="calendar-agenda-color" style={{ background: event.calendar_color || 'var(--accent)' }} />
    <span className="calendar-agenda-time">
      {showDate && <><strong>{formatEventDate(event.starts_at, locale)}</strong><br /></>}
      {event.all_day || event.allDay ? t('calendar.allDay') : <>{formatEventTime(event.starts_at, locale)}<br />{formatEventTime(event.ends_at, locale)}</>}
    </span>
    <span className="calendar-agenda-text"><strong>{event.summary || t('calendar.untitled')}</strong>{event.location && <small>{event.location}</small>}{isReadOnly(event.read_only) && <small>{t('calendar.readOnly')}</small>}</span>
  </button>);
}

export default function CalendarAgenda({ events, anchor, locale, onOpen, t, monthly = false, searchQuery = '', searchResults = null, searchError = null, onClearSearch }: {
  events: CalendarViewEvent[];
  anchor: Date;
  locale: string;
  onOpen: (event: CalendarViewEvent) => void;
  t: TFunction;
  monthly?: boolean;
  // A non-empty searchQuery switches the whole pane to a flat, cross-date result list —
  // taking over from the normal day/month agenda above regardless of `monthly`, since a
  // search has no single day or month of its own to group by. searchResults is null while
  // the debounced request is still in flight (or hasn't fired yet), and a real (possibly
  // empty) array once it resolves — the two states render differently (loading vs. "no
  // results"), so they are never collapsed into one "falsy" check.
  searchQuery?: string;
  searchResults?: CalendarViewEvent[] | null;
  searchError?: string | null;
  onClearSearch?: () => void;
}) {
  if (searchQuery) {
    const loading = searchResults == null && !searchError;
    // Most recent first — see sortEventsByRecency for why this, specifically, reads as
    // "descending" for a list with no single day to unfold through.
    const entries = searchResults ? sortEventsByRecency(searchResults) : [];
    return <div data-testid="calendar-agenda-search-results" className="calendar-search-results">
      <div className="calendar-agenda-search-header">
        <p className="calendar-agenda-count">
          {searchError || (loading ? t('calendar.loading') : t('calendar.searchResultsCount', { count: entries.length, query: searchQuery }))}
        </p>
        {onClearSearch && <button type="button" className="calendar-agenda-clear-search" onClick={onClearSearch}>{t('calendar.clearSearch')}</button>}
      </div>
      {!loading && !searchError && (entries.length
        ? <AgendaEntries events={entries} locale={locale} onOpen={onOpen} t={t} showDate />
        : <EmptyState title={t('calendar.noSearchResults', { query: searchQuery })} />)}
    </div>;
  }
  if (monthly) {
    const groups = agendaDays(events, anchor);
    return <div data-testid="calendar-agenda-view" className="calendar-month-agenda">
      {groups.length ? groups.map(({ day, events: entries }) => <section key={day.toISOString()}>
        <h3>{day.toLocaleDateString(locale, { weekday: 'long', day: 'numeric', month: 'long' })}</h3>
        <AgendaEntries events={entries} locale={locale} onOpen={onOpen} t={t} />
      </section>) : <EmptyState title={t('calendar.emptyMonth')} />}
    </div>;
  }
  const entries = sortedDayEvents(events, anchor);
  return <div data-testid="calendar-day-agenda">
    <h2>{anchor.toLocaleDateString(locale, { weekday: 'long', day: 'numeric', month: 'long' })}</h2>
    <p className="calendar-agenda-count">{t('calendar.eventCount', { count: entries.length })}</p>
    {entries.length ? <AgendaEntries events={entries} locale={locale} onOpen={onOpen} t={t} /> : <EmptyState title={t('calendar.emptyDay')} />}
  </div>;
}
