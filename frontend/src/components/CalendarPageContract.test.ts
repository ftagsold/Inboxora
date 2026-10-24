import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import test from 'node:test';

const calendarPath = new URL('./CalendarPage.tsx', import.meta.url);
const calendarCssPath = new URL('./calendar.css', import.meta.url);
const indexCssPath = new URL('../index.css', import.meta.url);
const sidebarPath = new URL('./CalendarSidebar.tsx', import.meta.url);
const localesPath = new URL('../locales/', import.meta.url);

test('calendar events expose context-menu invocation without per-event action buttons', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /onContextMenu=\{event => \{ event\.preventDefault\(\)/);
  assert.match(source, /keyboardEvent\.key !== 'ContextMenu'/);
  assert.match(source, /keyboardEvent\.shiftKey && keyboardEvent\.key === 'F10'/);
  // The ⋮ button was redundant: tapping an event already reaches edit and delete
  // through the preview, so no per-event action button may come back.
  assert.doesNotMatch(source, /data-testid="calendar-event-actions"/);
  assert.doesNotMatch(source, /eventActionButton/);
  // The context menu itself stays available (right-click / Shift+F10 / long-press).
  assert.match(source, /<CalendarContextMenu/);
  // Writability isn't local-only: a CalDAV calendar the user opted into two-way sync
  // for is editable too, via the same shared helper the context menu and grid share.
  assert.match(source, /isWritableCalendarSource\(event\.source\)/);
  assert.match(source, /<TimeGrid[^>]*openContextMenu=\{openContextMenu\}/);
  assert.match(source, /weekAllDay[\s\S]*onContextMenu/);
});

test('time grid re-anchors on a view change without resetting same-view manual scrolling', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /<TimeGrid[^>]*view=\{view\}/);
  assert.match(source, /function TimeGrid\([^)]*view[^)]*\)/);
  assert.match(source, /\}, \[calendarWorkHoursEnd, calendarWorkHoursStart, view\]\)/);
});

test('calendar renders one event dialog for an active form', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.equal(source.match(/\{form && <EventDialog/g)?.length, 1);
});

test('the visibility panel has no source management, but owned calendars expose safe actions', async () => {
  // Remote CalDAV/CardDAV source management moved to Settings → DAV servers
  // (DavServersSettings.tsx) — the calendar pane keeps only appearance actions
  // (visibility, rename, color, delete) for calendars it already has.
  const [calendar, sidebar] = await Promise.all([readFile(calendarPath, 'utf8'), readFile(sidebarPath, 'utf8')]);
  assert.doesNotMatch(calendar, /data-testid="calendar-manage-sources"/);
  assert.doesNotMatch(sidebar, /data-testid="calendar-sidebar-manage-sources"/);
  assert.match(sidebar, /role="menuitem"/);
  assert.match(sidebar, /calendar-appearance-dialog/);
  assert.match(sidebar, /type="color"/);
  assert.match(sidebar, /confirmCalendarDelete/);
  assert.match(sidebar, /source === 'local' && !calendar\.read_only/);
});


test('every locale declares a single effective calendar dictionary', async () => {
  const files = (await readdir(localesPath)).filter(name => name.endsWith('.json'));
  for (const file of files) {
    const source = await readFile(new URL(file, localesPath), 'utf8');
    assert.equal(source.match(/^ {2}"calendar"\s*:/gm)?.length, 1, file);
  }
});

// Deleting part of a series is three different operations, not one, so it cannot be a yes/no
// confirmation. Until this existed the interface could only ever remove a single occurrence:
// there was no way to remove an entire series, and none to stop one from a date onward.
test('deleting a recurring event asks which part of the series to remove', async () => {
  const source = await readFile(calendarPath, 'utf8');
  // A recurring event must open the chooser instead of confirming.
  assert.match(source, /if \(event\.recurring && event\.recurrence_id\) \{ setDeleteTarget\(target\); return; \}/);
  assert.match(source, /<CalendarDeleteScopeDialog/);
  assert.match(source, /onSelect=\{scope => performDelete\(deleteTarget, scope\)\}/);
  // The edit dialog deletes only what it opened, which for a series is one occurrence.
  assert.match(source, /if \(form\.recurrenceId\) \{ setDeleteTarget\(target\); return; \}/);
});

test('each delete scope maps to the request that matches its meaning', async () => {
  const source = await readFile(calendarPath, 'utf8');
  // 'all' removes the event outright and therefore carries no recurrenceId, which is the path
  // that also notifies invited attendees. 'following' ends the series at this occurrence.
  assert.match(source, /scope === 'all' \? undefined : target\.recurrenceId/);
  assert.match(source, /scope === 'following' \? 'following' : undefined/);
});

test('the scope chooser offers exactly the three meanings and nothing else', async () => {
  const dialog = await readFile(new URL('./CalendarDeleteScopeDialog.tsx', import.meta.url), 'utf8');
  assert.match(dialog, /data-testid="calendar-delete-scope-single"/);
  assert.match(dialog, /data-testid="calendar-delete-scope-following"/);
  assert.match(dialog, /data-testid="calendar-delete-scope-all"/);
  assert.match(dialog, /onSelect\('single'\)/);
  assert.match(dialog, /onSelect\('following'\)/);
  assert.match(dialog, /onSelect\('all'\)/);
});

test('every locale explains the three delete scopes', async () => {
  const locales = (await readdir(localesPath)).filter(name => name.endsWith('.json'));
  assert.equal(locales.length, 9);
  for (const name of locales) {
    const strings = JSON.parse(await readFile(new URL(name, localesPath), 'utf8'));
    for (const key of ['deleteRecurringTitle', 'deleteRecurringBody', 'deleteScopeSingle', 'deleteScopeFollowing', 'deleteScopeAll']) {
      assert.equal(typeof strings.calendar[key], 'string', `${name} is missing calendar.${key}`);
      assert.ok(strings.calendar[key].length > 0, `${name} has an empty calendar.${key}`);
    }
  }
});

// Saving an edit to a series occurrence is the same two-different-meanings problem as
// deleting one, and gets the same fix: ask, don't silently assume "only this occurrence"
// (the old, removed behavior — the interface never offered a way to edit the whole series).
test('saving a recurring occurrence asks which part of the series to edit', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /if \(form\?\.recurrenceId && !editScope\) \{ setEditScopeOpen\(true\); return; \}/);
  assert.match(source, /<CalendarEditScopeDialog/);
  assert.match(source, /onSelect=\{chooseEditScope\}/);
  assert.match(source, /onSave=\{requestSave\}/);
  // The removed one-line notice never let the series itself be edited — replaced outright,
  // not left dangling behind the new chooser.
  assert.doesNotMatch(source, /editOccurrence/);
});

test('each edit scope maps to the request that matches its meaning', async () => {
  const source = await readFile(calendarPath, 'utf8');
  // 'all' edits the series itself and therefore carries no recurrenceId, landing on the
  // base event's PATCH route instead of its `/occurrence` endpoint.
  assert.match(source, /if \(scope === 'all'\) delete payload\.recurrenceId;/);
});

test('the edit scope chooser offers exactly the two meanings edits support', async () => {
  const dialog = await readFile(new URL('./CalendarEditScopeDialog.tsx', import.meta.url), 'utf8');
  assert.match(dialog, /data-testid="calendar-edit-scope-single"/);
  assert.match(dialog, /data-testid="calendar-edit-scope-all"/);
  assert.match(dialog, /onSelect\('single'\)/);
  assert.match(dialog, /onSelect\('all'\)/);
  // Unlike delete, there's no "this and following" edit — a RECURRENCE-ID exception can't
  // retroactively change the rule the series itself expands from.
  assert.doesNotMatch(dialog, /onSelect\('following'\)/);
});

test('every locale explains the two edit scopes', async () => {
  const locales = (await readdir(localesPath)).filter(name => name.endsWith('.json'));
  assert.equal(locales.length, 9);
  for (const name of locales) {
    const strings = JSON.parse(await readFile(new URL(name, localesPath), 'utf8'));
    for (const key of ['editRecurringTitle', 'editRecurringBody', 'editScopeSingle', 'editScopeAll']) {
      assert.equal(typeof strings.calendar[key], 'string', `${name} is missing calendar.${key}`);
      assert.ok(strings.calendar[key].length > 0, `${name} has an empty calendar.${key}`);
    }
    assert.equal(strings.calendar.editOccurrence, undefined, `${name} still has the removed calendar.editOccurrence key`);
  }
});

// The hour-line background gradient starts at the column's own origin (0,0); once the column
// grew a TOP_PADDING headroom (so "00:00" isn't clipped at the very top), the gradient needed
// the same offset via backgroundPosition or its lines land TOP_PADDING px above every other
// element positioned with `+ TOP_PADDING` (axis labels, the work-hours band, events, now-line).
test('the hour-line background is shifted down by the same headroom as everything drawn on top of it', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /backgroundPosition: `0 \$\{TOP_PADDING\}px`/);
});

// Upper half of an hour (:00–:29) starts a double-click-created event at :00, lower half
// (:30–:59) at :30 — a floor, not a round-to-nearest (which would jump a click in the last
// quarter of the hour into the *next* hour instead of that hour's own :30 slot).
test('double-clicking a time slot floors to its half hour instead of rounding to the nearest one', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const openTimedCreate = \(day: Date, event: React\.MouseEvent<HTMLDivElement>\) => \{/);
  assert.match(source, /Math\.floor\(rawMinutes \/ 30\) \* 30/);
});

