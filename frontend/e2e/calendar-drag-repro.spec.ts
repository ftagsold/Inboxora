import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// These drag interactions can't be caught by CalendarPageContract.test.ts's source-regex
// assertions — the bugs they guard against are real DOM/CSSOM behavior (a background reverting
// to fully transparent instead of its base color once a highlight is removed) that only shows up
// once the browser actually applies and un-applies the styles.

test('month view: a click-drag create highlight fully reverts once the dialog is cancelled', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSSOM behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  await expect(page.getByTestId('calendar-month-grid')).toBeVisible();
  const cells = page.locator('[data-testid="calendar-month-grid"] .cal-cell');
  const startCell = cells.nth(14);
  const endCell = cells.nth(17);
  const before = await startCell.evaluate(el => getComputedStyle(el).backgroundColor);
  const startBox = await startCell.boundingBox();
  const endBox = await endCell.boundingBox();
  await page.mouse.move(startBox.x + startBox.width / 2, startBox.y + startBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(endBox.x + endBox.width / 2, endBox.y + endBox.height / 2, { steps: 5 });
  const dialog = page.getByTestId('calendar-event-dialog');
  await page.mouse.up();
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Anuluj', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(startCell).toHaveCSS('background-color', before);
  await expect(endCell).toHaveCSS('background-color', before);
});

test('month view: pressing down on an existing event does not change its style before any movement', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSSOM behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const before = await event.evaluate(el => getComputedStyle(el).backgroundColor);
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(80);
  await expect(event).toHaveCSS('background-color', before);
  await page.mouse.up();
});

test('week view: pressing down on a timed event does not change its style before any movement', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSSOM behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Tydzień', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const before = await event.evaluate(el => getComputedStyle(el).backgroundColor);
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(80);
  await expect(event).toHaveCSS('background-color', before);
  await page.mouse.up();
});

test('month view: dragging a multi-day all-day event keeps the ghost and the preview each on one consistent row', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSSOM behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  // A writable multi-day all-day event — the fixture's only multi-day all-day event
  // ("Wyjazd zespołu") lives on the read-only CalDAV calendar and can't be dragged.
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [{ id: 'writable-multi-day', calendar_id: 'calendar-personal', calendar_color: '#35548a', source: 'local', read_only: false, summary: 'Warsztaty wyjazdowe', all_day: true, starts_at: '2026-09-14T00:00:00Z', ends_at: '2026-09-18T00:00:00Z' }] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: /Warsztaty wyjazdowe/ }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 240, box.y + box.height / 2, { steps: 10 });
  await page.screenshot({ path: testInfo.outputPath('multi-day-drag-mid.png') });
  // The dragged event itself is never swapped for a separate "ghost" element — it keeps
  // rendering through its normal per-day segments, just dimmed, so those segments (one per
  // day it still occupies) are what needs to land on one consistent row here. Continuation
  // segments hide their title visually (so it isn't repeated on every day), which also drops
  // it from the accessible name — querying by the shared wrapper's data-testid, not role/name,
  // catches every segment regardless.
  const ghostTops = await page.getByTestId('calendar-month-span').evaluateAll(els => els.map(el => el.getBoundingClientRect().top));
  const previewTops = await page.getByTestId('calendar-month-move-preview').evaluateAll(els => els.map(el => el.getBoundingClientRect().top));
  console.log('ghost tops', ghostTops, 'preview tops', previewTops);
  expect(new Set(ghostTops).size).toBe(1);
  expect(new Set(previewTops).size).toBe(1);
  await page.mouse.up();
});

test('month view: dragging a single-day timed event renders a ghost preview', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSSOM behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 120, box.y + box.height / 2 + 120, { steps: 10 });
  await expect(page.getByTestId('calendar-month-move-preview')).toBeVisible();
  await page.mouse.up();
});
