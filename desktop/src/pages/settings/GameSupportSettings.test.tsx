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

describe('GameSupportSettings', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('lists every game with its source, and locks Minecraft on with the reason', async () => {
    invokeMock.mockResolvedValueOnce(rows);
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
    invokeMock.mockRejectedValueOnce(new Error('games_support_list did not return a list of games.'));
    render(<GameSupportSettings />);

    expect(await screen.findByRole('alert')).toHaveTextContent('games_support_list did not return a list of games.');
    expect(screen.queryByRole('checkbox')).toBeNull();
    expect(screen.getByRole('button', { name: 'Try again' })).toBeInTheDocument();
  });

  it('retries a failed load when asked', async () => {
    invokeMock.mockRejectedValueOnce(new Error('damaged'));
    invokeMock.mockResolvedValueOnce(rows);
    render(<GameSupportSettings />);
    fireEvent.click(await screen.findByRole('button', { name: 'Try again' }));
    expect(await screen.findByRole('checkbox', { name: 'Offer Valheim' })).toBeInTheDocument();
  });

  it('turns a game on and records it, sending the id and state', async () => {
    invokeMock.mockResolvedValueOnce(rows);
    invokeMock.mockResolvedValueOnce(undefined);
    render(<GameSupportSettings />);

    const valheim = await screen.findByRole('checkbox', { name: 'Offer Valheim' });
    fireEvent.click(valheim);

    await waitFor(() => expect(screen.getByRole('checkbox', { name: 'Offer Valheim' })).toBeChecked());
    expect(invokeMock).toHaveBeenCalledWith('games_support_set_enabled', { gameId: 'valheim', enabled: true });
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('keeps the old state and says why when a change is refused', async () => {
    invokeMock.mockResolvedValueOnce(rows);
    invokeMock.mockRejectedValueOnce({ code: 'core_failed', message: 'No game is known as \'x\'.' });
    render(<GameSupportSettings />);

    const skyrim = await screen.findByRole('checkbox', { name: 'Offer Skyrim Special Edition' });
    fireEvent.click(skyrim);

    expect(await screen.findByRole('alert')).toHaveTextContent('Skyrim Special Edition was not changed. No game is known as');
    expect(screen.getByRole('checkbox', { name: 'Offer Skyrim Special Edition' })).toBeChecked();
  });
});