// A drag inside the hourly grid creates a timed event at exact clock times, snapped to
// 15-minute steps — vertically within one day, or across day columns for a multi-day span
// where each end keeps its own dragged time (not forced onto the same time of day). Both
// axes must work together on the same gesture, unlike the all-day row's day-only drag.
test('a drag inside the hourly grid creates a timed-range event on either axis, snapped to 15 minutes', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /function useHourGridDrag\(onComplete: \(start: Date, end: Date\) => void, topPadding: number, stepMinutes = 15\)/);
  assert.match(source, /const hourDrag = useHourGridDrag\(openTimedRangeCreate, TOP_PADDING, 15\)/);
  assert.match(source, /onMouseDown=\{event => hourDrag\.startDrag\(day, event\)\} onMouseEnter=\{\(\) => \{ hourDrag\.enterDay\(day\); eventDrag\.enterDay\(day\); eventResize\.enterDay\(day\); \}\}/);
  // Minutes-of-day come from the pointer's Y position (vertical axis); the day comes from
  // whichever column's onMouseEnter last fired (horizontal axis) — both tracked, not just one.
  assert.match(source, /const minutes = minutesAt\(event\.clientY\);/);
  assert.match(source, /const enterDay = \(day: Date\) => \{/);
  // The all-day row keeps its own separate day-only drag.
  assert.match(source, /const dayDrag = useDayRangeDrag\(/);
});

// `onComplete` (which sets state several components up) must never be called from inside a
// setState updater — React treats updaters as pure functions of previous state, and a nested
// setState from within one is unreliable: sometimes dropped, sometimes reordered. That was the
// actual cause of two symptoms that looked unrelated: the drag highlight occasionally not
// clearing, and a horizontal (day-crossing) hourly-grid drag occasionally not producing an
// event. Every drag hook reads the pending value from a ref and calls `onComplete` as a
// separate, ordinary statement after clearing state, never nested inside a setState updater.
test('drag hooks never call onComplete from inside a setState updater', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const dayDragHook = source.slice(source.indexOf('function useDayRangeDrag'), source.indexOf('function useHourGridDrag'));
  const hourDragHook = source.slice(source.indexOf('function useHourGridDrag'), source.indexOf('function useEventDragMove'));
  const eventMoveHook = source.slice(source.indexOf('function useEventDragMove'), source.indexOf('function useEventResizeDrag'));
  const eventResizeHook = source.slice(source.indexOf('function useEventResizeDrag'), source.indexOf('function useAllDayResizeDrag'));
  const allDayResizeHook = source.slice(source.indexOf('function useAllDayResizeDrag'), source.indexOf('function TimedEventBody'));
  for (const hook of [dayDragHook, hourDragHook, eventMoveHook, eventResizeHook, allDayResizeHook]) {
    // onCompleteRef.current is called as its own statement, immediately after the state-clearing
    // commit/setRange(null) call — never as an argument passed into one of those setState calls.
    assert.doesNotMatch(hook, /setRange\(current => \{[\s\S]*onCompleteRef\.current/);
    assert.doesNotMatch(hook, /setDrag\(current => \{[\s\S]*onCompleteRef\.current/);
    assert.doesNotMatch(hook, /setMoving\(current => \{[\s\S]*onCompleteRef\.current/);
    assert.doesNotMatch(hook, /setResize\(current => \{[\s\S]*onCompleteRef\.current/);
    assert.match(hook, /onCompleteRef\.current\(/);
  }
});

// Dragging an existing writable event reschedules it: vertically in 15-minute steps (the
// hourly grid only), horizontally in whole days (both hourly grid and month view). A read-only
// or non-writable event's own onMouseDown never starts a move, so it falls through to opening
// the (disabled) edit dialog instead, same as before.
test('dragging an existing event moves it instead of creating a new one underneath it', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /function useEventDragMove\(onComplete: \(event: CalendarFormEvent, start: Date, end: Date\) => void \| Promise<void>, options: \{ vertical\?: boolean \} = \{\}\)/);
  assert.match(source, /Math\.round\(\(event\.clientY - anchorClientYRef\.current\) \/ 15\) \* 15/);
  assert.match(source, /Math\.round\(\(day\.getTime\(\) - anchorDayRef\.current\.getTime\(\)\) \/ 86400000\)/);
  // Week/workweek: both axes. Month view: day-only, no time-of-day axis to drag on.
  assert.match(source, /const eventDrag = useEventDragMove\(moveEvent\);/);
  assert.match(source, /const monthEventDrag = useEventDragMove\(moveEvent, \{ vertical: false \}\);/);
  // The event's own onMouseDown stops the mousedown from also reaching the day/hour column
  // underneath it (which would otherwise start a create-drag at the same time).
  assert.match(source, /onMouseDown=\{mouseDownEvent => \{ if \(movable\) \{ mouseDownEvent\.stopPropagation\(\); eventDrag\.startMove\(event, day, mouseDownEvent\); \} \}\}/);
  assert.match(source, /onMouseDown=\{mouseDownEvent => \{ if \(movable\) \{ mouseDownEvent\.stopPropagation\(\); monthEventDrag\.startMove\(event, day, mouseDownEvent\); \} \}\}/);
  assert.match(source, /const movable = !event\.read_only && isWritableCalendarSource\(event\.source\);/);
  assert.match(source, /const moveEvent = \(event: CalendarFormEvent, start: Date, end: Date\): Promise<void> => \{/);
  assert.match(source, /const commitMove = async \(event: CalendarFormEvent, start: Date, end: Date, scope: 'single' \| 'all'\) => \{/);
  // A moved event keeps its original all-day-ness rather than assuming it was always timed.
  assert.match(source, /const allDay = Boolean\(event\.all_day\);/);
});

// A month-view move is day-only: only a clean single-cell segment is a grabbable origin — a
// spanning segment (part of a multi-day all-day event shown in a *different* cell) is a visual
// continuation, not something that makes sense to grab and drag on its own.
// A multi-day month-view event is draggable from any of its segments, not just a clean
// single-cell one — useEventDragMove doesn't care which day along the span was grabbed, it
// just uses it as the anchor for a relative day delta applied to the event's actual start/end.
test('a multi-day month-view event can be picked up to move from any of its segments', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const movable = !event\.read_only && isWritableCalendarSource\(event\.source\);/);
});

// A month-view all-day event's left/right edge resizes it, same as the all-day row — reusing
// useAllDayResizeDrag unmodified. Each handle only appears on the segment that's the event's
// real boundary within the visible month (not a squared-off continuation edge). This is NOT
// gated on `spanning` (multi-day) — a single-day all-day event gets the same two handles, so it
// can grow into a multi-day one exactly like the all-day row already lets it (see
// calendarResponsiveness.test.ts for that non-gating); `resizable` (Boolean(segmentRaw) &&
// movable) only ever excludes a timed event (month view has no time axis to resize by
// day-granularity) or a read-only/non-writable one.
test('a month-view all-day event — single-day or multi-day — resizes by its left/right edge, via the same hook as the all-day row', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const monthResize = useAllDayResizeDrag\(moveEvent\);/);
  assert.match(source, /const resizable = allDay && movable;/);
  assert.match(source, /\{resizable && !segment\?\.continuesFrom && <div className="calendar-event-resize-handle calendar-allday-resize-handle" data-testid="calendar-month-resize-start"/);
  assert.match(source, /\{resizable && !segment\?\.continuesTo && <div className="calendar-event-resize-handle calendar-allday-resize-handle" data-testid="calendar-month-resize-end"/);
  assert.match(source, /monthResize\.startResize\(event, eventLastInclusiveDay, dayFromKey\(startKey\)\);/);
  assert.match(source, /monthResize\.startResize\(event, dayFromKey\(startKey\), eventLastInclusiveDay\);/);
  // Not gated on `spanning` — that still exists (title/corner/z-index logic still needs it) but
  // must never appear in the resize-handle conditions themselves.
  assert.doesNotMatch(source, /spanning && !segment\?\.continuesFrom && <div className="calendar-event-resize-handle/);
  assert.doesNotMatch(source, /spanning && !segment\?\.continuesTo && <div className="calendar-event-resize-handle/);
});

// A multi-day event's title shows once, on its first visible day — every later segment is just
// the bar continuing, so repeating the title on each one read as separate events rather than
// one. Month-view event text is also bold now, matching the timed event's redesign.
// The title span is always rendered (never omitted) — an empty button has no line box to size
// itself by and collapses to near-zero height, which stopped a continuation segment's bar from
// reserving its row's space, letting that day's other events overlap into where it should sit.
// Visually hiding the repeated text (not removing it) keeps the box's height correct while
// still showing the title only once.
test('a multi-day month-view event shows its title only on the first segment (visually, not by omitting it), and text is bold', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const titleSpanCount = source.match(/<span style=\{titleVisible \? eventCardTitle : \{ \.\.\.eventCardTitle, visibility: 'hidden' \}\}>\{event\.summary \|\| t\('calendar\.untitled'\)\}<\/span>/g);
  // The move/resize preview and ghost (MonthDragRow) share one generic version of the same
  // rule, keyed off its own `titleVisible`/`event` props instead of the real per-event loop's
  // own `titleVisible`/`event` names — both spans are textually identical.
  assert.equal(titleSpanCount?.length, 2);
  assert.match(source, /const eventCard: CSSProperties = \{ display: 'flex', alignItems: 'baseline', gap: 4, minWidth: 0, flex: 1, width: '100%', textAlign: 'left', border: 0, borderRadius: 4, padding: '2px 6px', color: 'white', fontSize: 11, fontWeight: 700,/);
});

// `titleVisible` follows `segment.continuesFrom` directly, in exactly the same formula whether
// this event is being dragged or not — the ghost is never a special case any more, just the same
// event rendered through `renderRealEventCard`, dimmed. It shows on whichever day is this event's
// own true first *visible* one: `segment` already folds in a row edge, so there's nothing left to
// special-case for dragging vs. resting.
test('a month-view event\'s title visibility follows directly from its own segment, no special-casing for dragging', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const titleVisible = !segment\?\.continuesFrom;/);
  assert.match(source, /titleVisible: boolean \}\) \{/);
  assert.match(source, /titleVisible=\{item\.titleVisible!\}/);
  assert.match(source, /titleVisible=\{item\.preview\.titleVisible\}/);
  assert.match(source, /titleVisible: !previewSegment\.continuesFrom \};/);
});

// If the preview's range reaches back far enough to suppress even the ghost's own true first day
// (a backward drag with enough overlap), its title has nowhere left to show — and that's
// correct, not a bug to work around: a user explicitly asked for exactly this ("if the first
// cell is covered by preview text should not be visible"), rejecting an earlier fallback that
// showed the title on some other day instead. No fallback exists; the ghost simply shows no
// title while its own true first day isn't rendered.
test('the ghost shows no title at all while its own true first day is suppressed by the preview, with no fallback to another day', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.doesNotMatch(source, /ghostVisibleFirstKey/);
  assert.doesNotMatch(source, /forwardAdjacent/);
  assert.doesNotMatch(source, /backwardAdjacent/);
});

// A multi-day event (and its move/resize preview) is placed by a *lane* assigned once per
// calendar week via the same greedy interval-partitioning `layoutWeekAllDayEvents` already uses
// for week view's all-day row — not by each cell independently sorting its own event list and
// hoping every neighbor's sort happens to agree. Two cells' own full lists can differ (a
// different number of same-day events, or the dragged event's own segment being filtered out of
// just the days the preview covers) without the *spanning* events among them needing to move —
// a shared, week-wide lane keeps a multi-day event (and its preview) on the exact same row in
// every cell it touches regardless, so it reads as one continuous bar with no way to drift.
test('a multi-day event and its move/resize preview share one lane, assigned once per week, not independently per cell', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const monthWeekLanes = new Map<number, \{ laneOf: Map<string, number>; laneCount: number \}>\(\);/);
  assert.match(source, /const placements = layoutWeekAllDayEvents\(\[\.\.\.spanningById\.values\(\)\], week\);/);
  assert.match(source, /const \{ laneOf, laneCount \} = monthWeekLanes\.get\(Math\.floor\(dayIndex \/ 7\)\) \?\? \{ laneOf: new Map<string, number>\(\), laneCount: 0 \};/);
  assert.match(source, /const existingLane = laneOf\.get\(String\(draggedEvent\.id\)\);/);
  // The preview takes over its lane's `preview` slot, stacking on top of (not replacing) that
  // same lane's `ghost` slot if this day is also within the real event's own range.
  assert.match(source, /laneSlots\[existingLane\] = \{ \.\.\.laneSlots\[existingLane\], preview: previewEntry \};/);
  assert.match(source, /const insertAt = others\.findIndex\(\(\{ event \}\) => compareAllDayOrder\(draggedEvent, event\) < 0\);/);
});

// The lane a dragged multi-day event needs isn't decided by its own real (unmoved) range alone
// — dragging it onto a week that already has a different multi-day event sitting in the ghost's
// old lane number (or into a week the ghost never touched at all) otherwise left the preview
// either overlapping that other event's own bar unchecked, or with no lane reservation in that
// week whatsoever. Folding the ghost's and the preview's ranges into one combined item — the
// union of wherever the event still visually is and wherever it's about to be — before handing
// a week's items to the greedy packer routes it around anything either range would collide with.
test('a dragged multi-day event\'s lane accounts for the preview\'s shifted range too, not just its own real one, so it never collides with an unrelated event at its new position', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /if \(draggedEvent && previewEvent\) \{/);
  assert.match(source, /const real = spanningById\.get\(draggedId\);/);
  assert.match(source, /const previewTouchesWeek = previewEndKey > weekStartKey && previewStartKey < weekEndKeyExclusive;/);
  assert.match(source, /spanningById\.set\(draggedId, \{ \.\.\.draggedEvent, all_day: true, starts_at: startKeys\.sort\(\)\[0\], ends_at: endKeys\.sort\(\)\.at\(-1\), span_days: monthSpanDays\(draggedEvent\) \}\);/);
});

