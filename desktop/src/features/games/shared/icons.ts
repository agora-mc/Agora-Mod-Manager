/**
 * The game's own executable icon, as a data URL (MASTER_SPEC §26).
 *
 * The command answers `null` for every expected miss (no install, no exe, no
 * icon, another platform) and never asks the UI to handle an error. A rejected
 * IPC call is still possible, so callers fall back to the letter or the
 * gamepad the same way they do for `null`.
 */

import { invoke } from '@tauri-apps/api/core';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** `null` when the game has no icon; a `data:image/png;base64,...` string otherwise. */
export async function loadGameIcon(gameId: string): Promise<string | null> {
  const reply: unknown = await invoke<unknown>('games_icon', { gameId });
  if (reply === null) return null;
  if (!isRecord(reply) || typeof reply.dataUrl !== 'string') {
    throw new Error('games_icon did not return an icon.');
  }
  return reply.dataUrl;
}
