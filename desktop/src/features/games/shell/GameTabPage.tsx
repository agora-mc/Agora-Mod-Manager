/**
 * The page for one of a non-Minecraft game's own pages (Home, Browse, Instances, Library), each a
 * variant of Minecraft's page of the same name. Each page's content comes from its owning slice's
 * folder. The game's import is a dialog on its Instances page, and its installs, bases and
 * recovery are in Settings.
 */

import type { GameTabId } from '@/lib/useDestination';
import { GameHomePage } from '../home';
import { GameInstancesPage } from '../instances';
import { GameBrowsePage } from '../catalog';
import { LibraryPage } from '@/features/library/LibraryPage';

interface GameTabPageProps {
  gameId: string;
  tab: GameTabId;
  /** Short game names by id ("Skyrim SE"), for pages that name the game. */
  gameNames?: Record<string, string>;
  /** Open an instance's page from the Instances tab. */
  onOpenInstance: (instanceId: string) => void;
  /** Open another of this game's pages (Home and Browse link to the others). */
  onOpenTab?: (tab: GameTabId) => void;
  /** Open Help & Guide (Home's guide card). */
  onOpenGuide?: () => void;
}

export function GameTabPage({ gameId, tab, gameNames = {}, onOpenInstance, onOpenTab, onOpenGuide }: GameTabPageProps) {
  const gameName = gameNames[gameId] ?? gameId;
  switch (tab) {
    case 'home':
      return (
        <GameHomePage
          key={gameId}
          gameId={gameId}
          gameName={gameName}
          onOpenInstance={onOpenInstance}
          onOpenInstances={() => onOpenTab?.('instances')}
          onOpenBrowse={() => onOpenTab?.('browse')}
          onOpenGuide={() => onOpenGuide?.()}
        />
      );
    case 'browse':
      return (
        <GameBrowsePage
          gameId={gameId}
          gameName={gameName}
          onOpenLibrary={onOpenTab ? () => onOpenTab('library') : undefined}
          onOpenInstances={onOpenTab ? () => onOpenTab('instances') : undefined}
        />
      );
    case 'instances':
      return <GameInstancesPage gameId={gameId} onOpenInstance={onOpenInstance} />;
    case 'library':
      // Minecraft's Library page, for this game. Its catalog is on Browse, as Minecraft's is.
      return <LibraryPage key={gameId} gameId={gameId} gameName={gameName} gameNames={gameNames} />;
  }
}
