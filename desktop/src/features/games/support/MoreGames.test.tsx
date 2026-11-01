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
};

function answer(overrides: {
  plugins?: unknown[];
  unavailableReason?: string | null;
  dismissed?: boolean;
  install?: (args?: Record<string, unknown>) => unknown;
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

    expect(await screen.findByText('Restart Agora to add Skyrim Special Edition.')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('install_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: true,
      allowScripts: false,
    });
    expect(screen.queryByTestId('more-games')).not.toBeNull();
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

    expect(await screen.findByText('Skyrim Special Edition runs code. Allow it?')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Install and allow code' }));

    expect(await screen.findByText('Restart Agora to add Skyrim Special Edition.')).toBeInTheDocument();
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
