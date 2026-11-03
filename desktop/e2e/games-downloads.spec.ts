import { test, expect, type Page } from '@playwright/test';
import { expectNoUnmockedCommands, installGamesMock } from './support/games-mock';

/**
 * The Library's "New downloads" section: a recognised archive is added through its game's own
 * path, or ignored by hash. Every command is answered from the games mock table.
 */

const download = {
  path: 'C:\\Users\\me\\Downloads\\JourneyMap-32274-5.9.7-1699999999.jar',
  fileName: 'JourneyMap-32274-5.9.7-1699999999.jar',
  size: 2 * 1024 * 1024,
  sha256: 'a'.repeat(64),
  gameId: 'minecraft',
  kind: 'mod',
  reason: 'has fabric.mod.json',
  nexusModId: '32274',
  alreadyInLibrary: false,
};

const ignored = {
  ...download,
  path: 'C:\\Users\\me\\Downloads\\Plain.zip',
  fileName: 'Plain.zip',
  sha256: 'b'.repeat(64),
  gameId: 'skyrim-se',
  kind: 'game content',
  reason: '`Cool.esp` matches the game content layout',
  nexusModId: null,
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
  games_library_list: { items: [], damaged: [], warnings: [] },
  games_library_unused: { items: [], totalSize: 0, warnings: [] },
  games_library_instances: { instances: [], warnings: [] },
  games_library_icon: null,
  downloads_default_folder: null,
};

async function boot(page: Page, rows: unknown[]) {
  await page.route('**/*', (route) => {
    const url = route.request().url();
    if (url.startsWith('http://localhost') || url.startsWith('http://127.0.0.1')) {
      return route.continue();
    }
    return route.abort();
  });
  await installGamesMock(page, {
    replies: { ...BOOT_REPLIES, downloads_list: rows, games_list: { games: [] } },
  });
}

test.afterEach(async ({ page }) => {
  await expectNoUnmockedCommands(page);
});

test('the Library adds a recognised download and the badge goes away', async ({ page }) => {
  await boot(page, [download]);
  await page.goto('/');

  await expect(page.getByTestId('nav-badge-library')).toHaveText('1');
  await page.getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: /^Library/ })
    .click();

  const section = page.getByTestId('new-downloads');
  await expect(section).toBeVisible();
  await expect(section.getByTestId('new-download')).toHaveCount(1);
  await expect(section.getByText('has fabric.mod.json')).toBeVisible();
  await expect(section.getByText('Nexus mod 32274')).toBeVisible();

  await section.getByRole('button', { name: 'Add to library' }).click();

  await expect(page.getByRole('status')).toContainText(
    'Added JourneyMap-32274-5.9.7-1699999999.jar to your library.',
  );
  await expect(section.getByTestId('new-download')).toHaveCount(0);
});

test('the Library ignores a download by hash', async ({ page }) => {
  await boot(page, [ignored]);
  await page.goto('/');

  await page.getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: /^Library/ })
    .click();

  const section = page.getByTestId('new-downloads');
  await expect(section.getByTestId('new-download')).toHaveCount(1);
  await section.getByRole('button', { name: 'Ignore' }).click();

  await expect(section.getByTestId('new-download')).toHaveCount(0);
});