// A lower lane an event shares with another one that doesn't reach this particular day would
// otherwise just be missing from this cell's own list — compacting the remaining occupied
// lane(s) up to fill the gap, which puts the same event at a *different* visual row on this cell
// than on its neighbors where that lower lane is filled. A blank spacer (styled to match a real
// row's actual height, not an empty div that collapses to 0px) keeps every occupied lane at its
// own fixed row regardless of what else this one cell does or doesn't have.
test('a month-view cell renders a same-height blank spacer for a lane it skips, instead of letting a higher lane compact up to fill the gap', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /let highestLane = -1;/);
  assert.match(source, /laneSlots\.forEach\(\(slot, lane\) => \{ if \(slot\) highestLane = lane; \}\);/);
  assert.match(source, /for \(let lane = 0; lane <= highestLane; lane \+= 1\) laneRows\.push\(laneSlots\[lane\] \?\? \{ empty: true \}\);/);
  assert.match(source, /if \('empty' in item\) return <div aria-hidden="true" key=\{`empty-lane-\$\{itemIndex\}`\} style=\{eventRow\}><div style=\{\{ \.\.\.eventCard, visibility: 'hidden' \}\}>&nbsp;<\/div><\/div>;/);
});

// The month grid wraps every 7 days — a calendar row has no cell to its right past the last
// column (or to its left before the first), so a segment that conceptually continues past either
// edge has nowhere to visually bridge into within *this* row. Squaring that edge off (or bleeding
// the bar's negative margin past it, same as a genuine mid-row continuation) poked the bar out
// past the grid's own border instead of reading as "continues on the next row" — only visible
// mid-drag, when a multi-day move/resize crosses from Sunday into the following Monday.
test('a segment clamps back to rounded at a calendar row edge, even when the event genuinely continues past it onto the next row', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const isRowStart = dayIndex % 7 === 0;/);
  assert.match(source, /const isRowEnd = dayIndex % 7 === 6;/);
  assert.match(source, /continuesFrom: !isRowStart && previewSegmentRaw\.continuesFrom,/);
  assert.match(source, /continuesTo: !isRowEnd && previewSegmentRaw\.continuesTo,/);
  assert.match(source, /const segment = segmentRaw \? \{ continuesFrom: !isRowStart && segmentRaw\.continuesFrom, continuesTo: !isRowEnd && segmentRaw\.continuesTo \} : null;/);
  assert.match(source, /const titleVisible = !segment\?\.continuesFrom;/);
});

// The title comes first and shrinks with an ellipsis; the time is fixed-width and never
// shrinks, so it always stays fully visible at the end of the line instead of the whole line
// (title + time together) getting clipped when the title alone is already long.
test('a month-view event puts its title first (ellipsized if long) and its time at the end of the line', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const eventCardTitle: CSSProperties = \{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' \};/);
  assert.match(source, /const eventCardTime: CSSProperties = \{ fontFamily: 'var\(--font-mono, ui-monospace, monospace\)', fontSize: 9\.5, fontWeight: 400, opacity: \.85, flexShrink: 0 \};/);
  // The title span comes before the time <strong> in both the real event and MonthDragRow (the
  // shared preview/ghost renderer).
  assert.match(source, /visibility: 'hidden' \}\}>\{event\.summary \|\| t\('calendar\.untitled'\)\}<\/span>\{timeLabel && <strong style=\{timedCard \? eventCardTimedTime : eventCardTime\}>\{timeLabel\}<\/strong>\}/);
});

// The move/resize ghost preview for a multi-day month-view event is computed the same way the
// real event is placed — a hypothetical event with the shifted/resized start+end, run back
// through the very same `allDayEventSegment` function real events use — so it bridges cells and
// squares off edges identically, not a simplified single-cell stand-in.
test('the month-view move/resize ghost preview places itself via the same allDayEventSegment as real events', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const previewSegmentRaw = previewEvent && dayKey >= String\(previewEvent\.starts_at\)\.slice\(0, 10\) && dayKey < String\(previewEvent\.ends_at\)\.slice\(0, 10\) \? allDayEventSegment\(previewEvent, day\) : null;/);
  assert.match(source, /testId=\{moving \? 'calendar-month-move-preview' : 'calendar-month-resize-preview'\}/);
});

