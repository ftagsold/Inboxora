import { test, expect } from './fixtures.ts';

test.describe('URL routing for account folders, calendar and contacts', () => {
  test('canonicalizes the root, pushes views and restores them with Back/Forward', async ({ page, fixtureApi }, testInfo) => {
    test.skip(testInfo.project.name !== 'chromium-desktop', 'desktop owns browser history; mobile Back stays with the overlay stack');
    await fixtureApi;
    await page.goto('/', { waitUntil: 'domcontentloaded' });
    await expect(page).toHaveURL(/\/mail\/all\/INBOX$/);

    await page.locator('[data-testid="calendar-nav-primary"]:visible').click();
    await expect(page).toHaveURL(/\/calendar$/);
    await expect(page.locator('[data-testid="calendar-search-input"]')).toBeVisible();

    await page.goBack();
    await expect(page).toHaveURL(/\/mail\/all\/INBOX$/);
    await expect(page.locator('[data-testid="calendar-search-input"]')).toHaveCount(0);

    await page.goForward();
    await expect(page).toHaveURL(/\/calendar$/);
    await expect(page.locator('[data-testid="calendar-search-input"]')).toBeVisible();
  });

  test('opens a deep-linked calendar, contacts view and account folder', async ({ page, fixtureApi }) => {
    await fixtureApi;
    await page.goto('/calendar', { waitUntil: 'domcontentloaded' });
    await expect(page.locator('[data-testid="calendar-view-select"], [role="group"].calendar-segments').first()).toBeVisible();
    await expect(page).toHaveURL(/\/calendar$/);

    await page.goto('/contacts', { waitUntil: 'domcontentloaded' });
    await expect(page.locator('[data-testid="contacts-list-scroll"]:visible').first()).toBeVisible();
    await expect(page).toHaveURL(/\/contacts$/);

    const folders = page.waitForRequest(request => new URL(request.url()).pathname === '/api/mail/messages'
      && new URL(request.url()).searchParams.get('accountId') === 'account-gmail');
    await page.goto('/mail/account-gmail/INBOX', { waitUntil: 'domcontentloaded' });
    await folders;
    await expect(page).toHaveURL(/\/mail\/account-gmail\/INBOX$/);
  });

  test('ignores an unknown account in the URL and falls back to the unified inbox', async ({ page, fixtureApi }) => {
    await fixtureApi;
    await page.goto('/mail/no-such-account/INBOX', { waitUntil: 'domcontentloaded' });
    await expect(page).toHaveURL(/\/mail\/all\/INBOX$/);
  });
});
