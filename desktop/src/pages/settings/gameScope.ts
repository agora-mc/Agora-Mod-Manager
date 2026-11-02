import { MINECRAFT_GAME_ID } from '../../features/games/shared/types';

/**
 * Which games a Settings card is for.
 *
 * `all-games` cards stay on screen whichever game is selected. `minecraft` cards
 * describe Minecraft only (Java, the Mojang launcher, mod updates, skins), so
 * they are hidden while another game is selected. Every card carries one of
 * these ids through `SettingsSection`, and the table below is the single place
 * that decides its audience: a new card cannot compile without being classified,
 * and `gameScope.test.ts` fails if a card is added without one.
 */
export type SettingsAudience = 'all-games' | 'minecraft';

export const SETTINGS_CARD_AUDIENCE = {
  // General
  'advanced-mode': 'all-games',
  installation: 'all-games',
  walkthrough: 'all-games',
  'game-support': 'all-games',
  'template-settings': 'minecraft',
  'runtime-reclaim': 'all-games',
  'download-watch': 'all-games',
  'content-sharing': 'minecraft',
  updates: 'all-games',
  'instance-updates': 'minecraft',

  // Appearance
  'appearance-theme': 'all-games',
  'appearance-type': 'all-games',
  'appearance-motion': 'all-games',
  'appearance-living-background': 'all-games',

  // Accounts
  'microsoft-account': 'minecraft',
  'github-account': 'all-games',
  skin: 'minecraft',

  // Launching
  'launch-mode': 'minecraft',
  'when-game-starts': 'minecraft',
  'launcher-path': 'minecraft',
  disclaimer: 'minecraft',
  'java-runtime': 'minecraft',
  'jvm-defaults': 'minecraft',
  'game-window': 'minecraft',

  // Services and Privacy
  'content-sources': 'all-games',
  'ai-assistant': 'all-games',
  'ai-mcp': 'all-games',
  plugins: 'all-games',
  privacy: 'all-games',

  // The selected game's own section, shown above the tabs while that game is
  // selected. It is app-wide in the sense that it is not a Minecraft card.
  'selected-game': 'all-games',
} as const satisfies Record<string, SettingsAudience>;

export type SettingsCardId = keyof typeof SETTINGS_CARD_AUDIENCE;

/** Minecraft, or no choice loaded yet, is the page Settings has always shown. */
export function isMinecraftSelected(activeGame: string | null | undefined): boolean {
  return activeGame === null || activeGame === undefined || activeGame === MINECRAFT_GAME_ID;
}

/** A card is shown for Minecraft always, and for other games only when it is app-wide. */
export function isSettingsCardVisible(card: SettingsCardId, minecraftSelected: boolean): boolean {
  return minecraftSelected || SETTINGS_CARD_AUDIENCE[card] === 'all-games';
}

/**
 * A readable name for a game id when discovery did not name it. "skyrim-se"
 * becomes "Skyrim Se"; a known listing's name is always preferred.
 */
export function gameDisplayName(gameId: string, knownName?: string | null): string {
  if (knownName && knownName.trim()) return knownName;
  const words = gameId.replace(/[-_]+/g, ' ').trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return gameId;
  return words.map((word) => word.charAt(0).toUpperCase() + word.slice(1)).join(' ');
}