// The ghost (dimmed, original position) and the preview (bright, new position) are two visually
// distinct things — they are never merged into one seamless bar that pretends to be a single
// element, even when a drag lands the preview touching or overlapping the ghost's own original
// range. Each rounds off at *its own* true edges independently, using only its own start/end.
// Neither one ever squares an edge just because the other happens to be sitting right next to
// it (or stacked directly on top of it) — a user explicitly asked for this ("I want ghost/preview
// to ALWAYS have rounded corners") after an earlier version did exactly the opposite. The ghost
// is never even aware the preview exists any more: it's the exact same `renderRealEventCard`
// used when nothing is being dragged, just dimmed — there is no ghost-specific segment logic
// left to conflate with the preview's at all.
test('the ghost and the preview never square off against each other — each rounds at its own true edges only', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.doesNotMatch(source, /forwardAdjacent/);
  assert.doesNotMatch(source, /backwardAdjacent/);
  assert.doesNotMatch(source, /previewCoversKey/);
  assert.doesNotMatch(source, /segmentGhostAware/);
  assert.match(source, /continuesFrom: !isRowStart && previewSegmentRaw\.continuesFrom,/);
  assert.match(source, /continuesTo: !isRowEnd && previewSegmentRaw\.continuesTo,/);
  assert.match(source, /const segment = segmentRaw \? \{ continuesFrom: !isRowStart && segmentRaw\.continuesFrom, continuesTo: !isRowEnd && segmentRaw\.continuesTo \} : null;/);
});

// `borderTopLeftRadius`/etc. on the real event's own card must be set unconditionally, every
// render, never omitted — `eventCard`'s own `borderRadius` (the shorthand) already always sets a
// value for every corner, and once a longhand like this has been set inline even once, the
// browser has lost the shorthand's own contribution to that corner. A later render that stops
// including the longhand (e.g. gating it behind `spanning`, which flips false the moment
// continuesFrom/To both clear on this same, persisted-across-renders button mid-drag as the
// preview suppresses/un-suppresses a neighboring day) clears it to nothing left to fall back to
// — not back to the shorthand's 4px, but the browser default of a hard 0 — the exact same CSSOM
// corruption this file already documents for `background`/`backgroundColor`, just for
// border-radius instead, and only visible once a drag's own overlap made `spanning` flip mid-way.
test('a month-view event\'s own border-radius longhands are always set, never conditionally omitted', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.doesNotMatch(source, /\.\.\.\(spanning \? \{ borderTopLeftRadius/);
  assert.match(source, /borderTopLeftRadius: segment\?\.continuesFrom \? 0 : 4, borderBottomLeftRadius: segment\?\.continuesFrom \? 0 : 4, borderTopRightRadius: segment\?\.continuesTo \? 0 : 4, borderBottomRightRadius: segment\?\.continuesTo \? 0 : 4, paddingLeft: segment\?\.continuesFrom \? 10 : 6, paddingRight: segment\?\.continuesTo \? 10 : 6, overflow: 'visible' \}\}>/);
});

// Capping at 5 and showing a "+1 more" chip for a 6th event takes exactly the same vertical
// space as just showing that 6th event directly, so the chip is only worth its own row once it
// actually collapses more than one event — a cell with exactly 6 shows all 6, with no chip.
test('a month-view cell only truncates once there is more than one event to hide', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const visibleList = renderList\.length <= 6 \? renderList : renderList\.slice\(0, 5\);/);
});

// Dragging an event's top or bottom edge resizes it: the dragged edge tracks the exact day+time
// under the pointer (not a delta, unlike a move — there's no meaningful "grab offset" for an
// edge), while the other edge stays fixed. Works both vertically (15-minute steps) and
// horizontally (a day-crossing drag extends the event across days), same as the create-drag.
test('dragging an event\'s top or bottom edge resizes it on either axis', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /function useEventResizeDrag\(onComplete: \(event: CalendarFormEvent, start: Date, end: Date\) => void \| Promise<void>, topPadding: number, stepMinutes = 15\)/);
  assert.match(source, /const eventResize = useEventResizeDrag\(moveEvent, TOP_PADDING, 15\);/);
  // The fixed (non-dragged) edge is the event's own other end — start-edge drags keep the
  // original end fixed, end-edge drags keep the original start fixed.
  assert.match(source, /eventResize\.startResize\(event, 'start', new Date\(event\.ends_at \?\? 0\), day, mouseDownEvent\)/);
  assert.match(source, /eventResize\.startResize\(event, 'end', new Date\(event\.starts_at \?\? 0\), day, mouseDownEvent\)/);
  // Each handle stops its mousedown from also starting the event's own move-drag or the day
  // column's create-drag underneath both of those.
  const startHandle = source.slice(source.indexOf("data-testid=\"calendar-event-resize-top\""), source.indexOf("data-testid=\"calendar-event-resize-bottom\""));
  assert.match(startHandle, /mouseDownEvent\.stopPropagation\(\);/);
  // Only a writable, non-read-only event grows resize handles at all.
  assert.match(source, /\{movable && <div className="calendar-event-resize-handle"/);
});

// A drag's ghost preview must never render on the day the drag hasn't left yet — the moment
// right after mousedown, before any real movement, the target day still equals the origin, and
// rendering a preview there stacked on top of the (also still-present) original read as two
// copies of the same event. Both drags gate this the same way: treat `deltaDays === 0` as "not
// moved yet" and simply skip the preview/dimming, letting the original render normally.
test('a drag preview never doubles the original event on the day it started from', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const moving = monthEventDrag\.moving && monthEventDrag\.moving\.deltaDays !== 0 \? monthEventDrag\.moving : null;/);
  assert.match(source, /const moving = allDayDrag\.moving && allDayDrag\.moving\.deltaDays !== 0 \? allDayDrag\.moving : null;/);
  // Both resize hooks have the identical failure mode on mousedown (before any movement, the
  // dragged edge still equals the event's own real edge, so a naively-published preview would
  // exactly overlap the original) — fixed by recording the initial state in the ref only and
  // publishing to state solely from the movement branches (onMouseMove / enterDay), never from
  // startResize itself.
  const eventResizeHook = source.slice(source.indexOf('function useEventResizeDrag'), source.indexOf('function useAllDayResizeDrag'));
  const allDayResizeHook = source.slice(source.indexOf('function useAllDayResizeDrag'), source.indexOf('function TimedEventBody'));
  for (const hook of [eventResizeHook, allDayResizeHook]) {
    const startResizeBody = hook.slice(hook.indexOf('const startResize'), hook.indexOf('const enterDay'));
    assert.doesNotMatch(startResizeBody, /commit\(/);
    assert.match(startResizeBody, /resizeRef\.current = /);
  }
});

// All-day pills move (day-only, no time axis) and resize (left/right edge extends the span)
// exactly like month-view cards and the hourly grid's timed events do.
test('all-day pills move horizontally and resize by their left/right edge', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /function useAllDayResizeDrag\(onComplete: \(event: CalendarFormEvent, start: Date, end: Date\) => void \| Promise<void>\)/);
  assert.match(source, /const allDayDrag = useEventDragMove\(moveEvent, \{ vertical: false \}\);/);
  assert.match(source, /const allDayResize = useAllDayResizeDrag\(moveEvent\);/);
  // The day-cell drag targets feed both the existing multi-day create-drag and the two new
  // all-day drags from the same onMouseEnter.
  assert.match(source, /onMouseEnter=\{\(\) => \{ dayDrag\.extendDrag\(day\); allDayDrag\.enterDay\(day\); allDayResize\.enterDay\(day\); \}\}/);
  // A pill's own mousedown starts a move and stops the day cell underneath from also starting
  // the unrelated multi-day create-drag.
  assert.match(source, /onMouseDown=\{mouseDownEvent => \{ if \(movable\) \{ mouseDownEvent\.stopPropagation\(\); allDayDrag\.startMove\(event, dayFromKey\(startKey\), mouseDownEvent\); \} \}\}/);
  // Each edge resize handle only appears when that edge is the event's real boundary within
  // the visible week — a squared-off continuation edge isn't a resizable one.
  assert.match(source, /\{movable && !continuesFrom && <div className="calendar-event-resize-handle calendar-allday-resize-handle"/);
  assert.match(source, /\{movable && !continuesTo && <div className="calendar-event-resize-handle calendar-allday-resize-handle"/);
});

// The all-day row is meant to read as part of the grid, not separate header chrome: its day
// cells get the same background/border treatment as a normal hour-row day column.
test('the all-day row background and borders match a normal hour row', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /borderRight: '1px solid var\(--border-subtle\)', \.\.\.\(isWeekend\(day\) \? weekendColumn : \{\}\), \.\.\.\(dayDrag\.inRange\(day\) \? monthCellDragging : \{\}\) \}\} \/>\)\}/);
});

// The day-header buttons no longer draw their own bottom border — directly above the all-day
// row's own top border, the two together read as one heavy double line rather than one divider.
test('the week-view day header has no bottom border of its own', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const dayHeader: CSSProperties = \{ position: 'sticky', top: 0, zIndex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', gap: 6, minWidth: 0, padding: '7px 6px', background: 'var\(--bg-secondary\)' \};/);
});

