import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { SetupInstallsSection } from './SetupInstallsSection';

const install = (gameId: string) => ({
  installId: `${gameId}:1`,
  store: 'Steam',
  name: gameId === 'skyrim-se' ? 'Skyrim Special Edition' : 'Valheim',
  location: `D:/Games/${gameId}`,
  storeVersion: '1.0',
  storeBuild: null,
  addOnCount: 0,
  runtime: { status: 'identified', runtime: { game: gameId, store: 'steam', version: '1.0', build: null }, source: 'executable' },
  installBytes: null,
});
const skyrim = { gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [install('skyrim-se')] };
const valheim = { gameId: 'valheim', name: 'Valheim', installs: [install('valheim')] };
const row = (gameId: string, name: string, enabled: boolean) => ({
  gameId,
  name,
  source: { kind: 'builtIn' },
  enabled,
  canDisable: true,
});

function answer(support: unknown) {
  invokeMock.mockImplementation(async (command: string) => {
    if (command === 'games_list') return { games: [skyrim, valheim] };
    if (command === 'games_discover') return { folders: [], warnings: [] };
    if (command === 'games_support_list') {
      if (support instanceof Error) throw support;
      return support;
    }
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe('Setup › Installs and turned-off games', () => {
  it('lists only the games that are on', async () => {
    answer([row('skyrim-se', 'Skyrim Special Edition', false), row('valheim', 'Valheim', true)]);
    render(<SetupInstallsSection gameId="skyrim-se" />);

    expect(await screen.findByRole('heading', { name: /Valheim/ })).toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: /Skyrim Special Edition/ })).toBeNull();
  });

  it('lists every found game when the support list fails, because a failed list hides nothing', async () => {
    answer(new Error('games_support_list did not return a list of games.'));
    render(<SetupInstallsSection gameId="skyrim-se" />);

    expect(await screen.findByRole('heading', { name: /Skyrim Special Edition/ })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: /Valheim/ })).toBeInTheDocument();
  });
});
