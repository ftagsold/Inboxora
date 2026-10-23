// Date helpers for scheduled send: quick presets, the month grid of the picker and labels.
// All values are local wall-clock times; the request carries them as ISO (UTC) strings.

import { intlLocale } from './intlLocale.ts';

/** The server rejects anything due sooner than this (it would be sent right away). */
export const MIN_LEAD_MS = 60 * 1000;

export interface SchedulePreset { key: 'laterToday' | 'tomorrowMorning' | 'tomorrowAfternoon' | 'monday'; at: Date }

function at(base: Date, dayOffset: number, hour: number, minute = 0): Date {
  const d = new Date(base);
  d.setDate(d.getDate() + dayOffset);
  d.setHours(hour, minute, 0, 0);
  return d;
}

/** Quick choices: in about three hours (today only), tomorrow 08:00 and 13:00, next Monday 08:00. */
export function schedulePresets(now: Date = new Date()): SchedulePreset[] {
  const presets: SchedulePreset[] = [];
  const later = new Date(now.getTime() + 3 * 60 * 60 * 1000);
  later.setMinutes(later.getMinutes() < 30 ? 30 : 60, 0, 0);
  if (later.toDateString() === now.toDateString() && later.getHours() < 21) presets.push({ key: 'laterToday', at: later });
  presets.push({ key: 'tomorrowMorning', at: at(now, 1, 8) });
  presets.push({ key: 'tomorrowAfternoon', at: at(now, 1, 13) });
  // getDay(): 0 = Sunday … 1 = Monday. Always the coming Monday, a week ahead on Mondays.
  const toMonday = ((8 - now.getDay()) % 7) || 7;
  if (toMonday > 1) presets.push({ key: 'monday', at: at(now, toMonday, 8) });
  return presets;
}

/** `YYYY-MM-DD` / `HH:MM` of a local date, as date and time inputs use them. */
export function dateKey(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}
export function timeKey(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** The local date at `day` (`YYYY-MM-DD`) and `time` (`HH:MM`), or null when either is invalid. */
export function combine(day: string, time: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day);
  const t = /^(\d{1,2}):(\d{2})$/.exec(time);
  if (!m || !t) return null;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]), Number(t[1]), Number(t[2]), 0, 0);
  return Number.isFinite(d.getTime()) ? d : null;
}

/** Six Monday-first weeks covering the month of `month` (any day in it). */
export function monthGrid(month: Date): Date[] {
  const first = new Date(month.getFullYear(), month.getMonth(), 1);
  const offset = (first.getDay() + 6) % 7;
  return Array.from({ length: 42 }, (_, i) => new Date(first.getFullYear(), first.getMonth(), 1 - offset + i));
}

/** Short weekday names, Monday first. */
export function weekdayLabels(locale: string | null | undefined): string[] {
  const f = new Intl.DateTimeFormat(intlLocale(locale), { weekday: 'short' });
  // 2024-01-01 was a Monday.
  return Array.from({ length: 7 }, (_, i) => f.format(new Date(2024, 0, 1 + i)));
}

export function monthLabel(d: Date, locale: string | null | undefined): string {
  return new Intl.DateTimeFormat(intlLocale(locale), { month: 'long', year: 'numeric' }).format(d);
}

/** "Mon, Oct 26, 08:00": the scheduled time as rows and the dialog show it. */
export function formatScheduled(value: Date | string | null | undefined, locale: string | null | undefined): string {
  if (!value) return '';
  const d = value instanceof Date ? value : new Date(value);
  if (!Number.isFinite(d.getTime())) return '';
  return new Intl.DateTimeFormat(intlLocale(locale), {
    weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit',
    ...(d.getFullYear() !== new Date().getFullYear() ? { year: 'numeric' } : {}),
  }).format(d);
}
