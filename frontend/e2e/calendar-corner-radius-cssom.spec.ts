import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

// `borderTopLeftRadius`/etc. on a month-view event's real `.cal-ev` button must be set
// unconditionally, every render — `eventCard`'s own `borderRadius` (the shorthand) always sets a
// value for every corner, and once a longhand like this has been set inline even once, the
// browser has lost the shorthand's own contribution to that corner. Gating the longhand overrides
// behind a `spanning` conditional (only applying them `spanning ? {...} : {}`) meant a corner that
// had ever been squared could get stuck squared even once it should round again, on the same
// persisted-across-renders DOM node — the same class of CSSOM bug this file already covers for
// `background`/`backgroundColor`. A user reported this directly: dragging a multi-day event kept
// its own ghost's corners wrong. The ghost now also never gets shortened by an overlapping preview
// (a separate, related fix — the two used to be tangled together) — it keeps rendering every one
// of its own real days, with the preview stacked on top wherever the two share one, so this test
// also doubles as confirmation that a busy overlapping drag doesn't corrupt the ghost's own
// unrelated, unmoved corners.
test('month view: a dragged multi-day event\'s ghost keeps its own correct corners at its true first/last day, unaffected by the overlapping preview', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSSOM behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  // A 4-day event ("123", 15th-18th) dragged forward by 1 day: the preview (16th-19th) overlaps
  // it on the 16th, 17th and 18th, stacking directly on top of the ghost there — the ghost itself
  // keeps showing on every one of its own 4 real days throughout.
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [{ id: '123', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: '123', all_day: true, starts_at: '2026-09-15T00:00:00Z', ends_at: '2026-09-19T00:00:00Z' }] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: '123' }).first();
  await expect(event).toBeVisible();
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + (box.width + 1), box.y + box.height / 2, { steps: 10 });

  const ghosts = page.locator('[data-testid="calendar-month-grid"] .cal-ev', { hasText: '123' });
  await expect(ghosts).toHaveCount(4);
  const corners = await ghosts.evaluateAll(elements => elements.map(el => ({
    tl: getComputedStyle(el).borderTopLeftRadius,
    tr: getComputedStyle(el).borderTopRightRadius,
    br: getComputedStyle(el).borderBottomRightRadius,
    bl: getComputedStyle(el).borderBottomLeftRadius,
  })));
  await page.mouse.up();

  // The 15th (true first day): rounded left, squared right (genuinely continues to the 16th,
  // which the ghost still shows — not shortened by the overlapping preview).
  expect(corners[0]).toEqual({ tl: '4px', tr: '0px', br: '0px', bl: '4px' });
  // The 16th and 17th (middle days): fully squared both sides.
  expect(corners[1]).toEqual({ tl: '0px', tr: '0px', br: '0px', bl: '0px' });
  expect(corners[2]).toEqual({ tl: '0px', tr: '0px', br: '0px', bl: '0px' });
  // The 18th (true last day): squared left, rounded right.
  expect(corners[3]).toEqual({ tl: '0px', tr: '4px', br: '4px', bl: '0px' });
});
