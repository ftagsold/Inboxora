import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { api } from '../utils/api.ts';
import { Button } from './ui.tsx';
import { formatInvitationRange } from '../utils/invitationTime.ts';
import { invitationConflicts } from '../utils/invitationConflicts.ts';
import type { ConflictCandidate } from '../utils/invitationConflicts.ts';
import { isWritableCalendarSource } from './calendarView.ts';
import { toAppError } from '../utils/errors.ts';
import type { CSSProperties } from 'react';

interface WritableCalendar {
  id: string;
  name: string;
  read_only: boolean;
  source: string;
}

type InvitationResponse = 'ACCEPTED' | 'TENTATIVE' | 'DECLINED';

// A mail invitation renders as one compact panel in the same register as the unsubscribe
// notice: the message already shows the title above and the body below, so the panel only
// carries what the message cannot — when the event is, what it collides with in the user's
// calendar, and the answer. Accept / Maybe reply to the organizer and put the event into the
// chosen calendar; Decline replies and removes the copy this message had added. Once the
// answer is sent the message has served its purpose and is archived (`onResponded`).
//
// Cancellation is actionable rather than informational: when the organizer retracts an
// invitation that was added, the copy this message created can be removed again.
export default function CalendarInvitationCard({ messageId, onResponded }: { messageId: string; onResponded?: () => void }) {
  const { t, i18n } = useTranslation();
  const [invitation, setInvitation] = useState<Record<string, unknown> | null>(null);
  const [calendars, setCalendars] = useState<WritableCalendar[]>([]);
  const [calendarId, setCalendarId] = useState('');
  const [error, setError] = useState<string | null>(null);
  // Why loading failed: 404 means no calendar data matches this message (event deleted, or the
  // attachment unreadable); anything else carries the server's reason.
  const [loadFailure, setLoadFailure] = useState<{ status?: number; reason: string } | null>(null);
  const [saving, setSaving] = useState<InvitationResponse | 'remove' | null>(null);
  // Derived from the server's `localEvent` / `response` so a reload shows the same state.
  const [localEvent, setLocalEvent] = useState<Record<string, unknown> | null>(null);
  const [response, setResponse] = useState<InvitationResponse | null>(null);
  const [removed, setRemoved] = useState(false);
  const [conflicts, setConflicts] = useState<ConflictCandidate[] | null>(null);
  const [retry, setRetry] = useState(0);

  useEffect(() => {
    let cancelled = false;
    setInvitation(null); setError(null); setLoadFailure(null); setRemoved(false); setLocalEvent(null); setResponse(null); setConflicts(null);
    Promise.all([api.calendar.getInvitation(messageId), api.calendar.listCalendars()]).then(([result, list]) => {
      if (cancelled) return;
      const writable = (list.calendars || []).filter((calendar: WritableCalendar) => !calendar.read_only && isWritableCalendarSource(calendar.source));
      const loaded = result.invitation;
      setInvitation(loaded);
      setLocalEvent(loaded?.localEvent || null);
      setResponse(loaded?.response || null);
      setCalendars(writable);
      setCalendarId(loaded?.localEvent?.calendarId || writable[0]?.id || '');
      // Collisions with the user's own calendar; a failed lookup simply shows none. (Only for a
      // request: a reply or a cancellation asks nothing of the user.)
      if (loaded?.method === 'REQUEST' && loaded.startsAt && loaded.endsAt) {
        api.calendar.listEvents(String(loaded.startsAt), String(loaded.endsAt))
          .then((events: { events?: ConflictCandidate[] }) => { if (!cancelled) setConflicts(invitationConflicts(loaded, events.events || [], messageId)); })
          .catch(() => { if (!cancelled) setConflicts([]); });
      }
    }).catch((err: unknown) => {
      if (cancelled) return;
      const e = toAppError(err);
      setLoadFailure({ status: e.status, reason: e.message });
      setError('load');
    });
    return () => { cancelled = true; };
  }, [messageId, retry]);

  const cancelledEvent = invitation?.method === 'CANCEL';
  const replyMessage = invitation?.method === 'REPLY';
  const added = Boolean(localEvent) && !removed;
  const when = invitation ? formatInvitationRange(invitation, i18n.language) : '';
  const busy = saving !== null;

  const notifyCalendarChanged = () => window.dispatchEvent(new Event('inboxora:calendar-changed'));

  const respond = async (answer: InvitationResponse) => {
    if (busy || (answer !== 'DECLINED' && !calendarId)) return;
    setSaving(answer); setError(null);
    try {
      const result = await api.calendar.respondInvitation(messageId, answer, answer === 'DECLINED' ? undefined : calendarId);
      setResponse(answer); setLocalEvent(result.localEvent || null); notifyCalendarChanged();
      onResponded?.();
    } catch { setError('reply'); }
    finally { setSaving(null); }
  };

  const remove = async () => {
    if (busy) return;
    setSaving('remove'); setError(null);
    try {
      await api.calendar.removeInvitation(messageId);
      setLocalEvent(null); setRemoved(true); notifyCalendarChanged();
    } catch { setError('save'); }
    finally { setSaving(null); }
  };

  if (error === 'load' && !invitation) {
    return <div className="msg-notice" data-testid="calendar-invitation-card" role="alert" style={noticeStyle}>
      <span style={{ flex: 1 }}>{loadFailure?.status === 404
        ? t('calendar.invitationNotFound')
        : t('calendar.invitationLoadFailedReason', { reason: loadFailure?.reason || t('calendar.invitationUnknownReason') })}</span>
      {loadFailure?.status !== 404 && <Button onClick={() => setRetry(value => value + 1)}>{t('common.retry')}</Button>}
    </div>;
  }
  if (!invitation) return null;

  if (replyMessage) {
    // An attendee answered an invitation this user sent; the answer is already recorded in the
    // user's own event (on sync, and again on opening), so this only reports it.
    const answers = Array.isArray(invitation.attendees) ? invitation.attendees as Array<{ email?: string; name?: string | null; partstat?: string }> : [];
    const replyLabel = (partstat?: string) => partstat === 'ACCEPTED' ? t('calendar.invitationReplyAccepted')
      : partstat === 'TENTATIVE' ? t('calendar.invitationReplyTentative')
        : partstat === 'DECLINED' ? t('calendar.invitationReplyDeclined')
          : t('calendar.invitationReplyPending');
    return <div className="msg-notice" data-testid="calendar-invitation-card" data-invitation-method="REPLY" style={{ ...noticeStyle, borderLeftColor: 'var(--accent)' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, flexWrap: 'wrap' }}>
        <span style={{ fontWeight: 500, color: 'var(--text-primary)' }}>{t('calendar.invitationReplyTitle')}</span>
        {when && <span data-testid="calendar-invitation-when">{when}</span>}
      </div>
      {answers.map(answer => <span key={answer.email} data-testid="calendar-invitation-reply" style={{ color: 'var(--text-primary)' }}>
        {answer.name || answer.email} · {replyLabel(answer.partstat)}
      </span>)}
      {invitation.recordedInCalendar === true
        ? <span role="status">{t('calendar.invitationReplyRecorded')}</span>
        : <span role="status" data-testid="calendar-invitation-reply-orphan">{t('calendar.invitationReplyNoEvent')}</span>}
    </div>;
  }

  const responseLabel: Record<InvitationResponse, string> = {
    ACCEPTED: t('calendar.invitationRespondedAccepted'),
    TENTATIVE: t('calendar.invitationRespondedTentative'),
    DECLINED: t('calendar.invitationRespondedDeclined'),
  };
  const answerButton = (answer: InvitationResponse, label: string) => <Button
    data-testid={`calendar-invitation-${answer.toLowerCase()}`}
    variant={response === answer ? 'primary' : 'default'}
    aria-pressed={response === answer}
    onClick={() => { void respond(answer); }}
    disabled={busy || (answer !== 'DECLINED' && !calendarId)}
  >{saving === answer ? t('calendar.saving') : label}</Button>;

  return <div className="msg-notice" data-testid="calendar-invitation-card" style={{ ...noticeStyle, borderLeftColor: 'var(--accent)' }}>
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, flexWrap: 'wrap' }}>
      <span style={{ fontWeight: 500, color: 'var(--text-primary)' }}>
        {cancelledEvent ? t('calendar.invitationCancelledTitle') : t('calendar.mailInvitation')}
      </span>
      {when && <span data-testid="calendar-invitation-when">{when}</span>}
      {response && !cancelledEvent && <span role="status" data-testid="calendar-invitation-response" style={{ marginLeft: 'auto', color: 'var(--text-primary)' }}>{responseLabel[response]}</span>}
    </div>

    {!cancelledEvent && conflicts && <div data-testid="calendar-invitation-conflicts" style={{ color: conflicts.length ? 'var(--amber)' : 'var(--text-tertiary)' }}>
      {conflicts.length
        ? <>
          <span>{t('calendar.invitationConflicts')}</span>
          <ul style={{ margin: '2px 0 0', paddingLeft: 18 }}>
            {conflicts.map(conflict => <li key={String(conflict.id)} data-testid="calendar-invitation-conflict" style={{ color: 'var(--text-secondary)' }}>
              <span style={{ color: 'var(--text-primary)' }}>{conflict.summary || t('calendar.untitled')}</span>
              {' · '}{formatInvitationRange({ startsAt: conflict.starts_at, endsAt: conflict.ends_at, allDay: conflict.all_day ?? conflict.allDay }, i18n.language)}
              {conflict.calendar_name ? ` · ${conflict.calendar_name}` : ''}
            </li>)}
          </ul>
        </>
        : t('calendar.invitationNoConflicts')}
    </div>}

    {cancelledEvent
      // A retracted invitation offers removal only for the copy it created; there is
      // nothing to add and nothing to reply to.
      ? (added
        ? <div style={actionRow}>
          <span role="status" style={{ flex: 1 }}>{t('calendar.invitationCancelled')}</span>
          <Button onClick={remove} disabled={busy}>{t(saving === 'remove' ? 'calendar.saving' : 'calendar.removeFromCalendar')}</Button>
        </div>
        : <span role="status">{t('calendar.invitationCancelledNotAdded')}</span>)
      : <div style={actionRow}>
        {added && !response && <span role="status">{t('calendar.invitationAlreadyAdded')}</span>}
        <select
          className="ui-select"
          aria-label={t('calendar.calendar')}
          value={calendarId}
          onChange={event => setCalendarId(event.target.value)}
          disabled={busy || !calendars.length}
          style={{ marginRight: 'auto' }}
        >
          {calendars.map(calendar => <option key={calendar.id} value={calendar.id}>{calendar.name}</option>)}
        </select>
        {answerButton('ACCEPTED', t('calendar.invitationAccept'))}
        {answerButton('TENTATIVE', t('calendar.invitationTentative'))}
        {answerButton('DECLINED', t('calendar.invitationDecline'))}
      </div>}

    {error === 'reply' && <span role="alert" style={{ color: 'var(--red)' }}>{t('calendar.invitationReplyFailed')}</span>}
    {error === 'save' && <span role="alert" style={{ color: 'var(--red)' }}>{t('calendar.invitationSaveFailed')}</span>}
  </div>;
}

// Matches the unsubscribe / blocked-images notice so the mail reader keeps one visual
// register for "this message needs one decision from you".
const noticeStyle: CSSProperties = {
  padding: '10px 14px', background: 'var(--bg-secondary)',
  border: '1px solid var(--border)', borderLeft: '3px solid var(--text-tertiary)',
  borderRadius: 8, display: 'flex', flexDirection: 'column', gap: 8,
  fontSize: 12, color: 'var(--text-secondary)',
};

const actionRow: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' };
