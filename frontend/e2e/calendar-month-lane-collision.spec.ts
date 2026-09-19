import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// Reproduces a user-reported bug: dragging a multi-day event onto cells that already contain a
// *different* multi-day event broke the pre-existing event's own continuous bar. The preview
// was reusing the dragged event's own pre-drag lane number unconditionally, without checking
// whether that lane was already occupied by something else at the *target* position — so moving
// event A onto event B's days landed A's preview directly in B's own lane, stacking on top of
// (covering) B instead of taking a lane of its own. Fixed by folding the ghost's and the
// preview's date ranges into one combined item before computing each week's lane assignments, so
// the packer sees the full extent of where the dragged event still visually is *and* where it's
// about to be, and routes it around any other real event either range would collide with.
test('month view: dragging a multi-day event onto cells that already contain a different multi-day event keeps both bars intact and continuous, in separate lanes', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM layout behavior, only visible mid-drag');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [
      // Dragged: spans 7-9 (3 days) — will be dragged forward 5 days onto 12-14.
      { id: 'dragged', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'dragged', all_day: true, starts_at: '2026-09-07T00:00:00Z', ends_at: '2026-09-10T00:00:00Z' },
      // Other: already sits alone on 12-14 (its own lane 0 there) before the drag arrives.
      { id: 'other', calendar_id: 'calendar-personal', calendar_color: '#35793a', source: 'local', read_only: false, summary: 'other', all_day: true, starts_at: '2026-09-12T00:00:00Z', ends_at: '2026-09-15T00:00:00Z' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();

  const event = page.locator('[data-testid="calendar-month-grid"] .cal-ev', { hasText: 'dragged' }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 5 * (box.width + 1), box.y + box.height / 2, { steps: 10 });

  const cells = page.locator('[data-testid="calendar-month-grid"] .cal-cell');
  const cellCount = await cells.count();
  const topsByDate: Record<string, { other?: number; preview?: number }> = {};
  for (let i = 0; i < cellCount; i += 1) {
    const cell = cells.nth(i);
    const dateLabel = (await cell.locator('.calendar-day-select').first().textContent().catch(() => ''))?.trim() ?? '';
    const cellBox = await cell.boundingBox();
    if (!cellBox) continue;
    const otherEl = cell.locator('.cal-ev', { hasText: 'other' }).first();
    const previewEl = cell.locator('[data-testid="calendar-month-move-preview"]').first();
    const otherBox = (await otherEl.count()) ? await otherEl.boundingBox() : null;
    const previewBox = (await previewEl.count()) ? await previewEl.boundingBox() : null;
    if (otherBox || previewBox) {
      topsByDate[dateLabel] = {
        other: otherBox ? Math.round(otherBox.y - cellBox.y) : undefined,
        preview: previewBox ? Math.round(previewBox.y - cellBox.y) : undefined,
      };
    }
  }
  await page.mouse.up();

  // "other" is on the same row throughout the 3 days the preview now overlaps it — never
  // knocked out of step by the incoming preview.
  const otherTops = ['12', '13', '14'].map(day => topsByDate[day]?.other);
  expect(otherTops.every(top => top === otherTops[0])).toBe(true);
  expect(otherTops[0]).not.toBeUndefined();

  // The preview is likewise on one consistent row of its own throughout.
  const previewTops = ['12', '13', '14'].map(day => topsByDate[day]?.preview);
  expect(previewTops.every(top => top === previewTops[0])).toBe(true);
  expect(previewTops[0]).not.toBeUndefined();

  // And the two don't collide — separate lanes, separate rows.
  expect(otherTops[0]).not.toBe(previewTops[0]);
});
