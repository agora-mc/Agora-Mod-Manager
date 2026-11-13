import { test, expect, type Page } from '@playwright/test';
import path from 'node:path';
import { expectNoUnmockedCommands, installGamesMock } from './support/games-mock';

/**
 * The crash answer screen on a game instance's Overview: the card for the newest crashed session,
 * and the answer it opens. Every command is answered from the table; anything else fails the test.
 *
 * The screenshots go to `.spike/out/screens/`, which is excluded from the repository.
 */

const SCREENSHOT_DIR = path.resolve(process.cwd(), '..', '.spike', 'out', 'screens');

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

const skyrim = {
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  installs: [skyrimInstall],
};

const capabilities = {
  launch: true,
  stop: true,
  rename: true,
  clone: true,
  trash: true,
  openFolder: true,
  playTime: true,
};

const grounded = {
  instanceId: 'grounded-1',
  gameId: 'skyrim-se',
  name: 'Grounded',
  store: 'steam',
  version: '1.6.1170',
  pinned: true,
  deployment: null,
  lastLaunchedAt: '2026-11-12 21:10:00',
  lastPlayedAt: '2026-11-12 21:10:00',
  playTimeMs: 7_200_000,
  running: false,
  capabilities,
};

const groundedDetail = {
  instanceId: 'grounded-1',
  gameId: 'skyrim-se',
  gameName: 'Skyrim Special Edition',
  name: 'Grounded',
  pinned: true,
  store: 'steam',
  version: '1.6.1170',
  build: null,
  baseId: 'skyrim-se_steam_1.6.1170',
  installId: null,
  unpinnedReason: null,
  deployment: null,
  saves: 'shared',
  createdAt: '2026-10-01 10:00:00',
  lastLaunchedAt: '2026-11-12 21:10:00',
  lastPlayedAt: '2026-11-12 21:10:00',
  playTimeMs: 7_200_000,
  running: false,
  capabilities,
};

const cleanCheck = {
  instanceId: 'grounded-1',
  checked: true,
  status: 'ok',
  findings: [],
  passed: [],
};

const source = {
  plugin_id: 'agora.creation-engine',
  plugin_name: 'Creation Engine crash rules',
  plugin_version: '0.1.0',
  rule_id: 'stack_in_example_module',
  rule_version: 1,
};

const suspect = {
  owner: { kind: 'item', key: 'acme.example', name: 'Example Overhaul' },
  level: 'very_likely',
  score: 21,
  cause: 'mod',
  families: ['execution', 'known_signature'],
  sources: [source],
  save_risk: 'unknown',
};

const answer = {
  contract: 1,
  verdict: { kind: 'crashed', how: 'pid' },
  headline: {
    en: 'The game crashed, and Example Overhaul is very likely the cause.',
  },
  level: 'very_likely',
  cause: 'mod',
  candidates: [suspect],
  findings: [
    {
      scope: 'root_cause',
      level: 'very_likely',
      cause: 'mod',
      candidate: suspect.owner,
      headline: { en: 'Example Overhaul was running when the game stopped.' },
      source,
    },
    {
      scope: 'environment',
      level: 'likely',
      cause: 'missing_requirement',
      headline: { en: 'Another mod needs a framework that is not installed.' },
      source,
    },
    {
      scope: 'root_cause',
      level: 'possible',
      cause: 'version_mismatch',
      headline: { en: 'A plugin was built for a different game version.' },
      source,
    },
    {
      scope: 'environment',
      level: 'possible',
      cause: 'file',
      headline: { en: 'One mesh file could not be read.' },
      source,
    },
  ],
  inspections: [],
  could_not_check: [
    { reason: 'not_covered', subject: 'core.memory', source },
    { reason: 'code_failed', subject: 'crash_logger', detail: 'the parser timed out', source },
  ],
  trust_notes: [
    {
      text: { en: 'The log was tied to this session by its process id.' },
      source,
    },
  ],
  next: { kind: 'confirm_experiment', keys: ['acme.example'] },
  repairs: [],
  artifacts: [
    {
      id: 'log-1',
      name: 'crash-2026-11-12-21-14.log',
      size: 48_320,
      privacy: 'personal',
      retained: { kind: 'whole' },
    },
  ],
};

