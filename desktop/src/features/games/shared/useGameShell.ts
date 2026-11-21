import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { getSetting, setSetting } from '@/lib/tauri';
import { listInstalledGames } from '../discovery/api';
import { GAME_SUPPORT_CHANGED_EVENT, listGameSupport, type GameSupportRow } from '../support/api';
import { MINECRAFT_GAME_ID, type GameListing } from './types';

/** The settings key the active game persists under (the app's own settings store). */
export const ACTIVE_GAME_SETTING = 'active_game';

export interface GameShell {
  /** The persisted choice. Minecraft until the stored value has loaded. */
  activeGame: string;
  /**
   * What the shell shows. A stored game that discovery no longer finds, or a discovery that
   * failed, falls back to Minecraft rather than showing a game the user cannot reach.
   */
  shownGame: string;
  /** Non-Minecraft games found on this machine. Empty until discovery has answered. */
  foundGames: GameListing[];
  /** The header switcher is shown only when there is another game to switch to. */
  switcherVisible: boolean;
  /**
   * The games the user turned off, with the support list's names. Empty while the list loads and
   * when it failed (a failed list hides nothing).
   */
  turnedOffGames: { gameId: string; name: string }[];
  /** True once the stored choice, discovery and the support list have all settled (answered or failed). */
  ready: boolean;
  /** Choose a game and remember the choice across restarts. */
  setActiveGame: (gameId: string) => void;
}

type Discovery =
  | { status: 'loading' }
  | { status: 'failed' }
  | { status: 'loaded'; games: GameListing[] };

/** Anything that is not a plausible game id is treated as "no choice made", not as a game. */
export function parseActiveGame(value: unknown): string {
  if (typeof value !== 'string') return MINECRAFT_GAME_ID;
  if (value.length === 0 || value.length > 64) return MINECRAFT_GAME_ID;
  return value;
}

/**
 * The games the switcher offers: the found games minus the ones the user has turned off. A support
 * list that did not load (null) hides nothing. This is a convenience filter, not a safety check,
 * so an unreadable list must not make a found game disappear.
 */
export function visibleGames(games: GameListing[], support: GameSupportRow[] | null): GameListing[] {
  if (support === null) return games;
  const off = new Set(support.filter((row) => !row.enabled).map((row) => row.gameId));
  return games.filter((game) => !off.has(game.gameId));
}

export function useGameShell(): GameShell {
  // null = the stored value has not come back yet.
  const [stored, setStored] = useState<string | null>(null);
  const [discovery, setDiscovery] = useState<Discovery>({ status: 'loading' });
  // undefined = the support list has not answered yet; null = it failed, so nothing is hidden.
  const [support, setSupport] = useState<GameSupportRow[] | null | undefined>(undefined);
  // Only the newest reply counts, so a slow earlier read cannot overwrite a later toggle.
  const supportRequest = useRef(0);

  const refreshSupport = useCallback(() => {
    const request = ++supportRequest.current;
    listGameSupport()
      .then((rows) => {
        if (request === supportRequest.current) setSupport(rows);
      })
      .catch(() => {
        // Not a list: hide nothing (see visibleGames).
        if (request === supportRequest.current) setSupport(null);
      });
  }, []);

  useEffect(() => {
    refreshSupport();
    // The Settings page announces a toggle, so the switcher follows it without a restart.
    window.addEventListener(GAME_SUPPORT_CHANGED_EVENT, refreshSupport);
    return () => window.removeEventListener(GAME_SUPPORT_CHANGED_EVENT, refreshSupport);
  }, [refreshSupport]);

  useEffect(() => {
    let cancelled = false;
    getSetting(ACTIVE_GAME_SETTING)
      .then((value) => {
        if (!cancelled) setStored(parseActiveGame(value));
      })
      .catch(() => {
        // An unreadable choice means Minecraft, the same as a fresh install.
        if (!cancelled) setStored(MINECRAFT_GAME_ID);
      });
    listInstalledGames()
      .then((list) => {
        if (!cancelled) {
          const games = list.games.filter((game) => game.gameId !== MINECRAFT_GAME_ID && game.installs.length > 0);
          setDiscovery({ status: 'loaded', games });
        }
      })
      .catch(() => {
        // Not a list (or no discovery at all): nothing is found, so nothing is offered.
        if (!cancelled) setDiscovery({ status: 'failed' });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const setActiveGame = useCallback((gameId: string) => {
    setStored(gameId);
    // A choice that fails to save costs the user one click on the next start; it is not
    // worth interrupting them for.
    setSetting(ACTIVE_GAME_SETTING, gameId).catch(() => undefined);
  }, []);

  const foundGames = useMemo(
    () => (discovery.status === 'loaded' ? visibleGames(discovery.games, support ?? null) : []),
    [discovery, support],
  );
  const turnedOffGames = useMemo(
    () => (support ?? [])
      .filter((row) => !row.enabled)
      .map((row) => ({ gameId: row.gameId, name: row.name })),
    [support],
  );
  const activeGame = stored ?? MINECRAFT_GAME_ID;
  const ready = stored !== null && discovery.status !== 'loading' && support !== undefined;
  const isFound = foundGames.some((game) => game.gameId === activeGame);
  const shownGame = !ready || activeGame === MINECRAFT_GAME_ID
    ? MINECRAFT_GAME_ID
    : isFound
      ? activeGame
      : MINECRAFT_GAME_ID;

  return {
    activeGame,
    shownGame,
    foundGames,
    switcherVisible: foundGames.length > 0,
    turnedOffGames,
    ready,
    setActiveGame,
  };
}
