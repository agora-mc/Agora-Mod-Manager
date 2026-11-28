import { test, expect, type Page } from '@playwright/test';
import { expectNoUnmockedCommands, installGamesMock } from './support/games-mock';

/**
 * The "More games" card: someone can notice that Agora supports another game
 * and install it from Home, without opening the plugin manager. The install
 * asks for the same consent as any plugin, then explains the restart.
 *
 * The emergency stop is engaged here on purpose: an official game package is
 * data only, so core loads it regardless of the stop, and the card must not
 * depend on the ability to run plugin code either.
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
  plugins_emergency_stop: true,
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

/**
 * Answers the folder flow from a small stateful script. `games_list` is empty until a folder is
 * added, so the card and Settings both see "not found" first and "found" after.
 */
async function driveAddFolder(page: Page, config: {
  before: unknown[];
  after: unknown[];
  picked: string;
  added: { path: string; gameId: string; gameName: string };
}) {
  await page.addInitScript((script) => {
    const tauri = (window as unknown as {
      __TAURI_INTERNALS__: { invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown> };
    }).__TAURI_INTERNALS__;
    const original = tauri.invoke;
    let added = false;
    tauri.invoke = (command: string, args: Record<string, unknown> = {}) => {
      if (command === 'games_list') return Promise.resolve({ games: added ? script.after : script.before });
      if (command === 'pick_directory') return Promise.resolve(script.picked);
      if (command === 'games_add_folder') {
        added = true;
        return Promise.resolve(script.added);
      }
      return original(command, args);
    };
  }, config);
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
  await expect(page.getByText('Restart Agora to finish adding Skyrim Special Edition.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Restart now' })).toBeVisible();
});

test('a plugin with no install found can be pointed at its folder from Settings › Game support', async ({ page }) => {
  const skyrimInstall = {
    installId: 'direct-d-games-skyrim',
    store: 'direct',
    name: 'Skyrim Special Edition',
    location: 'D:/Games/Skyrim',
    storeVersion: null,
    storeBuild: null,
    addOnCount: 0,
    installBytes: null,
    runtime: { status: 'unidentified', reasons: ['The executable could not be read.'] },
  };
  await installGamesMock(page, {
    replies: {
      ...BOOT_REPLIES,
      list_official_plugins: { plugins: [skyrimPlugin], unavailableReason: null },
      games_support_list: [
        { gameId: 'minecraft', name: 'Minecraft: Java Edition', source: { kind: 'builtIn' }, enabled: true, canDisable: false },
        { gameId: 'skyrim-se', name: 'Skyrim Special Edition', source: { kind: 'plugin', pluginId: 'agora.skyrim' }, enabled: true, canDisable: true },
      ],
      // Settings reads these on the way to the Game support list.
      games_base_list: { bases: [], damaged: [] },
      is_portable_mode: false,
      msa_get_status: null,
      msa_list_accounts: [],
      credential_storage_status: null,
      get_auth_status: false,
    },
    sequences: {
      install_official_plugin: [
        { outcome: 'needsConsent', preview: consentPreview },
        { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition', version: '1.0.0' } },
      ],
    },
    settings: { active_game: 'minecraft' },
  });
  await driveAddFolder(page, {
    before: [],
    after: [{ gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [skyrimInstall] }],
    picked: 'D:/Games/Skyrim',
    added: { path: 'D:/Games/Skyrim', gameId: 'skyrim-se', gameName: 'Skyrim Special Edition' },
  });
  await page.goto('/');

  // Install from More games: the game is added, but discovery does not find it yet.
  await page.getByRole('button', { name: 'Install Skyrim Special Edition' }).click();
  await page.getByRole('button', { name: 'Install', exact: true }).click();
  await expect(page.getByText('Agora did not find Skyrim Special Edition on this computer. Add the folder it is in.')).toBeVisible();

  // The same player can finish the job from Settings › Game support.
  await page.getByRole('navigation', { name: 'Main navigation' }).getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tablist', { name: / pages$/ }).getByRole('tab', { name: 'Game support' }).click();
  await expect(page.getByText('Not found on this computer.')).toBeVisible();

  await page.getByRole('button', { name: 'Add game folder for Skyrim Special Edition' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Added D:/Games/Skyrim as Skyrim Special Edition.' })).toBeVisible();
  await expect(page.getByText('Found on this computer.')).toBeVisible();
});
