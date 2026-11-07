import { test, expect, type Page } from '@playwright/test';
import { expectNoUnmockedCommands, installGamesMock } from './support/games-mock';

/**
 * The Library's unused-items review: it shows how much space nothing uses, lists those items, and
 * removes only the ones the user selects and confirms. Every command is answered from the games
 * mock table, and every non-local request is aborted.
 */

const item = {
  itemId: 'a'.repeat(64),
  name: 'Saved Mod',
  description: 'This description was saved to disk.',
  version: '1.0.0',
  gameId: 'minecraft',
  gameIds: ['minecraft'],
  pageUrl: null,
  iconSha256: null,
  sourceKind: 'file',
  sourceLabel: 'File: saved.jar',
  instances: [],
  fileCount: 1,
  totalSize: 2048,
  addedAtUnixMs: 1,
  pluginFiles: [],
  hasInstaller: false,
};

const unusedItem = {
  itemId: 'e'.repeat(64),
  name: 'Old Mod',
  totalSize: 2 * 1024 * 1024,
  gameId: 'minecraft',
  gameIds: ['minecraft'],
};

const BOOT_REPLIES = {
  get_windows_accent_color: null,
  get_registry_status: { has_cached_db: false, checked: true, message: 'Missing' },
  check_registry_update: { has_cached_db: false, checked: true, message: 'Missing' },
  list_instances: [],
  list_snapshots: [],
  for_you_items: [],
  query_launch_state: [],
  check_all_instance_health: [],
  take_pending_cli_launch: null,
  get_governance_config: null,
  plugins_emergency_stop: true,
  list_plugins: [],
  list_plugin_contributions: [],
  get_connectivity_status: { state: 'unknown', last_outcome_at: null },
  list_recent_play: [],
  games_instance_list: { instances: [] },
  games_library_list: { items: [item], damaged: [], warnings: [] },
  games_library_instances: { instances: [], warnings: [] },
  games_library_icon: null,
  games_content_remove: null,
};

async function bootOffline(page: Page, unusedReplies: unknown[] = [
  { items: [unusedItem], totalSize: unusedItem.totalSize, warnings: [] },
  { items: [], totalSize: 0, warnings: [] },
]) {
  await page.route('**/*', (route) => {
    const url = route.request().url();
    if (url.startsWith('http://localhost') || url.startsWith('http://127.0.0.1')) {
      return route.continue();
    }
    return route.abort();
  });
  await installGamesMock(page, {
    replies: { ...BOOT_REPLIES, games_list: { games: [] } },
    sequences: { games_library_unused: unusedReplies },
  });
}

test.afterEach(async ({ page }) => {
  await expectNoUnmockedCommands(page);
});

test('the library reviews unused items and removes only after confirmation', async ({ page }) => {
  await bootOffline(page);
  await page.goto('/');

  await page
    .getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: 'Library', exact: true })
    .click();

  const summary = page.getByTestId('unused-summary');
  await expect(summary).toContainText('Unused: 1 item, 2.0 MB');

  await page.getByRole('button', { name: 'Review unused items' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('Old Mod');
  await dialog.getByRole('checkbox').check();

  await dialog.getByRole('button', { name: 'Remove selected…' }).click();
  await expect(dialog).toContainText('Remove 1 item?');
  await dialog.getByRole('button', { name: 'Remove 1 item' }).click();

  await expect(page.getByRole('status')).toContainText('Removed 1 item.');
  await expect(summary).toHaveCount(0);
});

test('an unreadable instance makes the library offer nothing for removal', async ({ page }) => {
  await bootOffline(page, [
    {
      items: [],
      totalSize: 0,
      warnings: ["Instance 'mc-broken' could not be read: bad manifest"],
    },
  ]);
  await page.goto('/');

  await page
    .getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: 'Library', exact: true })
    .click();

  const summary = page.getByTestId('unused-summary');
  await expect(summary).toContainText('Unused items could not be checked');
  await expect(summary).toContainText("Instance 'mc-broken' could not be read");
  await expect(summary.getByRole('button', { name: 'Review unused items' })).toHaveCount(0);
});
