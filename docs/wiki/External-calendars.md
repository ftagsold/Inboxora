# External calendars

Inboxora subscribes to **CalDAV** and **ICS/webcal** calendars. ICS/webcal feeds are always
read-only (they're static files). CalDAV sources are read-only by default and can opt into
**two-way sync**, so local edits and deletions are pushed back to the server. Add one from
**Settings → Calendar → Calendar subscriptions** (a quick ICS/webcal add) or from the calendar
panel under **Manage sources**, which also covers CalDAV accounts, editing, and everything below.

## Source types

| Type | Needs | Use it for |
| --- | --- | --- |
| **CalDAV** | Server URL, remote username and password (or app password) | Nextcloud, Fastmail, Radicale, Synology and other CalDAV servers. |
| **ICS / webcal** | A calendar URL, or a `webcal://` link | Published `.ics` feeds, holiday calendars, team schedules. |

## Adding a CalDAV account

Paste the server's base URL (e.g. `https://cloud.example.com/`) along with the remote username and
password, and choose **Discover calendars**. Inboxora follows the standard CalDAV discovery chain —
`.well-known/caldav`, the account's principal, then its calendar home-set — so a plain server address
is usually enough; you don't need the exact collection URL a particular client would use.

If the account exposes more than one calendar, a checklist appears so you can pick which ones to
import; picking one calendar (the common case) skips the checklist entirely. Each imported calendar
becomes its own entry in the sidebar, named and coloured after the remote calendar.

**Allow editing** turns the source into a two-way sync: creating, editing, or deleting an event in
that calendar sends the change to the CalDAV server. It's off by default — turn it on only for a
calendar you're prepared to have Inboxora write to.

## Adding an ICS/webcal subscription

The fastest route is **Settings → Calendar → Calendar subscriptions**. Paste a name and the feed
URL and choose **Subscribe**. Inboxora accepts an `https://` link or the `webcal://` link a website
hands out; a `webcal://` link is converted to `https://` automatically. The source starts syncing
straight away and appears as a read-only calendar named after the entry you added. ICS/webcal
sources are never writable — they're a single static file, not an account.

### Holiday calendars

The **Public holidays** block in the same settings section adds a country's holidays as an ordinary
ICS subscription. Inboxora does **not** ship a holiday database: picking a country only resolves to
the matching read-only ICS feed published by the
[Thunderbird holiday calendar project](https://www.thunderbird.net/calendar/holidays/), and the
ordinary external-calendar sync pulls it like any other source. The calendar arrives as
**Holidays — <country>**, can be hidden or recoloured like any other calendar, and is refreshed
daily because holidays change at most once a year.

Inboxora deliberately does not generate holidays itself: moving feasts, statutory changes and
regional variants are exactly the kind of data the Thunderbird project already maintains and
updates. If a country is missing from the picker, add its `https://` or `webcal://` feed through
the subscription form above. Holiday calendars are subject to the same connection and credential
rules as every other source.

## Editing a source

Every source is editable after you add it, from its row in **Manage sources**: name, sync interval,
pausing sync without deleting the source, and (CalDAV only) credentials, the **Allow editing**
toggle, and which of the account's calendars are imported. Credential fields are left blank to keep
the stored value — Inboxora never displays a stored password back to you, so re-entering the server
address and credentials is also how you refresh the calendar list if a new one appears on the
account later.

Deselecting a calendar from a multi-calendar source removes its local copy; the remote calendar
itself is untouched, and it's picked up again if you re-select it.

## Sync schedule

Every source has its own interval, chosen when you add it and editable at any time from its row in
**Manage sources** — from every 15 seconds up to once a day. Pick it from how often the feed
actually changes: a busy shared team calendar is worth polling every 15–30 seconds, a public
holiday calendar is not (it defaults to once a day). A very short interval means more requests to
the remote server, so use it only where the remote calendar genuinely changes that often.

Changing the interval re-arms the schedule immediately — the server does not need a restart, and
the wait does not stay tied to the old interval. **Sync now** ignores the schedule and refreshes a
single source straight away.

## Behaviour

- ICS/webcal sources are **pull-only**; Inboxora never writes to them.
- CalDAV sources are pull-only unless **Allow editing** is turned on for that source, in which case
  local creates, edits, and deletes on its calendar(s) are pushed to the remote server.
- Two-way sync uses the calendar's ETag to detect a conflicting remote change. If the remote copy
  changed since Inboxora last saw it, the local write is **not** applied over it — Inboxora fetches
  the server's current version, shows that instead, and reports the conflict rather than silently
  overwriting either side.
- Each source keeps its own sync schedule (15 seconds to 24 hours, default hourly), and one source
  failing never blocks the others.
- Removing a source removes only the local copy; the remote calendar is untouched. On a writable
  source this includes events Inboxora already pushed there — deleting the source does not delete
  them from the server.
- Events removed from the remote feed are removed locally as well, so the local copy tracks the
  source.
- If a source returns something unusable, Inboxora keeps the last healthy copy and reports the
  problem instead of wiping your calendar. A genuinely empty calendar does clear the local copy.
- Each source shows its last sync time, last error category and any skipped entries.

## Credentials and safety

- Use a **dedicated remote username and app password** for CalDAV. Never reuse your Inboxora
  primary password, and never use your main account password for the remote service if it
  offers app passwords.
- Source credentials are encrypted at rest, are never returned by the API, and are not displayed
  again after you submit them.
- Error messages are redacted so a URL containing a secret cannot leak into the interface or a
  log.
- Source and discovered-calendar URLs are validated against the server connection policy: unsafe
  hosts are rejected, and public sources must use HTTPS. URLs that embed credentials are refused.
- `webcal://` links are converted to `https://`.

## Sharing one of your calendars as a link

The reverse direction is also supported: publish your own calendars as an anonymous read-only
`.ics` feed.

- Choose which owned calendars the feed contains and create a secret link.
- The link can be **rotated** (new secret, old link stops working) or **revoked** at any time.
- Failures are answered identically so the link cannot be probed for valid tokens, and repeated
  failures are rate-limited.
- The feed supports conditional requests, so subscribed clients refresh efficiently.

Anyone holding the link can read those calendars. Treat it like a password and rotate it if it
leaks.
