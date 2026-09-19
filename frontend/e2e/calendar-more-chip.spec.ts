import { test, expect } from './fixtures.ts';
import { setupV3, navigateModule } from './v3-fixtures.ts';

function makeEvents(count: number) {
  return Array.from({ length: count }, (_, index) => ({
    id: `e${index + 1}`,
    calendar_id: 'calendar-personal',
    calendar_color: '#c0392b',
    source: 'local',
    read_only: false,
    summary: `Event ${index + 1}`,
    starts_at: `2026-09-15T${String(8 + index).padStart(2, '0')}:00:00Z`,
    ends_at: `2026-09-15T${String(9 + index).padStart(2, '0')}:00:00Z`,
  }));
}

const sixEvents = makeEvents(6);
const sevenEvents = makeEvents(7);

test('month view: a cell with exactly 6 events shows all 6, no "+N more" chip', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: sixEvents } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const visibleTitles = await page.locator('[data-testid="calendar-month-grid"] .cal-ev').allTextContents();
  expect(visibleTitles).toHaveLength(6);
  await expect(page.locator('.calendar-more')).toHaveCount(0);
});

test('month view: a cell with 7 events shows 5 plus a "+2 more" chip', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: sevenEvents } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  const visibleTitles = await page.locator('[data-testid="calendar-month-grid"] .cal-ev').allTextContents();
  expect(visibleTitles).toHaveLength(5);
  await expect(page.locator('.calendar-more').first()).toHaveText(/2/);
});

test('month view: "+N more" selects the day like usual and reveals the side agenda rail if hidden', async ({ page, fixtureApi }, testInfo) => {
  test.skip(testInfo.project.name !== 'chromium-desktop', 'real DOM behavior');
  // Wide enough that the calendar page's own surface (window width minus the app nav and the
  // calendar's own mini-month rail) clears the 1100px compact-layout threshold — the desktop,
  // non-compact side agenda rail this test is about only exists above that width.
  await page.setViewportSize({ width: 1800, height: 900 });
  await fixtureApi; await setupV3(page);
  await page.route('**/api/calendar/events**', route => {
    if (route.request().method() !== 'GET') return route.fallback();
    return route.fulfill({ json: { events: sevenEvents } });
  });
  await page.goto('/'); await navigateModule(page, 'calendar');
  await page.getByRole('button', { name: 'Miesiąc', exact: true }).click();
  // Hide the side agenda rail first, so the chip's job of un-hiding it is meaningfully tested.
  const toggle = page.getByTestId('calendar-toggle-agenda');
  await toggle.click();
  await expect(page.locator('aside.calendar-agenda')).toHaveCount(0);
  const chip = page.locator('.calendar-more').first();
  await expect(chip).toBeVisible();
  await chip.click();
  // Still month view, not switched to Agenda.
  await expect(page.getByTestId('calendar-month-grid')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Agenda', exact: true })).toHaveAttribute('aria-pressed', 'false');
  // The day got selected exactly like clicking its date chip would.
  await expect(page.locator('[data-testid="calendar-month-grid"] .cal-cell[data-selected="true"]')).toContainText('15');
  // And the side rail is back, showing that day.
  const rail = page.locator('aside.calendar-agenda');
  await expect(rail).toBeVisible();
  await expect(rail.getByText('Event 7', { exact: false })).toBeVisible();
});
