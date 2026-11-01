import { test, expect, type Page } from '@playwright/test';
import { expectNoUnmockedCommands, installGamesMock } from './support/games-mock';

/**
 * The "More games" card: someone can notice that Agora supports another game
 * and install it from Home, without opening the plugin manager. The install
 * asks for the same consent as any plugin, then explains the restart.
 *
 * The community-plugins switch is off here on purpose: official plugins are
 * exempt from it in core, and the card must not depend on it either.
 */

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
  plugins_enabled: false,
  list_plugins: [],
  list_plugin_contributions: [],
  get_connectivity_status: { state: 'unknown', last_outcome_at: null },
  list_recent_play: [],
  games_list: { games: [] },
  games_instance_list: { instances: [] },
};

const skyrimPlugin = {
  id: 'agora.skyrim',
  name: 'Skyrim Special Edition',
  description: 'Adds Skyrim Special Edition to Agora.',
  gameIds: ['skyrim-se'],
  version: '1.0.0',
  previousVersion: null,
  sizeBytes: 1024,
  minAgoraVersion: '1.0.0',
  state: 'notInstalled',
  installedVersion: null,
  canRollBack: false,
  installedStatus: null,
  problem: null,
};

const consentPreview = {
  manifest: {
    manifest: 1,
    id: 'agora.skyrim',
    name: 'Skyrim Special Edition',
    version: '1.0.0',
    description: 'Adds Skyrim Special Edition to Agora.',
    license: 'MIT',
  },
  requiredCapabilities: [
    { name: 'game:define', summary: 'Add a game to Agora', isMutating: false },
  ],
  optionalCapabilities: [],
  unsupportedCapabilities: [],
  replacesVersion: null,
  addedCapabilities: [],
  addedHosts: [],
  updateSource: null,
  migratesData: false,
  fileCount: 1,
  uncompressedBytes: 1024,
};

async function boot(page: Page) {
  await installGamesMock(page, {
    replies: {
      ...BOOT_REPLIES,
      list_official_plugins: { plugins: [skyrimPlugin], unavailableReason: null },
    },
    sequences: {
      install_official_plugin: [
        { outcome: 'needsConsent', preview: consentPreview },
        { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition', version: '1.0.0' } },
      ],
    },
    settings: { active_game: 'minecraft' },
  });
}

test.afterEach(async ({ page }) => {
  await expectNoUnmockedCommands(page);
});

test('Home shows a game to add, asks for consent, and says to restart', async ({ page }) => {
  await boot(page);
  await page.goto('/');

  const card = page.getByTestId('more-games');
  await expect(card).toBeVisible();
  await expect(card).toContainText('Skyrim Special Edition');

  await page.getByRole('button', { name: 'Install Skyrim Special Edition' }).click();
  await expect(page.getByText('It needs permission to:')).toBeVisible();
  await expect(page.getByText('Add a game to Agora')).toBeVisible();

  await page.getByRole('button', { name: 'Install', exact: true }).click();
  await expect(page.getByText('Restart Agora to add Skyrim Special Edition.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Restart now' })).toBeVisible();
});
