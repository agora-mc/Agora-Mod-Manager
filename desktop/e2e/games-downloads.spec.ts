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
  kind: 'resource pack',
  reason: 'has pack.mcmeta and assets/',
  nexusModId: null,
};

/** A Skyrim download: Minecraft's Library does not list it. */
const forSkyrim = {
  ...download,
  path: 'C:/Users/me/Downloads/Cool.7z',
  fileName: 'Cool.7z',
  sha256: 'c'.repeat(64),
  gameId: 'skyrim-se',
  kind: 'game content',
  reason: '`Cool.esp` matches the game content layout',
  nexusModId: null,
};

const modpack = {
  ...download,
  path: 'C:/Users/me/Downloads/Fabulously Optimized.mrpack',
  fileName: 'Fabulously Optimized.mrpack',
  sha256: 'd'.repeat(64),
  kind: 'modpack',
  reason: 'has modrinth.index.json',
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
  await boot(page, [download, forSkyrim]);
  await page.goto('/');

  await expect(page.getByTestId('nav-badge-library')).toHaveText('1');
  await page.getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: /^Library/ })
    .click();

  const section = page.getByTestId('new-downloads');
  await expect(section).toBeVisible();
  // Only Minecraft's download: the Skyrim one waits in Skyrim's Library.
  await expect(section.getByTestId('new-download')).toHaveCount(1);
  await expect(section.getByText('It has fabric.mod.json.')).toBeVisible();
  await expect(section.getByTestId('new-download')).toContainText('Nexus mod 32274');

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


test('a modpack download opens the pack import review, not the library', async ({ page }) => {
  await boot(page, [modpack]);
  await page.addInitScript(() => {
    const target = window as unknown as {
      __TAURI_INTERNALS__: { invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown> };
    };
    const inner = target.__TAURI_INTERNALS__.invoke;
    target.__TAURI_INTERNALS__.invoke = (command: string, args: Record<string, unknown> = {}) => {
      if (command === 'plan_pack_import') {
        (window as unknown as { __planned?: unknown }).__planned = args.sourcePath;
        return Promise.resolve({
          pack_name: 'Fabulously Optimized',
          instance: {
            id: 'fo', name: 'Fabulously Optimized', minecraft_version: '1.21.4', loader: 'fabric', loader_version: '0.16.9',
          },
          entries: [],
          file_sha256: 'd'.repeat(64),
          file_size: 512,
          name_taken: false,
          suggested_name: 'Fabulously Optimized',
          left_out: [],
          problems: [],
          soft_problems: [],
          notes: [],
        });
      }
      return inner(command, args);
    };
  });
  await page.goto('/');

  await page.getByRole('navigation', { name: 'Main navigation' })
    .getByRole('button', { name: /^Library/ })
    .click();

  const section = page.getByTestId('new-downloads');
  await expect(section.getByTestId('new-download')).toContainText('Modpack');
  await expect(section.getByRole('button', { name: 'Add to library' })).toHaveCount(0);
  await section.getByRole('button', { name: 'Import as new instance…' }).click();

  await expect(page.getByRole('dialog')).toBeVisible();
  expect(await page.evaluate(() => (window as unknown as { __planned?: unknown }).__planned))
    .toBe(modpack.path);
});
