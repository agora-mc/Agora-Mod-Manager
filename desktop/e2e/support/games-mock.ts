import type { Page } from '@playwright/test';
import { expect } from '@playwright/test';

/**
 * A mock of Tauri's `__TAURI_INTERNALS__` for specs that exercise the multi-game shell.
 *
 * Unlike the older specs, which answer any unknown command with `null`, this one answers from a
 * per-test table and treats every other command as a bug in the test: it records the name and
 * rejects the call, and `expectNoUnmockedCommands` fails the test when anything was recorded.
 * A `null` reply therefore means "the command answered with nothing", never "I forgot it".
 *
 * Settings are the one stateful part. `get_setting` and `set_setting` read and write
 * `localStorage`, so a value written before a reload is still there after it, the way the real
 * settings store behaves across restarts.
 *
 * Tauri's own `plugin:*` protocol calls (event listeners and the like) are not application
 * commands and are answered without being recorded.
 */

/** Replies by command name. Values are plain data: they cross into the page as JSON. */
export interface GamesMockTable {
  replies: Record<string, unknown>;
  /**
   * Replies handed out in order for one command, then `replies` takes over.
   * Used where the real command answers differently per call, such as an
   * install that first asks for consent and then installs.
   */
  sequences?: Record<string, unknown[]>;
  /** Settings the app starts with. Values written later by `set_setting` take precedence. */
  settings?: Record<string, unknown>;
}

const SETTINGS_STORE_KEY = '__agora_games_mock_settings';
const UNMOCKED_LOG_KEY = '__agora_games_mock_unmocked';

/** Installs the mock before any page script runs. Call it before `page.goto`. */
export async function installGamesMock(page: Page, table: GamesMockTable): Promise<void> {
  await page.addInitScript(
    ({ table, settingsKey, unmockedKey }) => {
      // The shell asks which games are turned off on every start. A null answer is a failed list,
      // which hides nothing (specs that need rows pass their own). The game switcher asks for each
      // game's exe icon; null means "this game has none".
      const replies: Record<string, unknown> = {
        games_support_list: null,
        games_icon: null,
        // A verified registry that lists no official plugins. Specs that need a
        // card pass their own entry; an unanswered command would fail the test.
        list_official_plugins: { plugins: [], unavailableReason: null },
        ...table.replies,
      };
      const sequences: Record<string, unknown[]> = {};
      for (const [command, queue] of Object.entries(table.sequences ?? {})) {
        sequences[command] = [...queue];
      }
      const seeded: Record<string, unknown> = { onboarding_complete: true, ...table.settings };
      const stored = JSON.parse(localStorage.getItem(settingsKey) ?? '{}') as Record<string, unknown>;
      const settings: Record<string, unknown> = { ...seeded, ...stored };
      const persistSettings = () => localStorage.setItem(settingsKey, JSON.stringify(settings));
      persistSettings();

      const recordUnmocked = (command: string) => {
        const list = JSON.parse(sessionStorage.getItem(unmockedKey) ?? '[]') as string[];
        list.push(command);
        sessionStorage.setItem(unmockedKey, JSON.stringify(list));
      };

      Object.assign(window as unknown as Record<string, unknown>, {
        __TAURI_INTERNALS__: {
          transformCallback() { return 1; },
          unregisterCallback() {},
          invoke(command: string, args: Record<string, unknown> = {}) {
            if (command.startsWith('plugin:')) return Promise.resolve(1);
            if (command === 'get_setting') {
              const key = String(args.key);
              return Promise.resolve(Object.prototype.hasOwnProperty.call(settings, key) ? settings[key] : null);
            }
            if (command === 'set_setting') {
              settings[String(args.key)] = args.value;
              persistSettings();
              return Promise.resolve(null);
            }
            if (command === 'games_support_set_enabled') {
              // The list a Settings toggle changes is the same one later reads get.
              const rows = replies.games_support_list;
              if (Array.isArray(rows)) {
                for (const row of rows) {
                  if (row && typeof row === 'object' && (row as { gameId?: unknown }).gameId === args.gameId) {
                    (row as { enabled?: unknown }).enabled = args.enabled;
                  }
                }
              }
              return Promise.resolve(null);
            }
            const queued = sequences[command];
            if (Array.isArray(queued) && queued.length > 0) {
              const next = queued.shift();
              return Promise.resolve(Array.isArray(next) ? structuredClone(next) : next);
            }
            if (Object.prototype.hasOwnProperty.call(replies, command)) {
              const reply = replies[command];
              // A list is copied on the way out: a page that asks again after a toggle must see a
              // new value, not the same array object it already holds.
              return Promise.resolve(Array.isArray(reply) ? structuredClone(reply) : reply);
            }
            recordUnmocked(command);
            return Promise.reject(new Error(`Unmocked Tauri command in a games test: ${command}`));
          },
        },
        __TAURI_EVENT_PLUGIN_INTERNALS__: { unregisterListener() {} },
      });
    },
    { table, settingsKey: SETTINGS_STORE_KEY, unmockedKey: UNMOCKED_LOG_KEY },
  );
}

/** Commands that were called without a table entry. Survives reloads within the test. */
export async function unmockedCommands(page: Page): Promise<string[]> {
  return page.evaluate((key) => JSON.parse(sessionStorage.getItem(key) ?? '[]') as string[], UNMOCKED_LOG_KEY);
}

/** The stored value of a setting, as the app last wrote it (or the table's seed if it never did). */
export async function storedSetting(page: Page, key: string): Promise<unknown> {
  return page.evaluate(
    ({ storeKey, key }) => (JSON.parse(localStorage.getItem(storeKey) ?? '{}') as Record<string, unknown>)[key] ?? null,
    { storeKey: SETTINGS_STORE_KEY, key },
  );
}

/** Fails the test if the app called a command the table does not answer. */
export async function expectNoUnmockedCommands(page: Page): Promise<void> {
  expect(await unmockedCommands(page), 'commands the games table does not answer').toEqual([]);
}

/**
 * Replies for the sections other slices mount on the same game page, empty. The Setup tab shows
 * installs, bases, game folders and recovery together, the Library shows the store and the
 * catalog, and the Instances tab lists instances: a spec about one of them still renders the
 * others, and an unanswered command fails the test. Put this first in a spec's boot replies so
 * the spec's own entries override it.
 */
/**
 * What the header of a game instance page reads on every one of its pages: the instance and its
 * launch checks. Specs for the other pages spread this in so the header has an answer.
 */
export function instanceHeaderReplies(instanceId: string): Record<string, unknown> {
  return {
    games_instance_get: {
      instanceId,
      gameId: 'skyrim-se',
      gameName: 'Skyrim Special Edition',
      name: 'Test instance',
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
      lastLaunchedAt: null,
      running: false,
    },
    games_instance_check: { instanceId, checked: true, status: 'ok', findings: [], passed: [] },
  };
}

export const OTHER_SLICES_REPLIES: Record<string, unknown> = {
  games_instance_list: { instances: [] },
  games_discover: { folders: [], warnings: [] },
  games_user_files_status: { sessions: [] },
  games_base_list: { bases: [], damaged: [] },
  games_content_list: { items: [], damaged: [] },
  games_catalog_list: { entries: [], instances: [] },
  games_library_list: { items: [], damaged: [], warnings: [] },
  games_library_instances: { instances: [], warnings: [] },
  games_library_icon: null,
  games_library_cache_icon: { saved: false, matched: 0, iconDataUrl: null },
};
