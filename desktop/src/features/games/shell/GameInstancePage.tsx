/**
 * An instance of a non-Minecraft game, with its five pages (Overview, Content, Plugins,
 * INI & saves, Tools). The shell draws what every page shares, from the instance kit like
 * Minecraft's instance page: Back, the header (the instance and Launch) and the tab strip; each
 * page is a slot.
 */

import type { GameInstancePage as GameInstancePageId } from '@/lib/useDestination';
import { BackButton } from '@/components/ui/back-button';
import { InstanceTabBar } from '@/components/instance-kit';
import { InstanceHero, InstanceOverviewTab, InstanceSessionProvider } from '../instances';
import { InstanceContentTab } from '../content';
import { InstancePluginsTab } from '../plugins';
import { useLoadOrderTab } from '../plugins/loadOrderTab';
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
  const loadOrderTab = useLoadOrderTab(instanceId);
  const tabs = GAME_INSTANCE_PAGE_TABS
    .filter((tab) => tab.id !== 'plugins' || loadOrderTab.show)
    .map((tab) => (tab.id === 'plugins' ? { ...tab, label: loadOrderTab.label } : tab));
  return (
    <InstanceSessionProvider key={instanceId} instanceId={instanceId}>
      <div className="space-y-4" data-testid="game-instance-page">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <BackButton onBack={onBack} label="Instances" />
        </div>
        <InstanceHero onShowChecks={() => onNavigatePage('overview')} />
        <InstanceTabBar tabs={tabs} active={page} onSelect={onNavigatePage} />
        {page === 'overview' && (
          <InstanceOverviewTab
            gameId={gameId}
            instanceId={instanceId}
            onNavigatePage={onNavigatePage}
          />
        )}
        {page === 'content' && <InstanceContentTab gameId={gameId} instanceId={instanceId} />}
        {page === 'plugins' && <InstancePluginsTab gameId={gameId} instanceId={instanceId} />}
        {page === 'ini-saves' && <InstanceIniSavesTab gameId={gameId} instanceId={instanceId} />}
        {page === 'tools' && <InstanceToolsTab gameId={gameId} instanceId={instanceId} />}
      </div>
    </InstanceSessionProvider>
  );
}
