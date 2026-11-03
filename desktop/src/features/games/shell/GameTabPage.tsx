/**
 * The page for one of a non-Minecraft game's own pages (Instances, Library). The shell decides
 * which slots a page has; each slot's content comes from its owning slice's folder. The game's
 * import is a dialog on its Instances page, and its installs, bases and recovery are in Settings.
 */

import type { GameTabId } from '@/lib/useDestination';
import { GameInstancesPage } from '../instances';
import { LibraryStoreSection } from '../content';
import { LibraryCatalogSection } from '../catalog';

interface GameTabPageProps {
  gameId: string;
  tab: GameTabId;
  /** Open an instance's page from the Instances tab. */
  onOpenInstance: (instanceId: string) => void;
}

export function GameTabPage({ gameId, tab, onOpenInstance }: GameTabPageProps) {
  switch (tab) {
    case 'instances':
      return <GameInstancesPage gameId={gameId} onOpenInstance={onOpenInstance} />;
    case 'library':
      return (
        <div className="space-y-6">
          <LibraryStoreSection gameId={gameId} />
          <LibraryCatalogSection gameId={gameId} />
        </div>
      );
  }
}
