import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// Reproduces a user-reported bug: dragging a multi-day event far enough that it crosses from the
// last column of one calendar row (Sunday) into the first column of the next (Monday) — a real
// gesture, only visible mid-drag, that none of the earlier same-row drag tests exercised. There's
// no cell to bridge into past a row's own edge (the grid just wraps to a new line), so squaring
// that edge off (or bleeding the bar's negative margin past it, same as a genuine mid-row
// continuation) poked the bar out past the grid's own right border instead of reading as
// "continues on the next row".
test('month view: a drag preview crossing from the last column of one row into the first of the next rounds off at the row edge instead of bleeding past it', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM layout behavior, only visible mid-drag');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    // A 4-day event starting on a Tuesday (2026-09-15) — dragged forward 5 days lands on
    // 2026-09-20 (Sunday, the last column of its row) through 2026-09-23 (Wednesday, into the
    // next row).
    return route.fulfill({ json: { events: [
      { id: '123', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: '123', all_day: true, starts_at: '2026-09-15T00:00:00Z', ends_at: '2026-09-19T00:00:00Z' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.locator('[data-testid="calendar-month-grid"] .cal-ev', { hasText: '123' }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 5 * (box.width + 1), box.y + box.height / 2, { steps: 10 });

  const cells = page.locator('[data-testid="calendar-month-grid"] .cal-cell');
  const cellCount = await cells.count();
  const byDate: Record<string, { text: string; radius: string; titleVisible: boolean | null }[]> = {};
  for (let i = 0; i < cellCount; i += 1) {
    const cell = cells.nth(i);
    const dateLabel = (await cell.locator('.calendar-day-select').first().textContent().catch(() => ''))?.trim() ?? '';
    const rows = await cell.evaluate(el => Array.from(el.querySelectorAll('[data-testid="calendar-month-move-preview"] > div')).map(card => ({
      text: card.textContent ?? '',
      radius: getComputedStyle(card).borderRadius,
      titleVisible: card.querySelector('span') ? getComputedStyle(card.querySelector('span')!).visibility !== 'hidden' : null,
    })));
    if (rows.length) byDate[dateLabel] = rows;
  }
  await page.mouse.up();

  // Row-end (Sunday the 20th) is also the preview's own genuine first day here, so it's already
  // rounded on the left; what this guards is the *right* edge — clamped rounded (not squared or
  // bled past the grid) because there's no cell in this row to its right to bridge into. All
  // four corners round out to the same radius, which getComputedStyle reports as the shorthand.
  expect(byDate['20']?.[0]?.radius).toBe('4px');
  // Row-start (Monday the 21st): rounded on its left/start edge too, and shows the title again —
  // a fresh bar visually restarting the new row, not a hidden mid-bar continuation — while its
  // right edge stays squared, genuinely continuing to Tuesday the 22nd within this same row.
  expect(byDate['21']?.[0]?.radius).toBe('4px 0px 0px 4px');
  expect(byDate['21']?.[0]?.titleVisible).toBe(true);
});
