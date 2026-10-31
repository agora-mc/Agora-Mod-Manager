/**
 * IPC wrappers for the Game support list: which games Agora offers, and whether each is on
 * (MASTER_SPEC §26). Turning a game off hides it from the game switcher; it never touches the
 * game's instances, bases or files.
 *
 * Every call rejects on anything that is not the documented shape, so a caller never mistakes a
 * damaged reply for "every game is on".
 */

import { invoke } from '@tauri-apps/api/core';

/** Where a game comes from: built into Agora, or added by a plugin. */
export type GameSource = { kind: 'builtIn' } | { kind: 'plugin'; pluginId: string };

/** One game on the Game support list. */
export interface GameSupportRow {
  gameId: string;
  name: string;
  source: GameSource;
  enabled: boolean;
  /** False for a game that cannot be turned off yet (Minecraft). Such a game is always on. */
  canDisable: boolean;
}

/**
 * Fired on the window after a game is switched on or off, so the game switcher rereads the list
 * without an app restart.
 */
export const GAME_SUPPORT_CHANGED_EVENT = 'agora-game-support-changed';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isSource(value: unknown): value is GameSource {
  if (!isRecord(value)) return false;
  if (value.kind === 'builtIn') return true;
  return value.kind === 'plugin' && typeof value.pluginId === 'string' && value.pluginId.length > 0;
}

function isGameSupportRow(value: unknown): value is GameSupportRow {
  if (!isRecord(value)) return false;
  if (typeof value.gameId !== 'string' || value.gameId.length === 0) return false;
  if (typeof value.name !== 'string' || !isSource(value.source)) return false;
  if (typeof value.enabled !== 'boolean' || typeof value.canDisable !== 'boolean') return false;
  // A game that cannot be turned off is always on; a row that says otherwise is damaged.
  return value.canDisable || value.enabled;
}

/** Every game Agora knows, with whether it is on. Rejects if the reply is not a clean list. */
export async function listGameSupport(): Promise<GameSupportRow[]> {
  const reply: unknown = await invoke<unknown>('games_support_list');
  if (!Array.isArray(reply) || !reply.every(isGameSupportRow)) {
    throw new Error('games_support_list did not return a list of games.');
  }
  const ids = new Set(reply.map((row) => row.gameId));
  if (ids.size !== reply.length) {
    throw new Error('games_support_list returned the same game twice.');
  }
  return reply;
}

/**
 * Turn a game on or off. Rejects with the core's reason when the game is refused (an unknown id,
 * or Minecraft turned off); nothing changes in that case.
 */
export async function setGameEnabled(gameId: string, enabled: boolean): Promise<void> {
  await invoke<void>('games_support_set_enabled', { gameId, enabled });
  window.dispatchEvent(new Event(GAME_SUPPORT_CHANGED_EVENT));
}
