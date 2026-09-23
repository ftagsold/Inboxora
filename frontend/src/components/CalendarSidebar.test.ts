import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const source = () => readFile(new URL('./CalendarSidebar.tsx', import.meta.url), 'utf8');

describe('CalendarSidebar contract', () => {
  it('provides a mini-month and visible calendar toggles', async () => {
    const component = await source();
    assert.match(component, /data-testid="calendar-mini-month"/);
    assert.match(component, /data-testid="calendar-mini-month-previous"/);
    assert.match(component, /data-testid="calendar-mini-month-next"/);
    assert.match(component, /onShiftMonth\?\.\(-1\)/);
    assert.match(component, /onShiftMonth\?\.\(1\)/);
    assert.match(component, /data-testid="calendar-visibility-toggle"/);
  });

  it('keeps per-calendar rename/color/delete management', async () => {
    const component = await source();
    assert.match(component, /calendar\.rename/);
    assert.match(component, /calendar\.changeColor/);
    assert.match(component, /calendar\.deleteCalendar/);
    assert.match(component, /testId="calendar-appearance-dialog"/);
  });

  it('has no remote source management entry point — that moved to Settings → DAV servers', async () => {
    // The pane used to render its own "Manage sources" button and an inline
    // add/edit/discover dialog for CalDAV/ICS sources. That whole workflow now
    // lives in the unified DavServersSettings tab, so none of it belongs here.
    const component = await source();
    assert.doesNotMatch(component, /data-testid="calendar-sidebar-manage-sources"/);
    assert.doesNotMatch(component, /calendar\.manageSources/);
    assert.doesNotMatch(component, /discoverSource|api\.calendar\.createSource|api\.calendar\.deleteSource/);
    assert.doesNotMatch(component, /sourcePanelRequest/);
  });

  it('shows a flat calendar list with no local/external grouping or badges', async () => {
    // Confirmed product decision: drop the "Kalender" section heading, the
    // "Quelle"/owned badges, and the my-calendars/source-calendar group headings
    // entirely — just checkboxes, colors and names.
    const component = await source();
    assert.doesNotMatch(component, /calendar\.calendars\b/);
    assert.doesNotMatch(component, /calendar\.myCalendars/);
    assert.doesNotMatch(component, /calendar\.sourceCalendar/);
    assert.doesNotMatch(component, /calendar\.owned\b/);
  });

  it('leaves closing to the one shared sheet header control', async () => {
    // The panel used to render its own "Zamknij" button next to the dialog's ×.
    // The bottom sheet header now owns the single close affordance, so the rail
    // neither renders that button nor accepts an onClose prop.
    const component = await source();
    assert.doesNotMatch(component, /calendar-sidebar-close/);
    assert.doesNotMatch(component, /canCreate, onClose/);
  });

  it('keeps application-wide calendar preferences out of the calendar rail', async () => {
    const component = await source();
    assert.doesNotMatch(component, /calendar\.firstDayOfWeek/);
    assert.doesNotMatch(component, /calendar\.mobileNavigation/);
  });
});