// An empty all-day row is still a full compact row of chrome (border, background, spacer) with
// nothing in it — not worth the vertical space when the visible week has no all-day events.
test('the all-day row is hidden entirely when the visible week has no all-day events', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /\{weekAllDay\.length > 0 && \(\(\) => \{/);
  assert.match(source, /return <div style=\{\{ display: 'grid', gridTemplateColumns: columns, gridAutoRows: 22/);
});

// A timed event shows its full title (wrapped, not truncated to one ellipsized line) in bold,
// and its time prefixed with a clock icon rather than as bare mono text.
test('a timed event shows its full title in bold and a clock icon next to the time', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const timedEventTitle: CSSProperties = \{ display: 'block', fontWeight: 700, whiteSpace: 'normal', overflowWrap: 'break-word' \};/);
  assert.match(source, /<span style=\{timedEventTitle\}>\{event\.summary \|\| t\('calendar\.untitled'\)\}<\/span>/);
  assert.match(source, /<span style=\{timedEventTimeRow\}><svg width="10" height="10" viewBox="0 0 24 24"/);
});

// "A little transparent": every rendered event (timed, all-day pill, month-view card) blends
// slightly with what's behind it rather than sitting fully opaque.
// A real CSS `opacity` blends each element independently with whatever sits directly behind
// it — for one segment of a multi-day event that's the day cell, but the month grid's cells
// are separated by a 1px gap with the border color showing through underneath, and a spanning
// event's bridging technique covers that gap with the event's *own* box. A translucent box
// over the gap lets the border color show through right at the seam between two days — a
// visible line through what should read as one continuous bar (the exact bug this once was:
// "multi day event has a visible border between days"). `eventTint()` pre-blends the color
// with `color-mix()` into an opaque color that merely *looks* translucent instead, so every
// event renders "a little transparent" with nothing left for a seam to show through.
test('events render a little transparent via a pre-blended color, not real CSS opacity (which broke multi-day seams)', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const EVENT_OPACITY = 0\.55;/);
  assert.match(source, /function eventTint\(color: string \| null \| undefined, ratio: number = EVENT_OPACITY, glass = false\): string \{/);
  assert.match(source, /return `color-mix\(in srgb, \$\{color \|\| 'var\(--accent\)'\} \$\{Math\.round\(ratio \* 100\)\}%, \$\{glass \? 'transparent' : 'var\(--bg-primary\)'\}\)`;/);
  // Every real event and every move/resize ghost preview goes through it — none hardcode a raw
  // `event.calendar_color || 'var(--accent)'` background with a separate opacity any more.
  assert.doesNotMatch(source, /background: event\.calendar_color \|\| 'var\(--accent\)', opacity:/);
  assert.doesNotMatch(source, /background: (moving\.event|movePreview|resizePreview)\.calendar_color \|\| 'var\(--accent\)', opacity:/);
  const eventTintCalls = source.match(/background: (?:timedCard \? 'transparent' : )?(?:eventTint|eventFill)\(/g);
  // Month view's move preview and its dimmed ghost now share one call site (MonthDragRow, used
  // for both), so the count is unchanged from before: month card, MonthDragRow, all-day pill,
  // all-day move preview, all-day resize preview, the timed event, and its move + resize
  // previews — plus the month-view timed event's color bar (eventFill, which stripes a
  // tentative answer on top of eventTint).
  assert.equal(eventTintCalls?.length, 9);
});

// The same seam risk applies to dimming an event mid-drag: a real CSS `opacity` on a multi-day
// segment's bridging wrapper, or a week-view all-day pill sitting over its day-cell divider
// borders, lets whatever's underneath show through right at the seam — an "ugly black line" for
// the whole time it's being dragged. Diluting further with the same `eventTint()` ratio keeps it
// opaque (just paler), dimming it the same way without ever exposing what's underneath.
test('the moving/resizing dim state uses a paler eventTint(), never a real CSS opacity, on multi-day-capable events', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const EVENT_DIM_RATIO = 0\.28;/);
  const dimmedTintCalls = source.match(/(?:eventTint|eventFill)\([^)]*beingMoved \|\| beingResized \? (?:EVENT_DIM_RATIO : EVENT_OPACITY|EVENT_GLASS_DIM_RATIO : EVENT_GLASS_OPACITY, true)\)/g);
  assert.equal(dimmedTintCalls?.length, 3, 'expected the dim ratio on the month-view card, the all-day pill, and the timed event');
  // No wrapper around a multi-day-capable event (month card row, all-day pill, timed event) may
  // still dim itself via real opacity.
  assert.doesNotMatch(source, /opacity: beingMoved \|\| beingResized \? 0\.35 : 1/);
});

// The event actually being dragged is never removed or swapped for a separate "ghost" element —
// unlike an earlier version of this fix, which hid the real card and rendered a wholly separate
// pinned-row stand-in instead. That swap made a plain click (mousedown+mouseup, no real
// movement) or the very start of a drag momentarily replace one DOM element with a differently-
// laid-out one, which could shift size/position for an instant. This version keeps the exact
// same DOM node in its exact same place in the render list across a click or a drag, just
// dimming its background (see the dim-ratio eventTint() call above) — so nothing about its size
// or position ever changes from being pressed or dragged, only its color.
test('the dragged event keeps rendering through its normal render-list entry, dimmed in place, never swapped for a separate element', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.doesNotMatch(source, /if \(beingMoved \|\| beingResized\) return null;/);
  assert.match(source, /const renderRealEventCard = \(event: CalendarFormEvent\) => \{/);
});

// On a day the preview also reaches, the dragged event's own real (dimmed) segment for that same
// day is *never* left out of the render list any more — an earlier version did exactly that
// (suppressing the ghost wherever the preview also covered a day), which shortened the ghost down
// to only the days the preview *didn't* reach, reading as the event itself shrinking mid-drag. A
// user explicitly asked for the opposite: "keep the original event elements and just make them a
// little transparent". The ghost keeps every one of its own real days; the preview stacks
// directly on top of it (same lane) wherever the two share a day, instead of replacing it.
test('the ghost keeps every one of its own real days for the whole drag, never shortened by the preview — the preview stacks on top of it instead', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.doesNotMatch(source, /if \(previewSegment && draggedEvent && String\(event\.id\) === String\(draggedEvent\.id\)\) return;/);
  assert.match(source, /if \(lane != null\) laneSlots\[lane\] = \{ \.\.\.laneSlots\[lane\], ghost: event \};/);
  assert.match(source, /if \(item\.ghost && item\.preview\) \{/);
  assert.match(source, /\{renderRealEventCard\(item\.ghost\)\}/);
  assert.match(source, /<div aria-hidden="true" style=\{\{ position: 'absolute', inset: 0, zIndex: 2 \}\}>\{previewNode\}<\/div>/);
});

// `monthCell` (the month-view day cell) and `timeColumn` (the week-view hourly day column) both
// set a plain `background` shorthand unconditionally. Merging a *different* key —
// `backgroundColor` — on top for the drag highlight / weekend tint corrupts the DOM's inline
// style once that key is later removed: React clears just that longhand, and since the browser's
// CSSOM already lost the shorthand's own contribution to it the moment the longhand was first
// set, there's nothing left to fall back to — the element ends up fully transparent, not back to
// its base color. This was the real cause of the drag-to-create highlight "never resetting" on
// month view (confirmed with a real browser via e2e/calendar-drag-repro.spec.ts): the cell wasn't
// stuck highlighted, its background was gone outright. Using the same key everywhere avoids two
// different background properties ever fighting over one element.
test('the month/week drag highlight and weekend tint share the same `background` key as their base cell style, never `backgroundColor`', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const monthCellDragging: CSSProperties = \{ background: 'color-mix\(in srgb, var\(--accent\) 16%, var\(--bg-primary\)\)' \};/);
  assert.match(source, /const weekendColumn: CSSProperties = \{ background: 'color-mix\(in srgb, var\(--bg-secondary\) 55%, var\(--bg-primary\)\)' \};/);
  assert.doesNotMatch(source, /const monthCellDragging: CSSProperties = \{ backgroundColor:/);
  assert.doesNotMatch(source, /const weekendColumn: CSSProperties = \{ backgroundColor:/);
});

