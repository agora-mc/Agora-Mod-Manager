/**
 * The navigation a non-Minecraft game shows: the shared sidebar entries mapped onto the game's
 * own pages, and the pages of one instance.
 *
 * Every game uses one sidebar (Home, Browse, Library, My Instances, ..., Settings), and a game has
 * its own Home, Browse, Library and My Instances, each a variant of Minecraft's page of the same
 * name. A game has no entries of its own: its import is a dialog on My Instances, and its
 * installs, bases and recovery are pages of its Settings section, as Minecraft's are.
 */

import type { GameInstancePage, GameTabId, Tab } from '@/lib/useDestination';

/** Which shared sidebar entry each of a game's pages is highlighted under: the one of its name. */
const SHARED_ENTRY_FOR_GAME_TAB: Record<GameTabId, Tab> = {
  home: 'home',
  browse: 'browse',
  instances: 'instances',
  library: 'library',
};

/** Shared sidebar entries that open a game's page of the same name. */
export const GAME_PAGE_FOR_SHARED_ENTRY: Partial<Record<Tab, GameTabId>> = {
  home: 'home',
  browse: 'browse',
  library: 'library',
  instances: 'instances',
};

export interface GameInstancePageTab {
  id: GameInstancePage;
  label: string;
}

export const GAME_INSTANCE_PAGE_TABS: readonly GameInstancePageTab[] = [
  { id: 'overview', label: 'Overview' },
  { id: 'content', label: 'Content' },
  { id: 'plugins', label: 'Plugins' },
  { id: 'ini-saves', label: 'INI & saves' },
  { id: 'tools', label: 'Tools' },
];

/**
 * The game page a sidebar id opens while a non-Minecraft game is shown, or null for an entry that
 * is not a game page (Settings, Help & Guide and the other app-wide pages).
 */
export function gameTabForSidebarTab(tab: Tab): GameTabId | null {
  return GAME_PAGE_FOR_SHARED_ENTRY[tab] ?? null;
}

/** The sidebar id a game tab is highlighted under. */
export function sidebarTabForGameTab(tab: GameTabId): Tab {
  return SHARED_ENTRY_FOR_GAME_TAB[tab];
}
