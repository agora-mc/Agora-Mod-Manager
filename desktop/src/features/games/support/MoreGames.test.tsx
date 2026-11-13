import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  MORE_GAMES_COPY,
  MORE_GAMES_DISMISSED_SETTING,
  MoreGamesCard,
} from './MoreGames';
import type { OfficialPluginInfo } from '@/features/plugins/types';

function info(overrides: Partial<OfficialPluginInfo> = {}): OfficialPluginInfo {
  return {
    id: 'agora.skyrim',
    name: 'Skyrim Special Edition',
    description: 'Skyrim, with load order and SKSE support.',
    gameIds: ['skyrim-se'],
    version: '1.0.0',
    previousVersion: null,
    sizeBytes: 1024,
    minAgoraVersion: '1.0.0',
    state: 'notInstalled',
    installedVersion: null,
    canRollBack: false,
    installedStatus: null,
    problem: null,
    ...overrides,
  };
}

const consentPreview = {
  manifest: {
    manifest: 1,
    id: 'agora.skyrim',
    name: 'Skyrim Special Edition',
    version: '1.0.0',
    license: 'MIT',
  },
  requiredCapabilities: [
    { name: 'game:define', summary: 'Add a game to Agora', isMutating: false },
  ],
  optionalCapabilities: [],
  unsupportedCapabilities: [],
  replacesVersion: null,
  addedCapabilities: [],
  addedHosts: [],
  updateSource: null,
  migratesData: false,
  fileCount: 1,
  uncompressedBytes: 1024,
  official: true,
};

function answer(overrides: {
  plugins?: unknown[];
  unavailableReason?: string | null;
  dismissed?: boolean;
  install?: (args?: Record<string, unknown>) => unknown;
  games?: unknown | (() => unknown);
  picked?: string | null;
  added?: { path: string; gameId: string; gameName: string };
} = {}) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === 'list_official_plugins') {
      return {
        plugins: overrides.plugins ?? [info()],
        unavailableReason: overrides.unavailableReason ?? null,
      };
    }
    if (command === 'get_setting') {
      if (args?.key === MORE_GAMES_DISMISSED_SETTING) return overrides.dismissed === true;
      return null;
    }
    if (command === 'set_setting') return null;
    if (command === 'install_official_plugin') {
      if (overrides.install) return overrides.install(args);
      return { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition' } };
    }
    if (command === 'games_list') {
      return typeof overrides.games === 'function' ? overrides.games() : overrides.games ?? { games: [] };
    }
    if (command === 'pick_directory') return overrides.picked ?? null;
    if (command === 'games_add_folder') {
      return overrides.added ?? {
        path: 'D:/Games/Skyrim',
        gameId: 'skyrim-se',
        gameName: 'Skyrim Special Edition',
      };
    }
    if (command === 'restart_app') return null;
    throw new Error(`Unexpected command ${command}`);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  answer();
});

