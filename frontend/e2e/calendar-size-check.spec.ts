import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

test('month view: the real card and its move preview are the same size', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const before = await event.evaluate(el => el.getBoundingClientRect().height);
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 120, box.y + box.height / 2 + 120, { steps: 10 });
  const preview = page.getByTestId('calendar-month-move-preview').locator('div').first();
  await expect(preview).toHaveCSS('height', `${before}px`);
  await page.mouse.up();
});

test('week view: a timed event\'s move preview matches its real overlap-aware column width', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Tydzień', exact: true }).click();
  const event = page.getByRole('button', { name: /Planowanie projektu/ }).first();
  await expect(event).toBeVisible();
  const before = await event.evaluate(el => el.getBoundingClientRect().width);
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  // A small move (15min) that stays within the same overlap group as the original.
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2 + 15, { steps: 5 });
  const preview = page.getByTestId('calendar-event-move-preview');
  await expect(preview).toHaveCSS('width', `${before}px`);
  await page.mouse.up();
});

test('week view: an all-day pill and its move preview are the same size', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: [{ id: 'writable-multi-day', calendar_id: 'calendar-personal', calendar_color: '#35548a', source: 'local', read_only: false, summary: 'Warsztaty wyjazdowe', all_day: true, starts_at: '2026-09-07T00:00:00Z', ends_at: '2026-09-09T00:00:00Z' }] } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Tydzień', exact: true }).click();
  const event = page.getByRole('button', { name: /Warsztaty wyjazdowe/ }).first();
  await expect(event).toBeVisible();
  const before = await event.evaluate(el => el.getBoundingClientRect().width);
  const box = await event.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width + 60, box.y + box.height / 2, { steps: 10 });
  const preview = page.getByTestId('calendar-allday-move-preview');
  await expect(preview).toHaveCSS('width', `${before}px`);
  await page.mouse.up();
});
