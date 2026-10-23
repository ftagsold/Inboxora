// "Send later" picker: quick presets, a month calendar and a time field. Confirming hands the
// chosen local date-time to the composer, which submits it as the send's `sendAt`.

import { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Dialog } from './ui.tsx';
import {
  MIN_LEAD_MS, combine, dateKey, formatScheduled, monthGrid, monthLabel, schedulePresets, timeKey, weekdayLabels,
} from '../utils/scheduleSend.ts';

interface ScheduleSendDialogProps {
  onClose: () => void;
  onConfirm: (at: Date) => void;
}

export default function ScheduleSendDialog({ onClose, onConfirm }: ScheduleSendDialogProps) {
  const { t, i18n } = useTranslation();
  const locale = i18n.resolvedLanguage || i18n.language;
  const now = useMemo(() => new Date(), []);
  const presets = useMemo(() => schedulePresets(now), [now]);
  const initial = presets.find(p => p.key === 'tomorrowMorning')?.at ?? new Date(now.getTime() + 24 * 3600e3);
  const [day, setDay] = useState(dateKey(initial));
  const [time, setTime] = useState(timeKey(initial));
  const [month, setMonth] = useState(() => new Date(initial.getFullYear(), initial.getMonth(), 1));

  const chosen = combine(day, time);
  const tooSoon = !!chosen && chosen.getTime() < Date.now() + MIN_LEAD_MS;
  const today = dateKey(now);
  const weekdays = useMemo(() => weekdayLabels(locale), [locale]);
  const thisMonth = new Date(now.getFullYear(), now.getMonth(), 1);
  const shiftMonth = (by: number) => setMonth(m => new Date(m.getFullYear(), m.getMonth() + by, 1));

  return (
    <Dialog
      title={t('compose.schedule.title')}
      closeLabel={t('common.close')}
      onClose={onClose}
      className="schedule-dialog"
      testId="schedule-send-dialog"
      footer={<>
        <span className="schedule-summary" aria-live="polite">
          {chosen && !tooSoon ? t('compose.schedule.summary', { when: formatScheduled(chosen, locale) }) : tooSoon ? t('compose.schedule.tooSoon') : ''}
        </span>
        <div style={{ display: 'flex', gap: 8 }}>
          <Button onClick={onClose}>{t('common.cancel')}</Button>
          <Button
            variant="primary"
            data-testid="schedule-send-confirm"
            disabled={!chosen || tooSoon}
            onClick={() => { if (chosen && !tooSoon) onConfirm(chosen); }}
          >
            {t('compose.schedule.confirm')}
          </Button>
        </div>
      </>}
    >
      <div className="schedule-body">
        <div className="schedule-presets" role="list">
          {presets.map(p => (
            <button
              key={p.key}
              type="button"
              role="listitem"
              className="schedule-preset"
              onClick={() => onConfirm(p.at)}
            >
              <span>{t(`compose.schedule.presets.${p.key}`)}</span>
              <span className="schedule-preset-time">{formatScheduled(p.at, locale)}</span>
            </button>
          ))}
        </div>

        <div className="schedule-picker">
          <div className="schedule-month">
            <button type="button" className="schedule-nav" aria-label={t('compose.schedule.prevMonth')} disabled={month <= thisMonth} onClick={() => shiftMonth(-1)}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2"><polyline points="15 18 9 12 15 6" /></svg>
            </button>
            <span className="schedule-month-label">{monthLabel(month, locale)}</span>
            <button type="button" className="schedule-nav" aria-label={t('compose.schedule.nextMonth')} onClick={() => shiftMonth(1)}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2"><polyline points="9 18 15 12 9 6" /></svg>
            </button>
          </div>
          <div className="schedule-grid" role="grid">
            {weekdays.map(w => <span key={w} className="schedule-weekday">{w}</span>)}
            {monthGrid(month).map(d => {
              const key = dateKey(d);
              const past = key < today;
              const outside = d.getMonth() !== month.getMonth();
              return (
                <button
                  key={key}
                  type="button"
                  className={`schedule-day${key === day ? ' is-selected' : ''}${key === today ? ' is-today' : ''}${outside ? ' is-outside' : ''}`}
                  disabled={past}
                  aria-pressed={key === day}
                  onClick={() => { setDay(key); if (outside) setMonth(new Date(d.getFullYear(), d.getMonth(), 1)); }}
                >
                  {d.getDate()}
                </button>
              );
            })}
          </div>
          <label className="schedule-time">
            <span>{t('compose.schedule.time')}</span>
            <input type="time" value={time} onChange={e => setTime(e.target.value)} data-testid="schedule-send-time" />
          </label>
        </div>
      </div>
    </Dialog>
  );
}
