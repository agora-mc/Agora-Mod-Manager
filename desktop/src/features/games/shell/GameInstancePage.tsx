/**
 * An instance of a non-Minecraft game, with its five pages (Overview, Content, Plugins,
 * INI & saves, Tools). The shell draws the header every page shares (the instance and Launch) and
 * the page switch; each page is a slot.
 */

import type { GameInstancePage as GameInstancePageId } from '@/lib/useDestination';
import { InstanceHero, InstanceOverviewTab, InstanceSessionProvider } from '../instances';
import { InstanceContentTab } from '../content';
import { InstancePluginsTab } from '../plugins';
import { InstanceIniSavesTab } from '../iniSaves';
import { InstanceToolsTab } from '../tools';
import { GAME_INSTANCE_PAGE_TABS } from './gameNav';

interface GameInstancePageProps {
  gameId: string;
  instanceId: string;
  page: GameInstancePageId;
  onNavigatePage: (page: GameInstancePageId) => void;
  onBack: () => void;
}

export function GameInstancePage({ gameId, instanceId, page, onNavigatePage, onBack }: GameInstancePageProps) {
  return (
    <InstanceSessionProvider key={instanceId} instanceId={instanceId}>
      <div className="space-y-4" data-testid="game-instance-page">
        <button
          type="button"
          onClick={onBack}
          className="text-sm font-medium text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
        >
          ← Instances
        </button>
        <InstanceHero onShowChecks={() => onNavigatePage('overview')} />
        <nav aria-label="Instance pages" className="agora-tabbar">
          {GAME_INSTANCE_PAGE_TABS.map((entry) => {
            const isActive = entry.id === page;
            return (
              <button
                key={entry.id}
                type="button"
                aria-current={isActive ? 'page' : undefined}
                data-active={isActive}
                onClick={() => onNavigatePage(entry.id)}
                className="agora-tab text-sm"
              >
                {entry.label}
              </button>
            );
          })}
        </nav>
        {page === 'overview' && <InstanceOverviewTab gameId={gameId} instanceId={instanceId} />}
        {page === 'content' && <InstanceContentTab gameId={gameId} instanceId={instanceId} />}
        {page === 'plugins' && <InstancePluginsTab gameId={gameId} instanceId={instanceId} />}
        {page === 'ini-saves' && <InstanceIniSavesTab gameId={gameId} instanceId={instanceId} />}
        {page === 'tools' && <InstanceToolsTab gameId={gameId} instanceId={instanceId} />}
      </div>
    </InstanceSessionProvider>
  );
}
