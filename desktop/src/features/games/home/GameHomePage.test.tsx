/**
 * A game's Home: the instance to continue (the running one, else the one played last), the
 * others one click away, a welcome with Create and Import when there are none, and the catalog
 * strip that sends the player to Browse.
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));
const requestGameImport = vi.hoisted(() => vi.fn());
vi.mock('@/lib/gameImportRequest', () => ({ requestGameImport }));

import { GameHomePage } from './index';
import { clearGameLaunches } from '../instances/launchController';
import type { InstanceCard } from '../instances/api';

const capabilities = { launch: true, stop: true, rename: true, clone: true, trash: true, openFolder: true, playTime: true };
const card = (instanceId: string, name: string, lastPlayedAt: string | null, running = false): InstanceCard => ({
  instanceId,
  gameId: 'skyrim-se',
  name,
  store: 'steam',
  version: '1.6.1170',
  pinned: true,
  deployment: null,
  lastLaunchedAt: lastPlayedAt,
  lastPlayedAt,
  playTimeMs: 0,
  running,
  capabilities,
});

function replies(instances: InstanceCard[], entries: unknown[] = []) {
  invokeMock.mockImplementation(async (command: string) => {
    if (command === 'games_instance_list') return { instances };
    if (command === 'games_catalog_list') return { entries, instances: [] };
    if (command === 'games_instance_icon') return null;
    if (command === 'games_instance_launch') {
      return { status: 'launched', pid: 1, deployment: null, notice: null, findings: [], reason: null, next: null };
    }
    return null;
  });
}

function renderHome(overrides: Partial<Parameters<typeof GameHomePage>[0]> = {}) {
  const props = {
    gameId: 'skyrim-se',
    gameName: 'Skyrim SE',
    onOpenInstance: vi.fn(),
    onOpenInstances: vi.fn(),
    onOpenBrowse: vi.fn(),
    onOpenGuide: vi.fn(),
    ...overrides,
  };
  render(<GameHomePage {...props} />);
  return props;
}

describe('GameHomePage', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    requestGameImport.mockReset();
    clearGameLaunches();
  });

  it('welcomes a game with no instances, with Create and Import', async () => {
    replies([]);
    const props = renderHome();

    const welcome = await screen.findByTestId('game-home-welcome');
    expect(welcome).toHaveTextContent('No instances yet');
    expect(screen.queryByTestId('game-home-continue')).not.toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: 'Jump back in' })).not.toBeInTheDocument();

    fireEvent.click(within(welcome).getByRole('button', { name: 'Create an instance' }));
    expect(props.onOpenInstances).toHaveBeenCalledTimes(1);
    fireEvent.click(within(welcome).getByRole('button', { name: 'Import from Mod Organizer 2' }));
    expect(requestGameImport).toHaveBeenCalledWith('skyrim-se');
    expect(props.onOpenInstances).toHaveBeenCalledTimes(2);
  });

  it('puts the instance played last up front, and the others under Jump back in', async () => {
    replies([
      card('old', 'Vanilla-plus', '2026-09-01 10:00:00'),
      card('never', 'Fresh', null),
      card('new', 'Grounded', '2026-10-30 18:30:00'),
    ]);
    const props = renderHome();

    const hero = await screen.findByTestId('game-home-continue');
    expect(within(hero).getByRole('heading', { name: 'Grounded' })).toBeInTheDocument();
    expect(hero).toHaveTextContent('Steam 1.6.1170');

    const recent = screen.getByRole('heading', { name: 'Jump back in' }).closest('section')!;
    const names = within(recent).getAllByRole('listitem').map((item) => item.textContent);
    expect(names[0]).toContain('Vanilla-plus');
    expect(names[1]).toContain('Fresh');
    expect(names[1]).toContain('Never played');

    fireEvent.click(within(recent).getByRole('button', { name: /Vanilla-plus/ }));
    expect(props.onOpenInstance).toHaveBeenCalledWith('old');
    fireEvent.click(within(hero).getByRole('button', { name: 'Open instance' }));
    expect(props.onOpenInstance).toHaveBeenCalledWith('new');
  });

  it('plays the instance from the card', async () => {
    replies([card('new', 'Grounded', '2026-10-30 18:30:00')]);
    renderHome();

    fireEvent.click(await screen.findByRole('button', { name: 'Play Grounded' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_instance_launch', expect.objectContaining({ instanceId: 'new', launchAnyway: false })),
    );
  });

  it('shows a running instance first, with Stop in place of Play', async () => {
    replies([card('new', 'Grounded', '2026-10-30 18:30:00'), card('run', 'Requiem', '2026-09-01 10:00:00', true)]);
    renderHome();

    const hero = await screen.findByTestId('game-home-continue');
    expect(within(hero).getByRole('heading', { name: 'Requiem' })).toBeInTheDocument();
    expect(hero).toHaveTextContent('Running now');
    expect(within(hero).getByRole('button', { name: 'Stop' })).toBeInTheDocument();
    expect(within(hero).queryByRole('button', { name: /^Play/ })).not.toBeInTheDocument();
  });

  it('says so when the instances cannot be read', async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'games_instance_list') throw { code: 'core_failed', message: 'disk' };
      if (command === 'games_catalog_list') return { entries: [], instances: [] };
      return null;
    });
    renderHome();
    expect(await screen.findByRole('alert')).toHaveTextContent('Your Skyrim SE instances could not be read');
    expect(screen.queryByTestId('game-home-welcome')).not.toBeInTheDocument();
  });

  it('shows the catalog strip, or an empty one, and both lead to Browse', async () => {
    replies([], [
      { id: 'skse', name: 'SKSE64', author: 'ianpatt', description: null, downloadStrategy: 'github_release', status: 'ok', pageUrl: null, compatibility: [] },
    ]);
    const props = renderHome();
    const strip = await screen.findByTestId('game-home-catalog');
    expect(strip).toHaveTextContent('SKSE64');
    fireEvent.click(within(strip).getByRole('button', { name: /SKSE64/ }));
    expect(props.onOpenBrowse).toHaveBeenCalled();
  });

  it('says the catalog has nothing for the game yet', async () => {
    replies([]);
    const props = renderHome();
    const empty = await screen.findByTestId('game-home-catalog-empty');
    expect(empty).toHaveTextContent('No Skyrim SE mods in the catalog yet');
    fireEvent.click(within(empty).getByRole('button', { name: 'Open Browse' }));
    expect(props.onOpenBrowse).toHaveBeenCalled();
  });
});