// The real all-day pill is `position: relative` (so its own z-index takes effect), but its
// move/resize preview was left `position: static` — a non-positioned element paints *below* any
// positioned one in the same stacking context regardless of DOM order, so the dimmed real pill
// (positioned) rendered on top of the preview (not) for the whole time a drag overlapped both,
// even though the preview comes later in the DOM. Both now get an explicit `position: relative`
// plus a z-index (the preview's higher), so the stacking no longer depends on that DOM-order
// subtlety at all.
test('a week-view all-day pill\'s move/resize preview is explicitly positioned above the real (dimmed) pill', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /style=\{\{ \.\.\.allDayPill, position: 'relative', zIndex: 1, gridColumn: `\$\{startIndex \+ 2\} \/ \$\{endIndex \+ 2\}`/);
  assert.match(source, /data-testid="calendar-allday-move-preview" style=\{\{ \.\.\.allDayPill, position: 'relative', zIndex: 2,/);
  assert.match(source, /data-testid="calendar-allday-resize-preview" style=\{\{ \.\.\.allDayPill, position: 'relative', zIndex: 2,/);
});

// Each day cell stacks its multi-day event segments as separate DOM elements (unlike week
// view's single spanning pill), so a plain CSS `:hover` only ever reaches the one segment
// actually under the pointer. `hoveredEventId` tracks it in JS instead so every segment
// belonging to the hovered event can react together, via a `filter: brightness()` (never a
// property that could shift layout, unlike the "size must never change" dim-tint elsewhere).
test('hovering one segment of a multi-day month-view event highlights every one of its segments', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const \[hoveredEventId, setHoveredEventId\] = useState<string \| null>\(null\);/);
  assert.match(source, /const highlighted = spanning && hoveredEventId != null && hoveredEventId === String\(event\.id\);/);
  assert.match(source, /onMouseEnter=\{\(\) => setHoveredEventId\(String\(event\.id\)\)\} onMouseLeave=\{\(\) => setHoveredEventId\(current => current === String\(event\.id\) \? null : current\)\}/);
  assert.match(source, /filter: highlighted \? 'brightness\(1\.18\)' : undefined/);
});

// A week-view timed event's own width/position narrows to share its column with whatever else
// overlaps it that day (`column`/`columns` from `layoutTimedEvents`) — its move/resize preview
// used a fixed `left: 2, right: 2` instead, rendering at very close to the full column width
// regardless of how many other events the real event actually overlaps, which could be
// dramatically wider than the real event whenever it shared its day with anything else.
// Recomputing the same layout for a hypothetical version of the event at the preview's instant
// (that day's real events, with the dragged one itself swapped for the hypothetical) keeps the
// preview's width/position exactly matching what the real event will actually look like there.
test('a week-view timed event\'s move/resize preview recomputes its own overlap-aware column width, not a fixed inset', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.doesNotMatch(source, /left: 2, right: 2, zIndex: 3/);
  assert.match(source, /const moveLayoutEvent = moving && movePreview \? \{ \.\.\.moving\.event, starts_at: iso\(movePreview\.start\), ends_at: iso\(movePreview\.end\) \} : null;/);
  assert.match(source, /const resizeLayoutEvent = resizing && resizePreview \? \{ \.\.\.resizing\.event, starts_at: iso\(resizePreview\.start\), ends_at: iso\(resizePreview\.end\) \} : null;/);
  assert.match(source, /const moveHereLayout = moveHere && moveLayoutEvent \? layoutTimedEvents\(\[\.\.\.dayEvents\.filter\(candidate => String\(candidate\.id\) !== String\(moveLayoutEvent\.id\)\), moveLayoutEvent\], day\)\.find\(item => String\(item\.event\.id\) === String\(moveLayoutEvent\.id\)\) : null;/);
  assert.match(source, /left: `calc\(\$\{\(moveHereLayout\?\.column \?\? 0\) \* 100 \/ \(moveHereLayout\?\.columns \?\? 1\)\}% \+ 3px\)`, width: `calc\(\$\{100 \/ \(moveHereLayout\?\.columns \?\? 1\)\}% - 6px\)`/);
});

// `startMove` publishes state immediately on mousedown (deltaDays: 0, deltaMinutes: 0) so a
// cursor can already read as "grab" — but a plain, unfiltered read of that state treated the
// zero-movement instant right after mousedown as a real move, dimming a week-view timed event's
// style the moment it was pressed, before any drag happened at all. The rendered event must read
// the filtered local `moving`/`resizing` (gated on actual deltaDays/deltaMinutes movement), not
// the hooks' raw `eventDrag.moving`/`eventResize.resize`.
test('a week-view timed event only dims once the drag has actually moved, not on mere mousedown', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const moving = eventDrag\.moving && \(eventDrag\.moving\.deltaDays !== 0 \|\| eventDrag\.moving\.deltaMinutes !== 0\) \? eventDrag\.moving : null;/);
  assert.match(source, /const beingMoved = moving != null && String\(moving\.event\.id\) === String\(event\.id\);\n\s*const beingResized = resizing != null && String\(resizing\.event\.id\) === String\(event\.id\);\n\s*return <div key=\{event\.id\} style=\{\{ \.\.\.timedEvent/);
});

// A plain (non all-day) event's `ends_at` is just a later *time*, almost always on the same
// calendar day as `starts_at` — month view has no time axis, so reusing that date directly as
// the preview's exclusive end meant a single-day/timed event's day-membership check never
// matched any day, leaving it invisible mid-drag (confirmed via e2e/calendar-drag-repro.spec.ts:
// "dragging a single-day timed event renders a ghost preview"). The exclusive end must fall back
// to start-date-plus-one whenever the event isn't a genuinely multi-day all-day span.
test('the month-view move preview treats a non-multi-day event as covering exactly the one day it is dragged to', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const isAllDay = Boolean\(moving\.event\.all_day \|\| moving\.event\.allDay\);/);
  assert.match(source, /: isAllDay && rawEndKey > startKey \? rawEndKey : dateOnly\(new Date\(dayFromKey\(startKey\)\.getFullYear\(\), dayFromKey\(startKey\)\.getMonth\(\), dayFromKey\(startKey\)\.getDate\(\) \+ 1\)\);/);
});

// `shift()` (date-only, via dayFromKey/dateOnly) is correct for an all-day event's `starts_at`,
// which never carries a meaningful time, but applying it to a *timed* event's `starts_at`
// discarded the clock time entirely — `eventTime()` then read midnight off the resulting
// date-only string, which `new Date("2026-09-17")` parses as UTC midnight, showing up as e.g.
// 02:00 in a viewer two hours ahead of UTC instead of the event's real time. A timed event's
// start must keep its real hour/minute/second, shifted by day only, built as a timezone-less
// local string (no trailing `Z`) so both `new Date(...)` for display and this same string's own
// day-key slice agree with the viewer's own calendar days. Screenshot-confirmed by a real user
// (the preview showing 02:00 for an 08:00 event) and reproduced in
// e2e/calendar-preview-time.spec.ts.
test('the month-view move preview keeps a timed event\'s real clock time, not midnight', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const startsAt = isAllDay \? shift\(startKey\) : \(\(\) => \{/);
  assert.match(source, /const original = new Date\(String\(moving\.event\.starts_at \?\? moving\.event\.startsAt \?\? 0\)\);/);
  assert.match(source, /const shifted = new Date\(original\.getFullYear\(\), original\.getMonth\(\), original\.getDate\(\) \+ moving\.deltaDays, original\.getHours\(\), original\.getMinutes\(\), original\.getSeconds\(\)\);/);
  assert.match(source, /return `\$\{shifted\.getFullYear\(\)\}-\$\{pad\(shifted\.getMonth\(\) \+ 1\)\}-\$\{pad\(shifted\.getDate\(\)\)\}T\$\{pad\(shifted\.getHours\(\)\)\}:\$\{pad\(shifted\.getMinutes\(\)\)\}:\$\{pad\(shifted\.getSeconds\(\)\)\}`;/);
  assert.match(source, /return \{ \.\.\.moving\.event, starts_at: startsAt, ends_at: shift\(endKey\) \};/);
});

// Month-view day cells no longer guard `onMouseDown` with `event.target === event.currentTarget`
// (only `onClick`/`onDoubleClick` still do, since a click on a child event/day-select control
// must not also select/open the day) — week view's equivalent cell has never had this guard on
// `onMouseDown`, and its create-drag has always worked reliably. Aligning month view with that
// proven pattern means a mousedown that lands on a child (an event card, the day-number button)
// still starts the drag-range tracking; `startDrag` only writes to a ref, so it's harmless unless
// a real `extendDrag` follows from an actual drag gesture.
test('a month-view cell starts its range-drag unconditionally on mousedown, like week view, not only on the cell background', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /onMouseDown=\{\(\) => dayDrag\.startDrag\(day\)\}/);
  assert.doesNotMatch(source, /onMouseDown=\{event => \{ if \(event\.target === event\.currentTarget\) dayDrag\.startDrag\(day\); \}\}/);
});

// Opening an event's details now takes a double-click, not a single click — a single click
// (or a click-drag that doesn't move anywhere) must not open anything, matching how the day
// grid's own double-click-to-create already worked before this. Enter/Space on a focused event
// still opens it directly: a button's native click still fires on those keys, and there's no
// keyboard equivalent of "double-click" to require instead.
test('an event opens on double-click, not a single click, with Enter/Space still working from the keyboard', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /onDoubleClick=\{\(\) => openEdit\(event\)\}/); // month view
  assert.match(source, /onDoubleClick=\{doubleClickEvent => \{ doubleClickEvent\.stopPropagation\(\); openEdit\(event\); \}\}/); // hourly grid
  assert.match(source, /onDoubleClick=\{doubleClickEvent => \{ doubleClickEvent\.stopPropagation\(\); openEdit\(event\); \}\}\n {18}onContextMenu=\{keyboardEvent => \{ keyboardEvent\.preventDefault\(\); showMenu\(event, keyboardEvent\); \}\}/); // all-day pill
  assert.doesNotMatch(source, /onClick=\{\(\) => openEdit\(event\)\}/);
  // Both invokeMenu implementations (month view, hourly grid) open on Enter/Space.
  const occurrences = source.match(/if \(keyboardEvent\.key === 'Enter' \|\| keyboardEvent\.key === ' '\) \{ keyboardEvent\.preventDefault\(\); openEdit\(event\); return; \}/g);
  assert.equal(occurrences?.length, 2);
});

