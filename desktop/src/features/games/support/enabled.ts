/**
 * The Game support list as the game pickers use it: a turned-off game is not offered, and a list
 * that failed to load hides nothing. These are convenience filters, not safety checks, so a
 * damaged or missing answer must never make a found game disappear.
 */

import type { GameListing } from '../shared/types';
import type { GameSupportRow } from './api';

/**
 * The ids the support list turns on, or null when the list is still loading or failed. Null means
 * "hide nothing", the same rule the game switcher follows.
 */
export function enabledGameIds(support: GameSupportRow[] | null | undefined): Set<string> | null {
  if (support === undefined || support === null) return null;
  return new Set(support.filter((row) => row.enabled).map((row) => row.gameId));
}

/** The found games that are on, in their original order. A failed list keeps every game. */
export function gamesToOffer(
  games: GameListing[],
  support: GameSupportRow[] | null | undefined,
): GameListing[] {
  const enabled = enabledGameIds(support);
  return enabled === null ? games : games.filter((game) => enabled.has(game.gameId));
}

/** Whether the support list positively says this game is off. A failed list never says off. */
export function isGameTurnedOff(
  gameId: string,
  support: GameSupportRow[] | null | undefined,
): boolean {
  const enabled = enabledGameIds(support);
  return enabled !== null && !enabled.has(gameId);
}
