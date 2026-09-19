import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');

test('calendar settings offer an ICS/webcal subscription form and a holiday picker', async () => {
  const [admin, component] = await Promise.all([
    source('./AdminPanel.tsx'),
    source('./CalendarSubscriptionsSettings.tsx'),
  ]);

  // The section lives in Settings → Calendar, not only in the visibility panel.
  assert.match(admin, /<CalendarSubscriptionsSettings locale=/);
  assert.match(admin, /adminTab === 'calendar' && <CalendarSettingsTab/);

  // Subscribing by URL reuses the one external-source mechanism the panel already uses.
  assert.match(component, /api\.calendar\.createSource\(\{ kind: 'ical_url'/);
  assert.match(component, /normalizeSubscriptionUrl\(form\.url\)/);
  assert.match(component, /data-testid="calendar-subscriptions-settings"/);
  assert.match(component, /data-testid="calendar-subscription-row"/);

  // Country presets point at the maintained Thunderbird feeds instead of a local holiday engine.
  assert.match(component, /holidayCalendarUrl\(entry\.file\)/);
  assert.match(component, /HOLIDAY_SYNC_INTERVAL_SECONDS/);
  assert.match(component, /calendar\.holidayName/);
  // Adding a subscription refreshes a calendar that is already mounted behind the dialog.
  assert.match(component, /inboxora:calendar-changed/);
});

test('week view shows all-day events in one compact row above the hour grid, not a full-height band', async () => {
  const [calendar, view] = await Promise.all([
    source('./CalendarPage.tsx'),
    source('./calendarView.ts'),
  ]);

  // A multi-day event is one CSS-grid item spanning the day columns it covers —
  // laid out once for the whole visible week — not a chip repeated per day it
  // touches, which is what made the "full-height band down the day" design
  // possible in the first place.
  assert.match(view, /export function layoutWeekAllDayEvents/);
  assert.match(calendar, /layoutWeekAllDayEvents\(\[\.\.\.weekAllDayEventsById\.values\(\)\], days\)/);
  assert.match(calendar, /gridColumn: `\$\{startIndex \+ 2\} \/ \$\{endIndex \+ 2\}`/);
  assert.doesNotMatch(calendar, /calendar-allday-band/);
  // No leftover full-day-height styling for it.
  assert.doesNotMatch(calendar, /height: 1440px.*calendar-allday/);
});
