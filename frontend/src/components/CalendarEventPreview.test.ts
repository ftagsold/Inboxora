import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { describe, test } from 'node:test';

const source = (path: string): Promise<string> => readFile(new URL(path, import.meta.url), 'utf8');

describe('Calendar event dialog contract', () => {
  test('clicking an event opens the edit dialog directly, no separate preview step', async () => {
    const calendar = await source('./CalendarPage.tsx');
    // The grid, the agenda list and the context menu all reach the same openEdit —
    // there is no intermediate preview dialog to click "Edit" out of first.
    assert.match(calendar, /openCreate=\{openCreate\}[^>]*openEdit=\{openEdit\}/);
    assert.match(calendar, /onOpen: openEdit/);
    assert.match(calendar, /onEdit=\{\(\) => openEdit\(contextMenu\.event\)\}/);
    assert.doesNotMatch(calendar, /setPreview/);
    assert.doesNotMatch(calendar, /const \[preview, setPreview\]/);
  });

  test('a read-only event disables every field instead of opening a separate dialog', async () => {
    const calendar = await source('./CalendarPage.tsx');
    // Writability isn't local-only: a CalDAV calendar opted into two-way sync is
    // editable too, via the same shared helper the grid and context menu share.
    assert.match(calendar, /const formReadOnly = form \? !writable\.some\(calendar => calendar\.id === form\.calendarId\) : false;/);
    assert.match(calendar, /readOnly=\{formReadOnly\}/);
    // The calendar picker needs the full list (not just the writable ones) to
    // display a read-only event's actual calendar, even disabled.
    assert.match(calendar, /calendars=\{formReadOnly \? calendars : writable\}/);
    const dialog = await source('./CalendarPage.tsx');
    assert.match(dialog, /disabled=\{locked\}/);
    assert.match(dialog, /const locked = saving \|\| readOnly;/);
    assert.match(dialog, /\{readOnly && <span className="calendar-readonly">\{t\('calendar\.readOnly'\)\}<\/span>\}/);
    // Save/Delete are replaced with just Close when the fields are locked.
    assert.match(dialog, /footer=\{readOnly \? <>/);
  });

  test('the description renders through the message body renderer when read-only, the rich editor otherwise', async () => {
    const calendar = await source('./CalendarPage.tsx');
    assert.match(calendar, /calendarDescriptionBody\(form\.description\)/);
    assert.match(calendar, /data-testid="calendar-event-description-body"><MessageBodyRenderer \{\.\.\.descriptionBody\}/);
    assert.match(calendar, /<RichTextEditor value=\{form\.description\}/);
  });

  test('a mail-derived event links back to the message it came from', async () => {
    const calendar = await source('./CalendarPage.tsx');
    assert.match(calendar, /sourceMessageId && <Button data-testid="calendar-open-source-message"/);
    // The message may live in another account or folder and may not be in the loaded
    // list, so it is fetched and published as a one-message thread before selecting
    // it — selecting an unknown id would open a blank reader.
    assert.match(calendar, /await openDeepLinkMessage\(messageId, \{[\s\S]*?getMessage: async \(id\) => \{[\s\S]*?api\.getMessage\(id\)[\s\S]*?isStoreMessageRow\(message\)[\s\S]*?setThreadMessages: \(key, messages\) => \{[\s\S]*?setThreadMessages\(key, storeMessages\)[\s\S]*?setSelectedMessage,[\s\S]*?\}\);/);
    assert.match(calendar, /if \(sourceAccountId\) setSelectedAccount\(sourceAccountId, sourceFolder \|\| 'INBOX'\);/);
    assert.match(calendar, /setShowCalendar\(false\)/);
  });

  test('the event dialog is full screen on a phone', async () => {
    const calendar = await source('./CalendarPage.tsx');
    assert.match(calendar, /fullScreen=\{isMobile\}/);
    const css = await source('../ui.css');
    assert.match(css, /\.ui-dialog\.ui-fullscreen \{[\s\S]*?height: 100dvh/);
  });
});
