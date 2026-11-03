/**
 * The page for one of a non-Minecraft game's own pages (Instances, Library). The shell decides
 * which slots a page has; each slot's content comes from its owning slice's folder. The game's
 * import is a dialog on its Instances page, and its installs, bases and recovery are in Settings.
 */

import type { GameTabId } from '@/lib/useDestination';
import { GameInstancesPage } from '../instances';
import { LibraryCatalogSection } from '../catalog';
import { LibraryPage } from '@/features/library/LibraryPage';

interface GameTabPageProps {
  gameId: string;
  tab: GameTabId;
  /** Short game names by id ("Skyrim SE"), for pages that name the game. */
  gameNames?: Record<string, string>;
  /** Open an instance's page from the Instances tab. */
  onOpenInstance: (instanceId: string) => void;
}

export function GameTabPage({ gameId, tab, gameNames = {}, onOpenInstance }: GameTabPageProps) {
  switch (tab) {
    case 'instances':
      return <GameInstancesPage gameId={gameId} onOpenInstance={onOpenInstance} />;
    case 'library':
      // Minecraft's Library page, for this game: its items, plus the game's curated catalog.
      return (
        <LibraryPage
          key={gameId}
          gameId={gameId}
          gameName={gameNames[gameId] ?? gameId}
          gameNames={gameNames}
          gameSection={<LibraryCatalogSection gameId={gameId} />}
        />
      );
  }
}