// The hourly grid's move/resize ghost previews render through the exact same TimedEventBody
// component as the real event — not a simplified stand-in — so a preview being dragged looks
// exactly like the event it's a preview of.
test('the move and resize ghost previews render through the same component as the real event', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /function TimedEventBody\(\{ event, t \}: \{ event: \{ summary\?: string \| null; starts_at\?: string \| number \| Date \| null; location\?: string \| null \}; t: TFunction \}\)/);
  assert.match(source, /<TimedEventBody event=\{event\} t=\{t\} \/>/);
  assert.match(source, /<TimedEventBody event=\{\{ summary: movePreview\.summary, starts_at: movePreview\.start, location: movePreview\.location \}\} t=\{t\} \/>/);
  assert.match(source, /<TimedEventBody event=\{\{ summary: resizePreview\.summary, starts_at: resizePreview\.start, location: resizePreview\.location \}\} t=\{t\} \/>/);
});

// < Today > — the Today button sits between the arrows, and looks exactly like them (same
// ghost variant) rather than standing out as the only filled button in the group.
test('the Today button matches the prev/next arrows and sits between them', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /<Button variant="ghost" onClick=\{\(\) => step\(-1\)\} aria-label=\{t\('calendar\.previous'\)\}>‹<\/Button>\s*<Button variant="ghost" className="calendar-today-btn" onClick=\{\(\) => setAnchor\(new Date\(\)\)\}>\{t\('calendar\.today'\)\}<\/Button>\s*<Button variant="ghost" onClick=\{\(\) => step\(1\)\} aria-label=\{t\('calendar\.next'\)\}>›<\/Button>/);
  // The width:28px rule sized for the single-character arrows must not also squeeze the
  // Today button's text.
  const css = await readFile(calendarCssPath, 'utf8');
  assert.match(css, /\.calendar-date-controls > \.ui-button-ghost:not\(\.calendar-today-btn\) \{ width: 28px;/);
});

// A drag highlight must clear whether or not it produced an event — including when the mouse
// button is released outside the browser window entirely, which never fires a `mouseup` on
// `document`. Both the month/all-day drag and the hourly grid drag need this fallback, or a
// drag ending off-window leaves the highlight stuck on until some unrelated re-render clears it.
test('a drag highlight always resets, even if released outside the window', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const dayDragHook = source.slice(source.indexOf('function useDayRangeDrag'), source.indexOf('function useHourGridDrag'));
  const hourDragHook = source.slice(source.indexOf('function useHourGridDrag'), source.indexOf('function useEventDragMove'));
  const eventMoveHook = source.slice(source.indexOf('function useEventDragMove'), source.indexOf('function useEventResizeDrag'));
  const eventResizeHook = source.slice(source.indexOf('function useEventResizeDrag'), source.indexOf('function useAllDayResizeDrag'));
  const allDayResizeHook = source.slice(source.indexOf('function useAllDayResizeDrag'), source.indexOf('function TimedEventBody'));
  for (const hook of [dayDragHook, hourDragHook, eventMoveHook, eventResizeHook, allDayResizeHook]) {
    assert.match(hook, /window\.addEventListener\('blur', /);
    assert.match(hook, /window\.removeEventListener\('blur', /);
  }
});

// Dragging (moving or resizing) a recurring occurrence is the same three-different-meanings
// problem as saving or deleting one — asking which part of the series to reschedule is the same
// scope choice, just reached from a drag instead of a click, and reuses the very same dialog.
test('dragging a recurring occurrence asks which part of the series to reschedule, via the same dialog as save/delete', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const moveEvent = \(event: CalendarFormEvent, start: Date, end: Date\): Promise<void> => \{/);
  assert.match(source, /const choosePendingMoveScope = \(scope: 'single' \| 'all'\) => \{/);
  assert.match(source, /\{pendingMove && <CalendarEditScopeDialog onSelect=\{choosePendingMoveScope\} onClose=\{\(\) => \{ setPendingMove\(null\); resolvePendingMove\(\); \}\} t=\{t\} \/>\}/);
  // 'all' strips the recurrenceId so the request lands on the series itself, not its
  // `/occurrence` endpoint — same convention as save's scope handling.
  assert.match(source, /recurrenceId: scope === 'all' \? undefined : \(event\.recurring \? event\.recurrence_id : undefined\),/);
});

// A plain click (mousedown+mouseup with no movement — what every click and double-click is)
// must never publish a highlight to state, even momentarily: the fix that stopped the drag
// highlight from getting stuck also has to stop it from ever flashing on in the first place, or
// double-clicking a month-view day to create an event could leave that brief flash looking like
// it never cleared. Same fix as the resize hooks: record the starting state in the ref only,
// and let the first real movement (extendDrag) be what publishes it.
test('a plain click on a month-view day never publishes a highlight, even momentarily', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const dayDragHook = source.slice(source.indexOf('function useDayRangeDrag'), source.indexOf('function useHourGridDrag'));
  const startDragBody = dayDragHook.slice(dayDragHook.indexOf('const startDrag'), dayDragHook.indexOf('const extendDrag'));
  assert.doesNotMatch(startDragBody, /setRange\(/);
  assert.match(startDragBody, /rangeRef\.current = /);
  const extendDragBody = dayDragHook.slice(dayDragHook.indexOf('const extendDrag'), dayDragHook.indexOf('const inRange'));
  assert.match(extendDragBody, /setRange\(rangeRef\.current\);/);
});

// A drag anywhere in the grid (to create, move, or resize) also drags the browser's own native
// text selection across the date numbers and event titles it passes over, unless the grid
// opts out. That selection is independent of React state, so it doesn't clear just because a
// drag's own React-driven highlight correctly does — it sits there selected until something
// else collapses it, reading as "the highlight didn't reset" even once the drag state has.
test('the calendar grid disables native text selection, so a drag never leaves a stray selection highlight behind', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const calendarSurface: CSSProperties = \{ overflow: 'auto', display: 'flex', flexDirection: 'column', minHeight: 0, background: 'var\(--bg-secondary\)', userSelect: 'none', WebkitUserSelect: 'none' \};/);
  // Both the month grid and the week/workweek grid render through it.
  const gridMounts = source.match(/style=\{\{ \.\.\.calendarSurface,/g);
  assert.equal(gridMounts?.length, 2);
});

// Neither `mouseup` nor `blur` fires for every way a drag can end — a browser context menu, an
// OS-level interrupt, or a native modal stealing the mouseup can all swallow it silently,
// leaving `draggingRef` stuck true with nothing left to ever call `finish`. This — not a bug in
// clearing the state itself — is what actually explained a highlight occasionally surviving a
// cancelled drag. Every drag hook self-corrects on the very next `mousemove` where the button
// is no longer actually held (`event.buttons === 0`), treating it as the release that got lost.
test('every drag hook self-corrects on the next mousemove if the mouseup that should have ended it was swallowed', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const dayDragHook = source.slice(source.indexOf('function useDayRangeDrag'), source.indexOf('function useHourGridDrag'));
  const hourDragHook = source.slice(source.indexOf('function useHourGridDrag'), source.indexOf('function useEventDragMove'));
  const eventMoveHook = source.slice(source.indexOf('function useEventDragMove'), source.indexOf('function useEventResizeDrag'));
  const eventResizeHook = source.slice(source.indexOf('function useEventResizeDrag'), source.indexOf('function useAllDayResizeDrag'));
  const allDayResizeHook = source.slice(source.indexOf('function useAllDayResizeDrag'), source.indexOf('function TimedEventBody'));
  for (const hook of [dayDragHook, hourDragHook, eventMoveHook, eventResizeHook, allDayResizeHook]) {
    assert.match(hook, /event\.buttons === 0/);
  }
});

// Navigating to a different month/week makes any drag against the *old* period meaningless — a
// hard clear on top of (not instead of) everything else already keeping drag state from
// sticking, since letting stale state carry across a navigation is never correct regardless of
// the underlying cause. Every hook exposes a `reset()`, and both grids call all of theirs
// whenever the visible range actually changes (not on every render — keyed on the first/last
// visible day as strings, not the `days` array itself, which is a new reference every render).
test('navigating to a different period hard-resets every drag hook in both grids', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const hookNames = ['useDayRangeDrag', 'useHourGridDrag', 'useEventDragMove', 'useEventResizeDrag', 'useAllDayResizeDrag', 'TimedEventBody'];
  for (let i = 0; i < hookNames.length - 1; i++) {
    const hook = source.slice(source.indexOf(`function ${hookNames[i]}`), source.indexOf(`function ${hookNames[i + 1]}`));
    assert.match(hook, /const reset = \(\) => \{/, `${hookNames[i]} has no reset()`);
  }
  assert.match(source, /const rangeKey = `\$\{days\[0\]\?\.toDateString\(\)\}\|\$\{days\[days\.length - 1\]\?\.toDateString\(\)\}`;/);
  const rangeKeyEffects = source.match(/\}, \[rangeKey\]\);/g);
  assert.equal(rangeKeyEffects?.length, 2, 'expected one rangeKey effect in CalendarGrid and one in TimeGrid');
});

