import { Button, Dialog } from './ui.tsx';
import type { TFunction } from 'i18next';

// Saving an edit to one occurrence of a series is not the same operation as editing the
// series itself, and — same reasoning as CalendarDeleteScopeDialog — that's not something a
// single Save click can express on its own. The backend only supports 'single' (an exception
// on this occurrence) or the whole series (no recurrenceId, PATCHes the base event); unlike
// delete, there is no "this and following" edit, since a `RECURRENCE-ID` exception can't
// retroactively change the rule the series itself expands from.
export default function CalendarEditScopeDialog({ busy = false, onSelect, onClose, t }: {
  busy?: boolean;
  onSelect: (scope: 'single' | 'all') => void;
  onClose: () => void;
  t: TFunction;
}) {
  return <Dialog
    title={t('calendar.editRecurringTitle')}
    closeLabel={t('calendar.close')}
    onClose={onClose}
    busy={busy}
    testId="calendar-edit-scope-dialog"
    footer={<Button disabled={busy} onClick={onClose}>{t('calendar.cancel')}</Button>}
  >
    <div className="ui-form">
      <p>{t('calendar.editRecurringBody')}</p>
      <div className="calendar-scope-actions">
        <Button disabled={busy} data-testid="calendar-edit-scope-single" onClick={() => onSelect('single')}>{t('calendar.editScopeSingle')}</Button>
        <Button variant="primary" disabled={busy} data-testid="calendar-edit-scope-all" onClick={() => onSelect('all')}>{t('calendar.editScopeAll')}</Button>
      </div>
    </div>
  </Dialog>;
}
