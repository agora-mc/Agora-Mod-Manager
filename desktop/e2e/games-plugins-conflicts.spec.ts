import { test, expect, type Page } from '@playwright/test';
import {
  expectNoUnmockedCommands,
  instanceHeaderReplies,
  installGamesMock,
  OTHER_SLICES_REPLIES,
} from './support/games-mock';

/**
 * The conflicts card (MASTER_SPEC §31.7): a pair is let go with a rule and a move, the pair leaves
 * the lists, and Undo puts both back. The screenshot is kept for the owner to look at.
 */

const INSTANCE_ID = 'skyrim-conflicts-1';

const skyrimInstall = {
  installId: 'steam-489830',
  store: 'steam',
  name: 'Skyrim Special Edition',
  location: 'D:/Games/Skyrim Special Edition',
  storeVersion: '1.6.1170',
  storeBuild: null,
  addOnCount: 0,
  installBytes: null,
  runtime: {
    status: 'identified',
    runtime: { game: 'skyrim-se', store: 'steam', version: '1.6.1170', build: null },
    source: 'executable',
  },
};

const BOOT_REPLIES = {
  ...OTHER_SLICES_REPLIES,
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
  games_mod_inis_list: [],
  games_mod_ini_keep_game_version: [],
  games_mod_ini_restore_mine: [],
  plugins_emergency_stop: true,
  list_plugins: [],
  list_plugin_contributions: [],
  get_connectivity_status: { state: 'unknown', last_outcome_at: null },
  list_recent_play: [],
  games_list: { games: [{ gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [skyrimInstall] }] },
};

function plugin(overrides: Record<string, unknown>) {
  return {
    position: 1,
    name: 'Mod.esp',
    implicit: false,
    active: true,
    present: true,
    managed: false,
    locked: false,
    badges: [],
    masters: [],
    ...overrides,
  };
}

const heavy = plugin({ position: 1, name: 'Heavy Boots.esp' });
const moon = plugin({ position: 2, name: 'Moon Jump.esp' });
const rock = plugin({ position: 3, name: 'Rock.esp' });

const pair = (
  winner: string,
  loser: string,
  count: number,
  labels: string[],
  newSinceLastLaunch = false,
) => ({ winner, loser, count, labels, newSinceLastLaunch });

const before = {
  pairs: [
    pair('Heavy Boots.esp', 'Moon Jump.esp', 2, ['GMST fJumpHeightMin', 'INFO MoonGravity'], true),
    pair('Rock.esp', 'Moon Jump.esp', 1, ['MISC RockWeight'], false),
  ],
  patched: 2,
  decided: 0,
  implicitPairs: 1,
  notes: [],
};

const after = { pairs: [], patched: 2, decided: 1, implicitPairs: 1, notes: [] };

function listed(overrides: Record<string, unknown> = {}) {
  return {
    supported: true,
    listId: 'plugins',
    listName: 'Plugins',
    rulesKind: 'script',
    intro: 'Read the list from the top down.',
    itemHeading: 'Plugin',
    hasSorterUpdate: false,
    sorterUpdateLabel: null,
    rulesApply: true,
    listExists: true,
    rows: [heavy, moon, rock],
    findings: [],
    notes: [],
    notesOutOfDate: false,
    masterlist: null,
    hasOverlapsReader: true,
    overlaps: before,
    overlapsOutOfDate: false,
    ...overrides,
  };
}

async function openPlugins(page: Page, replies: Record<string, unknown>) {
  await installGamesMock(page, {
    replies: {
      ...BOOT_REPLIES,
      ...instanceHeaderReplies(INSTANCE_ID),
      games_plugins_lists: { lists: [{ id: 'plugins', name: 'Plugins', rulesKind: 'script' }] },
      games_plugins_list: listed(),
      games_plugins_rules: { rules: [{ item: 'Heavy Boots.esp', after: 'Moon Jump.esp', unused: null }] },
      ...replies,
    },
    settings: { active_game: 'skyrim-se' },
  });
  await page.goto('/');
  await page.evaluate(
    (dest) => window.history.pushState({ __agora: dest, __agoraDepth: 1 }, ''),
    { type: 'game-instance', gameId: 'skyrim-se', instanceId: INSTANCE_ID, page: 'plugins' },
  );
  await page.reload();
}

test.afterEach(async ({ page }) => {
  await expectNoUnmockedCommands(page);
});

test('lets the loser win with a rule and a move, then Undo puts both back', async ({ page }) => {
  await openPlugins(page, {
    games_plugins_conflict_action: {
      rules: [{ item: 'Moon Jump.esp', after: 'Heavy Boots.esp', unused: null }],
      plugins: listed({
        rows: [
          { ...heavy, position: 1 },
          { ...moon, position: 2 },
          { ...rock, position: 3 },
        ],
        overlaps: after,
      }),
      overlaps: after,
      overlapsError: null,
    },
    games_plugins_rule_remove: {
      changed: true,
      rules: [],
      overlaps: before,
      overlapsError: null,
    },
    games_plugins_move: {
      report: { plugin: 'Moon Jump.esp', from: 2, to: 2, written: false },
      plugins: listed({
        rows: [
          { ...heavy, position: 1 },
          { ...moon, position: 2 },
          { ...rock, position: 3 },
        ],
        overlaps: before,
      }),
    },
    games_plugins_conflicts: before,
  });

  const card = page.getByTestId('plugins-conflicts');
  await expect(card.getByTestId('conflict-pair')).toHaveCount(2);
  await expect(card).toContainText('Heavy Boots.esp wins over Moon Jump.esp · 2 records');
  await expect(card).toContainText('New');
  await card.screenshot({ path: '../.spike/out/conflicts-card.png' });

  await card.getByRole('button', { name: 'Let Moon Jump.esp win' }).first().click();
  await expect(page.getByTestId('plugins-conflict-notice')).toContainText(
    'Let Moon Jump.esp win. It now loads after Heavy Boots.esp.',
  );
  await expect(card.getByTestId('conflict-pair')).toHaveCount(0);

  await page.getByTestId('plugins-conflict-notice').getByRole('button', { name: 'Undo' }).click();
  await expect(page.getByRole('status')).toContainText('Put Moon Jump.esp back and removed the rule.');
  await expect(card.getByTestId('conflict-pair')).toHaveCount(2);
});