const listReply = {
  answers: [{
    sessionId: 'session-42',
    instanceId: 'grounded-1',
    verdict: { kind: 'crashed', how: 'pid' },
    endedAt: '2026-11-12T21:14:00Z',
    createdAt: '2026-11-12T21:15:00Z',
    status: 'analyzed',
    headline: answer.headline.en,
    level: 'very_likely',
    cause: 'mod',
  }],
  latestVerdict: { kind: 'crashed', how: 'pid' },
  retainedCrashes: 40,
};

const detailReply = {
  sessionId: 'session-42',
  instanceId: 'grounded-1',
  verdict: { kind: 'crashed', how: 'pid' },
  endedAt: '2026-11-12T21:14:00Z',
  createdAt: '2026-11-12T21:15:00Z',
  status: 'analyzed',
  answer,
  changes: [{
    kind: 'updated',
    key: 'acme.example',
    from_version: '2.0',
    to_version: '2.1',
  }],
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
  games_instance_list: { instances: [grounded] },
  games_instance_get: groundedDetail,
  games_instance_check: cleanCheck,
  list_crash_answers: listReply,
  get_crash_answer: detailReply,
};

async function boot(page: Page, mode: 'light' | 'dark') {
  await installGamesMock(page, {
    replies: { ...BOOT_REPLIES, games_list: { games: [skyrim] } },
    settings: { active_game: 'skyrim-se' },
  });
  await page.addInitScript((colorMode: string) => {
    // `colorMode` alone leaves the default custom dark surfaces in place, so a light shot needs
    // the theme surfaces too. This is the same set the Appearance page calls "Agora classic".
    localStorage.setItem('agora-ui-preferences', JSON.stringify({
      version: 1,
      colorMode,
      accentMode: 'agora',
      surfaceMode: 'theme',
      navMode: 'theme',
      backgroundMode: 'theme',
      textMode: 'theme',
      backgroundTextMode: 'theme',
      borderMode: 'theme',
    }));
  }, mode);
  await page.goto('/');
  await expect(page.getByTestId('game-home-page')).toBeVisible();
  await page.getByRole('navigation', { name: 'Main navigation' }).getByRole('button', { name: 'My Instances' }).click();
  await expect(page.getByTestId('game-instances-tab')).toBeVisible();
}

function cardFor(page: Page, name: string) {
  return page.getByTestId('instance-card').filter({ has: page.getByRole('heading', { name, exact: true }) });
}

for (const mode of ['light', 'dark'] as const) {
  test(`a crashed instance opens to the crash answer (${mode})`, async ({ page }) => {
    await boot(page, mode);
    await cardFor(page, 'Grounded').getByRole('button', { name: 'Edit Grounded' }).click();

    const card = page.getByTestId('crash-answer-card');
    await expect(card).toBeVisible();
    await expect(card).toContainText('Crashed last time');
    await expect(card).toContainText('Example Overhaul is very likely the cause');
    await card.screenshot({
      path: path.join(SCREENSHOT_DIR, `crash-card-${mode}.png`),
    });

    await page.getByRole('button', { name: 'See what happened' }).click();
    // The answer is taller than one viewport; a tall window lets the screenshot show all of it.
    await page.setViewportSize({ width: 1280, height: 2400 });
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByTestId('crash-headline')).toContainText('Example Overhaul is very likely the cause');
    await expect(dialog.getByTestId('crash-confidence')).toContainText('very likely');
    await expect(dialog.getByTestId('crash-confidence')).toContainText('Crashed');
    await expect(dialog.getByTestId('crash-save-warning')).toContainText(
      'Removing a mod partway through a playthrough can damage your save.',
    );
    await expect(dialog.getByTestId('crash-suspect')).toContainText('Example Overhaul');
    await expect(dialog.getByTestId('save-risk-chip')).toHaveText('Back up your save first');
    await expect(dialog.getByTestId('crash-cause-group')).toHaveCount(3);
    await expect(dialog.getByTestId('crash-changes')).toContainText('updated acme.example from 2.0 to 2.1');
    await expect(dialog.getByTestId('crash-files')).toContainText('crash-2026-11-12-21-14.log');
    await expect(dialog.getByTestId('crash-files')).toContainText("Agora keeps the last 40 crashes' files");

    const couldNotCheck = dialog.getByRole('button', { name: '2 checks could not run' });
    await expect(couldNotCheck).toHaveAttribute('aria-expanded', 'false');
    await couldNotCheck.click();
    await expect(dialog.getByText('the parser timed out')).toBeVisible();

    await dialog.screenshot({
      path: path.join(SCREENSHOT_DIR, `crash-answer-${mode}.png`),
    });

    await expectNoUnmockedCommands(page);
  });
}
