import { test, expect, type Page } from '@playwright/test';
import { expectNoUnmockedCommands, installGamesMock } from './support/games-mock';

/**
 * The offline Library: saved descriptions shown with the network off, and "Add to instance"
 * placing a stored item into a Minecraft instance. Every command is answered from the games mock
 * table, and every non-local request is aborted, so the page can only work from what is on disk.
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
  catalogStrategy: null,
  instances: [],
  fileCount: 1,
  totalSize: 2048,
  addedAtUnixMs: 1,
  pluginFiles: [],
  hasInstaller: false,
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
  games_library_unused: { items: [], totalSize: 0, warnings: [] },
  games_library_instances: {
    instances: [{ instanceId: 'mc1', name: 'My Minecraft', gameId: 'minecraft' }],
    warnings: [],
  },
  games_library_icon: null,
  minecraft_add_stored_content: {
    filename: 'saved.jar',
    registry_id: null,
    modrinth_id: null,
    source: 'manual',
    version: '1.0.0',
    sha256: 'a'.repeat(64),
    installed_at: '2026-01-01T00:00:00Z',
    enabled: true,
    content_type: 'mod',
    content_item_id: item.itemId,
    object_sha256: 'a'.repeat(64),
  },
};

async function bootOffline(page: Page) {
  // Nothing outside the preview server may load: the library must work from disk.
  await page.route('**/*', (route) => {
    const url = route.request().url();
    if (url.startsWith('http://localhost') || url.startsWith('http://127.0.0.1')) {
      return route.continue();
    }
    return route.abort();
  });
  await installGamesMock(page, {
    replies: { ...BOOT_REPLIES, games_list: { games: [] } },
  });
}

test.afterEach(async ({ page }) => {
  await expectNoUnmockedCommands(page);
});

test('the library shows a saved description and adds an item with the network off', async ({ page }) => {
  await bootOffline(page);
  await page.goto('/');

  await page.getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: 'Library', exact: true })
    .click();

  await expect(page.getByTestId('library-item')).toHaveCount(1);
  await expect(page.getByText('This description was saved to disk.')).toBeVisible();

  await page.getByRole('button', { name: /Add to instance/ }).first().click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Add to instance' }).click();

  await expect(page.getByRole('status')).toContainText('Added Saved Mod to My Minecraft.');
});