describe('More games card', () => {
  it('shows one uninstalled game on Home and installs it after consent', async () => {
    answer({
      install: (args) => {
        if (args?.acceptCapabilities === true) {
          return { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition' } };
        }
        return { outcome: 'needsConsent', preview: consentPreview };
      },
    });
    render(<MoreGamesCard surface="home" />);

    expect(await screen.findByText('Skyrim Special Edition')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Install Skyrim Special Edition' }));

    expect(await screen.findByText('It needs permission to:')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('install_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: false,
      allowScripts: false,
    });
    // Skyrim is data only, so the prompt never asks about running code.
    expect(screen.queryByText(/runs code/)).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: 'Install' }));

    expect(await screen.findByText('Restart Agora to finish adding Skyrim Special Edition.')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('install_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: true,
      allowScripts: false,
    });
    expect(screen.queryByTestId('more-games')).not.toBeNull();
  });

  it('says the installed game was not found and offers to add its folder', async () => {
    let hasInstall = false;
    answer({
      install: (args) => {
        if (args?.acceptCapabilities === true) {
          return { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition' } };
        }
        return { outcome: 'needsConsent', preview: consentPreview };
      },
      games: () => ({
        games: hasInstall
          ? [{ gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [{}] }]
          : [],
      }),
      picked: 'D:/Games/Skyrim',
      added: { path: 'D:/Games/Skyrim', gameId: 'skyrim-se', gameName: 'Skyrim Special Edition' },
    });
    render(<MoreGamesCard surface="settings" />);

    fireEvent.click(await screen.findByRole('button', { name: 'Install Skyrim Special Edition' }));
    fireEvent.click(await screen.findByRole('button', { name: /^Install$/ }));

    expect(await screen.findByText(MORE_GAMES_COPY.notFound('Skyrim Special Edition'))).toBeInTheDocument();

    hasInstall = true;
    fireEvent.click(screen.getByRole('button', { name: 'Add game folder for Skyrim Special Edition' }));

    expect(await screen.findByText('Added D:/Games/Skyrim as Skyrim Special Edition.')).toBeInTheDocument();
    expect(screen.queryByText(MORE_GAMES_COPY.notFound('Skyrim Special Edition'))).toBeNull();
    expect(invokeMock).toHaveBeenCalledWith('games_add_folder', { path: 'D:/Games/Skyrim' });
  });

  it('is hidden on Home when every official game plugin is installed or the list is empty', async () => {
    answer({ plugins: [info({ state: 'upToDate', installedVersion: '1.0.0' })] });
    const { unmount } = render(<MoreGamesCard surface="home" />);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('list_official_plugins'));
    expect(screen.queryByTestId('more-games')).toBeNull();
    unmount();

    answer({ plugins: [] });
    render(<MoreGamesCard surface="home" />);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('list_official_plugins'));
    expect(screen.queryByTestId('more-games')).toBeNull();
  });

  it('shows a short error in Settings and nothing on Home when the list fails', async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'list_official_plugins') throw new Error('registry missing');
      if (command === 'get_setting') return false;
      return null;
    });

    const settings = render(<MoreGamesCard surface="settings" />);
    expect(await screen.findByRole('alert')).toHaveTextContent(MORE_GAMES_COPY.unavailable);
    settings.unmount();

    render(<MoreGamesCard surface="home" />);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('list_official_plugins'));
    expect(screen.queryByTestId('more-games')).toBeNull();
  });

  it('remembers a dismissal on Home', async () => {
    const first = render(<MoreGamesCard surface="home" />);
    expect(await screen.findByText('Skyrim Special Edition')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: MORE_GAMES_COPY.dismiss }));

    expect(screen.queryByTestId('more-games')).toBeNull();
    expect(invokeMock).toHaveBeenCalledWith('set_setting', {
      key: MORE_GAMES_DISMISSED_SETTING,
      value: true,
    });
    first.unmount();

    answer({ dismissed: true });
    render(<MoreGamesCard surface="home" />);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('get_setting', {
      key: MORE_GAMES_DISMISSED_SETTING,
    }));
    expect(screen.queryByTestId('more-games')).toBeNull();
  });

  it('turns a hash mismatch into plain words', async () => {
    answer({
      install: () => {
        throw { code: 'ERR_OFFICIAL_PLUGIN_HASH', message: 'The download for `agora.skyrim` does not match' };
      },
    });
    render(<MoreGamesCard surface="settings" />);

    fireEvent.click(await screen.findByRole('button', { name: 'Install Skyrim Special Edition' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(MORE_GAMES_COPY.hashMismatch);
  });

  it('asks the per-plugin code question only for a package that runs code', async () => {
    answer({
      install: (args) => {
        if (args?.acceptCapabilities === true) {
          return { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition' } };
        }
        return { outcome: 'needsConsent', preview: { ...consentPreview, runsCode: true } };
      },
    });
    render(<MoreGamesCard surface="settings" />);

    fireEvent.click(await screen.findByRole('button', { name: 'Install Skyrim Special Edition' }));

    expect(await screen.findByText('Skyrim Special Edition runs code')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Install and allow code (recommended)' }));

    expect(await screen.findByText('Restart Agora to finish adding Skyrim Special Edition.')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('install_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: true,
      allowScripts: true,
    });
  });

  it('lists an official dependency the install will also add, and asks about its code', async () => {
    const dependency = {
      ...consentPreview,
      manifest: {
        manifest: 1,
        id: 'agora.creation-engine',
        name: 'Creation Engine',
        version: '1.0.0',
        license: 'MIT',
      },
      requiredCapabilities: [
        { name: 'game:read', summary: 'Read game data', isMutating: false },
      ],
      runsCode: true,
      codeChanged: true,
      official: true,
    };
    answer({
      install: (args) => {
        if (args?.acceptCapabilities === true) {
          return { outcome: 'installed', plugin: { id: 'agora.skyrim', name: 'Skyrim Special Edition' } };
        }
        return { outcome: 'needsConsent', preview: { ...consentPreview, alsoInstalls: [dependency] } };
      },
    });
    render(<MoreGamesCard surface="settings" />);

    fireEvent.click(await screen.findByRole('button', { name: 'Install Skyrim Special Edition' }));

    expect(await screen.findByText('Creation Engine 1.0.0')).toBeInTheDocument();
    expect(screen.getByText('Needed by Skyrim Special Edition')).toBeInTheDocument();
    expect(screen.getByText('Read game data')).toBeInTheDocument();
    expect(screen.getByText('Creation Engine runs code')).toBeInTheDocument();
    // A dependency with code installs without it, so both answers are offered.
    expect(screen.getByRole('button', { name: 'Install without running code' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Install and allow code (recommended)' }));

    expect(await screen.findByText('Restart Agora to finish adding Skyrim Special Edition.')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('install_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: true,
      allowScripts: true,
    });
  });

  it('lists without consulting a global plugin switch', async () => {
    answer({ plugins: [info()] });
    render(<MoreGamesCard surface="settings" />);

    expect(await screen.findByText('Skyrim Special Edition')).toBeInTheDocument();
    expect(invokeMock).not.toHaveBeenCalledWith('plugins_enabled');
    expect(invokeMock).not.toHaveBeenCalledWith('plugins_emergency_stop');
  });
});
