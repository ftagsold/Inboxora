// Which of the user's own events a received invitation would collide with.

export interface ConflictCandidate {
  id?: unknown;
  summary?: string | null;
  starts_at?: string | null;
  ends_at?: string | null;
  all_day?: boolean | null;
  allDay?: boolean | null;
  calendar_id?: string | null;
  calendar_name?: string | null;
  source_message_id?: string | null;
}

const CONTACT_CALENDAR_ID = 'contacts-birthdays';

const time = (value: unknown) => {
  const parsed = new Date(String(value ?? ''));
  return Number.isNaN(parsed.getTime()) ? null : parsed.getTime();
};

/**
 * Events overlapping the invitation's time range. Excluded: the copy this very invitation
 * already added (it is not a conflict with itself), contact birthdays/anniversaries, and —
 * for a timed invitation — all-day events, which mark a day rather than occupy its hours.
 */
export function invitationConflicts(
  invitation: { startsAt?: unknown; endsAt?: unknown; allDay?: unknown },
  events: readonly ConflictCandidate[],
  messageId: string,
): ConflictCandidate[] {
  const start = time(invitation.startsAt);
  const end = time(invitation.endsAt);
  if (start == null || end == null || end <= start) return [];
  const timedInvitation = !invitation.allDay;
  return events
    .filter(event => {
      if (event.source_message_id && event.source_message_id === messageId) return false;
      if (event.calendar_id === CONTACT_CALENDAR_ID) return false;
      if (timedInvitation && (event.all_day || event.allDay)) return false;
      const eventStart = time(event.starts_at);
      const eventEnd = time(event.ends_at);
      return eventStart != null && eventEnd != null && eventStart < end && eventEnd > start;
    })
    .sort((left, right) => (time(left.starts_at) ?? 0) - (time(right.starts_at) ?? 0));
}
