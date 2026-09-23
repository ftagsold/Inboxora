import type { TFunction } from 'i18next';
import type { CSSProperties } from 'react';

// Sync cadence for one external calendar. The server accepts any whole number of
// seconds between 15 and 86400, so the control offers the values people actually
// choose (down to 15 seconds for a busy shared calendar) rather than a free-text
// field that invites typos the server would reject. Shared by the calendar panel's
// Manage Sources dialog and Settings → Calendar → Calendar subscriptions.
const SYNC_INTERVALS = [15, 30, 60, 300, 900, 1800, 3600, 10800, 21600, 43200, 86400];

export default function CalendarSourceIntervalSelect({ label, value, onChange, t }: { label: string; value?: number; onChange: (value: number) => void; t: TFunction }) {
  const known = value != null && SYNC_INTERVALS.includes(value);
  const format = (seconds: number) => (seconds < 60
    ? t('calendar.sourceSyncSeconds', { count: seconds })
    : seconds % 3600 === 0
      ? t('calendar.sourceSyncHours', { count: seconds / 3600 })
      : t('calendar.sourceSyncMinutes', { count: seconds / 60 }));
  return <label style={intervalLabel}>{label}
    <select
      className="ui-select"
      data-testid="calendar-source-interval"
      value={known ? value : ''}
      onChange={event => onChange(Number(event.target.value))}
    >
      {/* A value set outside this list (an older custom value) stays selectable so
          opening the dialog never silently rewrites it. */}
      {!known && <option value="">{format(value || 3600)}</option>}
      {SYNC_INTERVALS.map(seconds => <option key={seconds} value={seconds}>{format(seconds)}</option>)}
    </select>
  </label>;
}

const intervalLabel: CSSProperties = { display: 'flex', alignItems: 'center', gap: 6, marginTop: 6, fontSize: 11, color: 'var(--text-secondary)' };
