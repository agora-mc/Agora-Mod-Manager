/**
 * The page for one of a non-Minecraft game's four tabs. The shell decides which slots a tab
 * has; each slot's content comes from its owning slice's folder.
 */

import type { GameTabId } from '@/lib/useDestination';
import { GameInstancesPage } from '../instances';
import { LibraryCatalogSection } from '../catalog';
import { LibraryPage } from '@/features/library/LibraryPage';
import { GameImportPage } from '../import';
import { SetupInstallsSection } from '../discovery';
import { SetupBasesSection } from '../bases';
import { SetupRecoverySection } from '../userFiles';

interface GameTabPageProps {
  gameId: string;
  tab: GameTabId;
  /** Short game names by id ("Skyrim SE"), for pages that name the game. */
  gameNames?: Record<string, string>;
  /** Open an instance's page from the Instances tab. */
  onOpenInstance: (instanceId: string) => void;
  /** Open the Import tab (the Instances page offers it next to New instance). */
  onOpenImport?: () => void;
}

export function GameTabPage({ gameId, tab, gameNames = {}, onOpenInstance, onOpenImport }: GameTabPageProps) {
  switch (tab) {
    case 'instances':
      return <GameInstancesPage gameId={gameId} onOpenInstance={onOpenInstance} onOpenImport={onOpenImport} />;
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
    case 'import':
      return <GameImportPage gameId={gameId} />;
    case 'setup':
      return (
        <div className="space-y-6">
          <SetupInstallsSection gameId={gameId} />
          <SetupBasesSection gameId={gameId} />
          <SetupRecoverySection gameId={gameId} />
        </div>
      );
  }
}
