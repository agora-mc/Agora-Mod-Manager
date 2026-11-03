/**
 * The navigation a non-Minecraft game shows: the shared sidebar entries mapped onto the game's
 * own pages, and the pages of one instance.
 *
 * Every game uses one sidebar (Home, Browse, Library, My Instances, ..., Settings), so a game's
 * pages sit under the entries Minecraft uses for the same idea. A shared entry the game has no
 * page for yet opens the closest page it does have, rather than being hidden or dead. A game has
 * no entries of its own: its import is a dialog on My Instances, and its installs, bases and
 * recovery are pages of its Settings section, as Minecraft's are.
 */

import type { GameInstancePage, GameTabId, Tab } from '@/lib/useDestination';

/**
 * Which shared sidebar entry each of a game's pages is highlighted under. Instances and Library
 * are the same idea for every game, so they use Minecraft's entries.
 */
const SHARED_ENTRY_FOR_GAME_TAB: Record<GameTabId, Tab> = {
  instances: 'instances',
  library: 'library',
};

/**
 * Shared sidebar entries that open a game's page. Home and Browse have no page of their own for a
 * game yet: Home opens My Instances (where a game's session starts and where you play from) and
 * Browse opens the Library (its "Curated items" list is the game's catalog).
 */
export const GAME_PAGE_FOR_SHARED_ENTRY: Partial<Record<Tab, GameTabId>> = {
  home: 'instances',
  browse: 'library',
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