// Navigating away is one way a drag's state can go stale, but not the only one: a move/resize
// whose `onComplete` promise never actually settles (a request that hangs, or a recurring-scope
// dialog the user navigated away from without answering) leaves nothing to ever call
// `commit(null)` for it, so its preview and its "hide the real segment here" filter stay stuck
// indefinitely — reappearing, misaligned, against whatever the *next* fetch's data happens to
// be. `dayEventsFor` is a brand new function reference exactly when the underlying event data
// genuinely changed (a completed edit's refetch, a sync landing, a visibility toggle) and stays
// referentially stable otherwise, so resetting on it too catches this independently of
// navigation.
test('the month grid also hard-resets every drag hook whenever the underlying event data itself changes, not only on navigation', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const dayEventsForEffects = source.match(/\}, \[dayEventsFor\]\);/g);
  assert.equal(dayEventsForEffects?.length, 1, 'expected one dayEventsFor-keyed reset effect in CalendarGrid');
});

// `.cal-btn:active` shrinking on press is fine for an ordinary button, but every calendar event
// is a drag target — `:active` applies for as long as the mouse stays down, which is also
// exactly when a drag is or might be in progress, and CSS has no way to only apply it to "a
// press that turns out to be just a click". A multi-day event's day cells each render their own
// separate `.cal-ev` segment, so scaling down only the one under the pointer shrank just that
// segment relative to its still full-size siblings — screenshot-confirmed by a real user, and
// reproduced in e2e/calendar-shrink-repro.spec.ts — breaking the "one continuous bar" look the
// moment it was pressed, before any movement even happened.
test('calendar events never shrink on `:active` — every other press affordance (.cal-btn) may, but a multi-day event\'s segments must all stay the same size the whole time one is held down', async () => {
  const css = await readFile(indexCssPath, 'utf8');
  assert.doesNotMatch(css, /\.calendar-page \.cal-ev:active/);
  assert.match(css, /\.calendar-page \.cal-btn:active:not\(:disabled\) \{ transform: scale\(0\.97\); \}/);
});

// Clearing a drag hook's preview the moment the mouse is released (before the save that drag
// triggers has actually finished) let the moved/resized event flicker back to its old spot for
// however long the request took, then jump to the new one once it landed — reported directly by
// a user as "flickering/hectic movement". Each of the three hooks a real move/resize goes
// through instead keeps its last preview value up across the async `onComplete`, only clearing
// once it settles — and guards that clear by identity (`=== pending`) so an in-flight save from
// an *earlier* drag can never wipe out a newer one's still-active preview if a second drag
// starts before the first one's request finishes.
test('the move/resize preview stays visible until the save it triggers actually completes, not immediately on mouseup', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const moveHook = source.slice(source.indexOf('function useEventDragMove'), source.indexOf('function useEventResizeDrag'));
  const resizeHook = source.slice(source.indexOf('function useEventResizeDrag'), source.indexOf('function useAllDayResizeDrag'));
  const allDayResizeHook = source.slice(source.indexOf('function useAllDayResizeDrag'), source.indexOf('function TimedEventBody'));
  assert.match(moveHook, /if \(movingRef\.current === pending\) commit\(null\);/);
  assert.match(resizeHook, /if \(resizeRef\.current === pending\) commit\(null\);/);
  assert.match(allDayResizeHook, /if \(resizeRef\.current === pending\) commit\(null\);/);
  for (const hook of [moveHook, resizeHook, allDayResizeHook]) {
    assert.match(hook, /Promise\.resolve\(onCompleteRef\.current\(pending\.event, /);
  }
  // moveEvent's own returned promise is what these `.finally()`s actually wait on — for a
  // recurring occurrence, that promise doesn't resolve until the scope dialog is answered (or
  // cancelled), not the instant the dialog opens.
  assert.match(source, /const pendingMoveResolveRef = useRef<\(\(\) => void\) \| null>\(null\);/);
  assert.match(source, /return new Promise<void>\(resolve => \{/);
  assert.match(source, /void commitMove\(event, start, end, scope\)\.finally\(resolvePendingMove\);/);
});

// `sortedDayEvents`' own comparator (real events are already sorted by it) compares full start
// instants, which is correct there since every event it compares is already sitting on the day
// it's rendered for. A synthetic preview being inserted is being dragged onto a *different* day
// than the one its own (unmoved) `starts_at` still names, so comparing full instants would
// compare against that stale day instead of the target cell's other events' actual times — an
// event dragged forward from an earlier date would always sort first there regardless of either
// one's real clock time. Comparing minutes-since-midnight instead orders purely by time of day.
test('the drag preview\'s insertion point for two timed events compares time-of-day, not the dragged event\'s stale original day', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const minutesOfDay = \(event: CalendarFormEvent\) => \{ const d = new Date\(String\(event\.starts_at \?\? event\.startsAt \?\? 0\)\); return d\.getHours\(\) \* 60 \+ d\.getMinutes\(\); \};/);
  assert.match(source, /const timeDiff = minutesOfDay\(a\) - minutesOfDay\(b\);/);
});

// A month cell shows at most 3 events before capping to a "+N more" chip — clicking it used to
// just select the day, which on desktop only updates the sidebar's own single-day agenda (easy
// to miss and not actually "more" of anything). It now switches to the dedicated Agenda view,
// which lists every event on every day without a per-cell cap, and scrolls straight to that
// day's own section there.
// "+N more" selects the day exactly like clicking its date chip does (same onSelectDay path,
// same anchor/data-selected behavior everywhere else already relies on) — it does not switch
// the view. On desktop, where the side agenda rail can be hidden independently of the compact
// layout, it's also revealed if the viewer had hidden it, since that rail is what actually shows
// every event on that day past the per-cell cap; compact mode already opens its own day-panel
// dialog via the same selectDay it reuses.
test('"+N more" on a month-view cell selects the day like usual, and reveals the side agenda rail (desktop) if it was hidden', async () => {
  const source = await readFile(calendarPath, 'utf8');
  assert.match(source, /const openDayAgenda = \(day: Date\) => \{ selectDay\(day\); if \(!compact && agendaHidden\) setAgendaHidden\(false\); \};/);
  assert.match(source, /onClick=\{\(\) => \(onOpenDayAgenda \?\? onSelectDay\)\(day\)\}/);
  assert.doesNotMatch(source, /setView\('agenda'\)/);
});

// The week view's sticky day-header (and the all-day band under it) sits at a fixed z-index,
// but "sticky" alone doesn't guarantee staying visually on top of everything scrolled beneath
// it — a later sibling in the DOM with an equal-or-higher z-index paints over it wherever their
// boxes overlap on screen, regardless of one being sticky. Dragging a timed event's resize
// handle across several day boundaries gives the days strictly in between a full 24-hour-tall
// preview block (the event now genuinely spans those full days) whose top, once scrolled past,
// can land at the same screen position as the header — so the header's own z-index has to stay
// strictly above every one of those absolutely-positioned things in the scrollable grid below
// it (move/resize previews at 3, the now-line at 4), not just above the un-scrolled event cards.
test('the week view\'s sticky day header stays above every absolutely-positioned thing that scrolls beneath it', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const headerMatch = source.match(/<div style=\{\{ position: 'sticky', top: 0, zIndex: (\d+), background: 'var\(--bg-secondary\)' \}\}>/);
  assert.ok(headerMatch, 'sticky week-view header not found');
  const headerZIndex = Number(headerMatch[1]);
  assert.ok(headerZIndex > 4, `sticky header zIndex (${headerZIndex}) must stay above the now-line (4) and move/resize previews (3)`);
});

// A full-text search across every event (not just the visible range) sits to the left of the
// month/week/day selector. It must debounce (not fire one request per keystroke), reveal the
// agenda pane (or the compact day sheet) the moment a search begins rather than waiting for the
// first response — there would be nowhere else for results to land — and hand its results to
// CalendarAgenda rather than building its own separate result list.
test('the calendar search box sits left of the view selector, debounces, and reveals the agenda pane', async () => {
  const source = await readFile(calendarPath, 'utf8');
  const searchIndex = source.indexOf('className="calendar-search"');
  const segmentsIndex = source.indexOf('className="calendar-segments"');
  const selectIndex = source.indexOf('data-testid="calendar-view-select"');
  assert.ok(searchIndex > 0, 'calendar-search box not found');
  assert.ok(searchIndex < segmentsIndex && searchIndex < selectIndex, 'search box must render before the view selector');
  assert.match(source, /}, 300\);/); // the debounce delay
  assert.match(source, /if \(compact\) setDayPanelOpen\(true\); else setAgendaHidden\(false\);/);
  assert.match(source, /api\.calendar\.searchEvents\(trimmed, \{ signal: controller\.signal \}\)/);
  assert.match(source, /searchQuery: searchQuery\.trim\(\), searchResults, searchError, onClearSearch: clearSearch/);
});

// Every locale that ships the search box's own strings — not just English — so a non-English
// viewer sees results described in their own language, same bar every other calendar string
// added this way (see "every locale explains the three delete scopes") already holds to.
test('every locale provides the calendar search strings', async () => {
  const locales = (await readdir(localesPath)).filter(name => name.endsWith('.json'));
  assert.equal(locales.length, 9);
  for (const name of locales) {
    const strings = JSON.parse(await readFile(new URL(name, localesPath), 'utf8'));
    for (const key of ['searchEvents', 'searchResultsCount', 'noSearchResults', 'clearSearch']) {
      assert.equal(typeof strings.calendar[key], 'string', `${name} is missing calendar.${key}`);
      assert.ok(strings.calendar[key].length > 0, `${name} has an empty calendar.${key}`);
    }
  }
});
