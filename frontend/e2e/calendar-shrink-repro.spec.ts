import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: a multi-day event\'s segments stay full size while the pointer holds one of them down', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSS behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  // Mirrors a real, busy calendar week: a 3-day event ("123") sharing its row with several other
  // all-day events on neighboring days, plus a timed event on its own first day.
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [
      { id: '123', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: '123', all_day: true, starts_at: '2026-09-15T00:00:00Z', ends_at: '2026-09-18T00:00:00Z' },
      { id: 'wirt-zu', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'Wirt zu', all_day: true, starts_at: '2026-09-14T00:00:00Z', ends_at: '2026-09-15T00:00:00Z' },
      { id: 'vorstellen', calendar_id: 'calendar-personal', calendar_color: '#c0392b', source: 'local', read_only: false, summary: 'Vorstellen 5. Klassen', starts_at: '2026-09-15T19:00:00Z', ends_at: '2026-09-15T20:00:00Z' },
      { id: 'papier', calendar_id: 'calendar-personal', calendar_color: '#8a6d3b', source: 'local', read_only: false, summary: 'Papier', all_day: true, starts_at: '2026-09-16T00:00:00Z', ends_at: '2026-09-17T00:00:00Z' },
      { id: 'birthday', calendar_id: 'calendar-personal', calendar_color: '#2e8b45', source: 'local', read_only: false, summary: 'Philipp Tagsold hat Geburtstag', all_day: true, starts_at: '2026-09-17T00:00:00Z', ends_at: '2026-09-18T00:00:00Z' },
    ] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const segments = page.locator('[data-testid="calendar-month-span"] .cal-ev');
  await expect(segments).toHaveCount(3);
  const before = await segments.evaluateAll(els => els.map(el => el.getBoundingClientRect().toJSON()));
  const box = await segments.first().boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  // A tiny move — not even enough to register as a real drag yet — while the CSS `:active`
  // press state is definitely engaged on the segment under the pointer.
  await page.mouse.move(box.x + box.width / 2 + 5, box.y + box.height / 2, { steps: 3 });
  const during = await segments.evaluateAll(els => els.map(el => el.getBoundingClientRect().toJSON()));
  await page.mouse.up();
  for (let i = 0; i < before.length; i += 1) {
    expect(during[i].width).toBeCloseTo(before[i].width, 1);
    expect(during[i].height).toBeCloseTo(before[i].height, 1);
  }
});

test('week view: a timed event does not shrink while the pointer holds it down', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM/CSS behavior, not just source shape');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Tydzień', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const before = await event.evaluate(el => el.getBoundingClientRect().toJSON());
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 5, box.y + box.height / 2, { steps: 3 });
  const during = await event.evaluate(el => el.getBoundingClientRect().toJSON());
  await page.mouse.up();
  expect(during.width).toBeCloseTo(before.width, 1);
  expect(during.height).toBeCloseTo(before.height, 1);
});
