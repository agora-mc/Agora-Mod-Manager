import { render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));

import { BuildBaseDialog } from './BuildBaseDialog';

const install = {
  installId: 'steam:489830',
  store: 'Steam',
  name: 'Skyrim Special Edition',
  location: 'D:/Steam/steamapps/common/Skyrim Special Edition',
  storeVersion: '1.6.1170',
  storeBuild: null,
  addOnCount: 0,
  runtime: { status: 'identified', runtime: { game: 'skyrim-se', store: 'steam', version: '1.6.1170', build: null }, source: 'executable' },
  installBytes: 1024,
};
const skyrim = { gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [install] };
const enabledSkyrim = {
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  source: { kind: 'builtIn' },
  enabled: true,
  canDisable: true,
};

function answer(settings: Record<string, unknown> = {}, support: unknown = [enabledSkyrim]) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === 'get_setting') {
      const key = String((args as { key?: unknown } | undefined)?.key ?? '');
      return Object.prototype.hasOwnProperty.call(settings, key) ? settings[key] : null;
    }
    if (command === 'games_list') return { games: [skyrim] };
    if (command === 'games_support_list') return support;
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe('BuildBaseDialog per-game defaults', () => {
  it('preselects the base type saved for this game', async () => {
    answer({ 'game.skyrim-se.baseType': 'copied' });
    render(<BuildBaseDialog open onOpenChange={() => undefined} gameId="skyrim-se" onBuilt={() => undefined} />);

    const copied = await screen.findByRole('button', { name: /^Copied/ });
    expect(copied).toHaveAttribute('aria-pressed', 'true');
  });

  it('hides the build choices and says why when the game is turned off', async () => {
    answer({}, [{ ...enabledSkyrim, enabled: false }]);
    render(<BuildBaseDialog open onOpenChange={() => undefined} gameId="skyrim-se" onBuilt={() => undefined} />);

    expect(await screen.findByText(/This game is turned off/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /^Build a/ })).toBeNull();
  });
});
