import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { GameSupportSettings } from './GameSupportSettings';

const rows = [
  { gameId: 'minecraft', name: 'Minecraft: Java Edition', source: { kind: 'builtIn' }, enabled: true, canDisable: false },
  { gameId: 'skyrim-se', name: 'Skyrim Special Edition', source: { kind: 'builtIn' }, enabled: true, canDisable: true },
  { gameId: 'valheim', name: 'Valheim', source: { kind: 'plugin', pluginId: 'agora-tracer.valheim' }, enabled: false, canDisable: true },
];

/** Answers by command. The card and the list load together, so call order is not a contract. */
function answer(overrides: Record<string, unknown> = {}) {
  invokeMock.mockImplementation(async (command: string) => {
    if (Object.prototype.hasOwnProperty.call(overrides, command)) {
      const reply = overrides[command];
      if (typeof reply === 'function') return (reply as () => unknown)();
      if (reply instanceof Error) throw reply;
      return reply;
    }
    if (command === 'games_support_list') return rows;
    if (command === 'games_list') return { games: [] };
    if (command === 'list_official_plugins') return { plugins: [], unavailableReason: null };
    if (command === 'games_support_set_enabled') return null;
    if (command === 'get_setting') return null;
    return null;
  });
}

describe('GameSupportSettings', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    answer();
  });

  it('lists every game with its source, and locks Minecraft on with the reason', async () => {
    render(<GameSupportSettings />);

    const minecraft = await screen.findByRole('checkbox', { name: 'Offer Minecraft: Java Edition' });
    expect(minecraft).toBeChecked();
    expect(minecraft).toBeDisabled();
    expect(screen.getByText(/Always on: Agora falls back to Minecraft/)).toBeInTheDocument();

    const skyrim = screen.getByRole('checkbox', { name: 'Offer Skyrim Special Edition' });
    expect(skyrim).toBeChecked();
    expect(skyrim).toBeEnabled();
    // Minecraft and Skyrim are both built in; only Valheim comes from a plugin.
    expect(screen.getAllByText(/^Built in/)).toHaveLength(2);

    const valheim = screen.getByRole('checkbox', { name: 'Offer Valheim' });
    expect(valheim).not.toBeChecked();
    expect(screen.getByText('From plugin agora-tracer.valheim')).toBeInTheDocument();
  });

  it('shows an error, not an empty list, when the list fails to load', async () => {
    answer({ games_support_list: new Error('games_support_list did not return a list of games.') });
    render(<GameSupportSettings />);

    expect(await screen.findByRole('alert')).toHaveTextContent('games_support_list did not return a list of games.');
    expect(screen.queryByRole('checkbox')).toBeNull();
    expect(screen.getByRole('button', { name: 'Try again' })).toBeInTheDocument();
  });

  it('retries a failed load when asked', async () => {
    let attempts = 0;
    answer({
      games_support_list: () => {
        attempts += 1;
        if (attempts === 1) throw new Error('damaged');
        return rows;
      },
    });
    render(<GameSupportSettings />);
    fireEvent.click(await screen.findByRole('button', { name: 'Try again' }));
    expect(await screen.findByRole('checkbox', { name: 'Offer Valheim' })).toBeInTheDocument();
  });

  it('offers Add game folder for a game discovery did not find, and marks it found after the add', async () => {
    let hasInstall = false;
    answer({
      games_support_list: [
        rows[0],
        { gameId: 'skyrim-se', name: 'Skyrim Special Edition', source: { kind: 'plugin', pluginId: 'agora.skyrim' }, enabled: true, canDisable: true },
      ],
      games_list: () => ({
        games: hasInstall
          ? [{ gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [{}] }]
          : [],
      }),
      pick_directory: 'D:/Games/Skyrim',
      games_add_folder: () => {
        hasInstall = true;
        return { path: 'D:/Games/Skyrim', gameId: 'skyrim-se', gameName: 'Skyrim Special Edition' };
      },
    });
    render(<GameSupportSettings />);

    expect(await screen.findByText('Not found on this computer.')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Add game folder for Minecraft: Java Edition' })).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: 'Add game folder for Skyrim Special Edition' }));

    expect(await screen.findByText('Added D:/Games/Skyrim as Skyrim Special Edition.')).toBeInTheDocument();
    expect(await screen.findByText('Found on this computer.')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('pick_directory', {
      title: 'Choose the folder the game is installed in',
    });
    expect(invokeMock).toHaveBeenCalledWith('games_add_folder', { path: 'D:/Games/Skyrim' });
  });

  it('turns a game on and records it, sending the id and state', async () => {
    render(<GameSupportSettings />);

    const valheim = await screen.findByRole('checkbox', { name: 'Offer Valheim' });
    fireEvent.click(valheim);

    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Offer Valheim' })).toBeChecked());
    expect(invokeMock).toHaveBeenCalledWith('games_support_set_enabled', { gameId: 'valheim', enabled: true });
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('keeps the old state and says why when a change is refused', async () => {
    answer({
      games_support_set_enabled: () => {
        throw { code: 'core_failed', message: 'No game is known as \'x\'.' };
      },
    });
    render(<GameSupportSettings />);

    const skyrim = await screen.findByRole('checkbox', { name: 'Offer Skyrim Special Edition' });
    fireEvent.click(skyrim);

    expect(await screen.findByRole('alert')).toHaveTextContent('Skyrim Special Edition was not changed. No game is known as');
    expect(screen.getByRole('checkbox', { name: 'Offer Skyrim Special Edition' })).toBeChecked();
  });

  it('marks a game from a verified official plugin and explains an unverified one', async () => {
    answer({
      games_support_list: [
        rows[0],
        { gameId: 'skyrim-se', name: 'Skyrim Special Edition', source: { kind: 'plugin', pluginId: 'agora.skyrim' }, enabled: true, canDisable: true },
        { gameId: 'fallout-4', name: 'Fallout 4', source: { kind: 'plugin', pluginId: 'agora.fallout' }, enabled: true, canDisable: true },
      ],
      list_official_plugins: {
        plugins: [
          { id: 'agora.skyrim', name: 'Skyrim Special Edition', gameIds: ['skyrim-se'], state: 'upToDate', installedStatus: { state: 'official', version: '1.0.0' } },
          { id: 'agora.fallout', name: 'Fallout 4', gameIds: ['fallout-4'], state: 'notInstalled', installedStatus: { state: 'unverified', reason: 'files in the plugin folder changed after install' } },
        ],
        unavailableReason: null,
      },
    });
    render(<GameSupportSettings />);

    expect(await screen.findByText('Skyrim Special Edition')).toBeInTheDocument();
    await waitFor(() => expect(screen.getAllByTestId('official-badge')).toHaveLength(1));
    expect(screen.getByTestId('official-status-note')).toHaveTextContent(
      'files in the plugin folder changed after install',
    );
  });
});
