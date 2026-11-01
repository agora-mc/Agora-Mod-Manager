import { act, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { ConfirmProvider } from '@/components/ui/confirm';
import { GameInstancesPage } from './InstancesTab';

const card = (gameId: string) => ({
  instanceId: `${gameId}-inst`,
  gameId,
  name: `${gameId} instance`,
  store: null,
  version: null,
  pinned: true,
  deployment: null,
  lastLaunchedAt: null,
  running: false,
});

describe('GameInstancesPage: out-of-order replies', () => {
  beforeEach(() => invokeMock.mockReset());

  // Review finding (final-ui). `load` has no "newest request wins" guard (BasesSection has one).
  // Switch game A -> B while A's list is still in flight: B's list arrives first, then A's late
  // reply overwrites it, so game B's Instances tab lists game A's instances (with Launch/Delete).
  // Run: npx vitest run src/features/games/instances/InstancesTab.race.test.tsx (un-skip first)
  // proves bug: GameInstancesPage lets a slow reply for the previous game overwrite the current game's list
  it.skip('shows only the current game\'s instances when replies arrive out of order', async () => {
    const pending: Record<string, (value: unknown) => void> = {};
    invokeMock.mockImplementation((command: string, args: { gameId: string }) => {
      if (command !== 'games_instance_list') return Promise.resolve(null);
      return new Promise((resolve) => {
        pending[args.gameId] = resolve;
      });
    });
    const ui = (gameId: string) => (
      <ConfirmProvider>
        <GameInstancesPage gameId={gameId} onOpenInstance={() => undefined} />
      </ConfirmProvider>
    );
    const { rerender } = render(ui('game-a'));
    rerender(ui('game-b'));
    await act(async () => {
      pending['game-b']({ instances: [card('game-b')] });
    });
    expect(await screen.findByText('game-b instance')).toBeInTheDocument();
    await act(async () => {
      pending['game-a']({ instances: [card('game-a')] });
    });
    expect(screen.queryByText('game-a instance')).not.toBeInTheDocument();
  });
});
