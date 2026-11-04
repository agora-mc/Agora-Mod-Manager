import { useCallback } from 'react';
import { MINECRAFT_GAME_ID } from './types';
import type { GameTabId } from '@/lib/useDestination';

/**
 * Switching games normally takes the user to that game's Home, as it does for Minecraft. Settings is shared by every
 * game, so its "Switch to Minecraft" button must be able to change the game without leaving
 * Settings: that is what `navigate: false` is for.
 */
export function useGameSwitch({
  setActiveGame,
  navigateToTab,
  navigateToGameTab,
}: {
  setActiveGame: (gameId: string) => void;
  navigateToTab: (tab: 'home') => void;
  navigateToGameTab: (gameId: string, tab: GameTabId) => void;
}) {
  return useCallback((gameId: string, options?: { navigate?: boolean }) => {
    setActiveGame(gameId);
    if (options?.navigate === false) return;
    if (gameId === MINECRAFT_GAME_ID) {
      navigateToTab('home');
    } else {
      navigateToGameTab(gameId, 'home' satisfies GameTabId);
    }
  }, [setActiveGame, navigateToTab, navigateToGameTab]);
}
