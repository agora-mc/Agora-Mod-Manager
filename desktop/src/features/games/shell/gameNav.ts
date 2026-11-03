/**
 * The navigation a non-Minecraft game shows: the shared sidebar entries mapped onto the game's
 * own pages, the entries only a game has, and the pages of one instance.
 *
 * Every game uses one sidebar (Home, Browse, Library, My Instances, ..., Settings), so a game's
 * pages sit under the entries Minecraft uses for the same idea. A shared entry the game has no
 * page for yet opens the closest page it does have, rather than being hidden or dead.
 */

import { Download, Wrench, type LucideIcon } from 'lucide-react';
import type { GameInstancePage, GameTabId, Tab } from '@/lib/useDestination';

export interface GameShellTab {
  /** The sidebar id. Game-only ids are `game-*` so they never collide with a shared entry. */
  id: Tab;
  /** The destination tab this sidebar entry opens. */
  tab: GameTabId;
  label: string;
  icon: LucideIcon;
  /** The emoji the command palette shows for this entry, matching the rest of the palette. */
  emoji: string;
}

/**
 * Which shared sidebar entry each of a game's pages is highlighted under. Instances and Library
 * are the same idea for every game, so they use Minecraft's entries.
 */
const SHARED_ENTRY_FOR_GAME_TAB: Partial<Record<GameTabId, Tab>> = {
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

/** The entries only a game has, shown right after My Instances. */
export const GAME_ONLY_TABS: readonly GameShellTab[] = [
  { id: 'game-import', tab: 'import', label: 'Import', icon: Download, emoji: '⬇️' },
  { id: 'game-setup', tab: 'setup', label: 'Setup', icon: Wrench, emoji: '🔧' },
];

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
  return GAME_PAGE_FOR_SHARED_ENTRY[tab]
    ?? GAME_ONLY_TABS.find((entry) => entry.id === tab)?.tab
    ?? null;
}

/** The sidebar id a game tab is highlighted under. */
export function sidebarTabForGameTab(tab: GameTabId): Tab {
  const shared = SHARED_ENTRY_FOR_GAME_TAB[tab];
  if (shared) return shared;
  const entry = GAME_ONLY_TABS.find((candidate) => candidate.tab === tab);
  if (!entry) throw new Error(`No sidebar entry for game tab ${tab}`);
  return entry.id;
}

/**
 * Inserts a game's own entries after My Instances, the one place they always go. Works on any
 * entry shape, so the sidebar and the command palette share the rule.
 */
export function withGameOnlyEntries<T extends { tab?: Tab; id?: Tab }>(entries: readonly T[], gameOnly: readonly T[]): T[] {
  const at = entries.findIndex((entry) => (entry.id ?? entry.tab) === 'instances');
  if (at < 0) return [...entries, ...gameOnly];
  return [...entries.slice(0, at + 1), ...gameOnly, ...entries.slice(at + 1)];
}
