import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('./PluginProvider', () => ({
  usePlugins: () => ({ plugins: [currentPlugin], refresh: vi.fn(async () => undefined), ready: true }),
}));
vi.mock('./PluginTheme', () => ({ PluginThemeSelect: () => null }));
vi.mock('./PluginSurfacePicker', () => ({ PluginSurfacePicker: () => null }));

import { PluginManager } from './PluginManager';
import type { PluginSummary } from './types';

let currentPlugin: PluginSummary;

function summary(overrides: Record<string, unknown> = {}): PluginSummary {
  return {
    id: 'agora.skyrim',
    name: 'Skyrim Special Edition',
    version: '1.0.0',
    description: 'Skyrim support.',
    license: 'MIT',
    sourceUrl: null,
    enabled: true,
    running: false,
    development: false,
    status: { state: 'ready' },
    statusText: 'Ready.',
    capabilities: [],
    declaredHosts: [],
    contributions: [],
    definitions: {},
    installedAt: '2026-01-01',
    updatedAt: '2026-01-01',
    updateSource: null,
    lastUpdateCheck: null,
    restorableData: null,
    droppedEvents: 0,
    official: { state: 'official', version: '1.0.0' },
    ...overrides,
  } as unknown as PluginSummary;
}

const officialInfo = {
  id: 'agora.skyrim',
  name: 'Skyrim Special Edition',
  description: 'Skyrim support.',
  gameIds: ['skyrim-se'],
  version: '2.0.0',
  previousVersion: '0.9.0',
  sizeBytes: 1024,
  minAgoraVersion: '1.0.0',
  state: 'updateAvailable',
  installedVersion: '1.0.0',
  canRollBack: true,
  installedStatus: { state: 'official', version: '1.0.0' },
  problem: null,
};

const consentPreview = {
  manifest: {
    manifest: 1,
    id: 'agora.skyrim',
    name: 'Skyrim Special Edition',
    version: '2.0.0',
    license: 'MIT',
  },
  requiredCapabilities: [
    { name: 'game:define', summary: 'Add a game to Agora', isMutating: false },
  ],
  optionalCapabilities: [],
  unsupportedCapabilities: [],
  replacesVersion: '1.0.0',
  addedCapabilities: [],
  addedHosts: [],
  updateSource: null,
  migratesData: false,
  fileCount: 1,
  uncompressedBytes: 1024,
};

function answer(officialPlugins: unknown[] = [officialInfo]) {
  invokeMock.mockImplementation(async (command: string) => {
    if (command === 'get_setting') return true;
    if (command === 'set_setting') return null;
    if (command === 'list_plugins') return [currentPlugin];
    if (command === 'list_official_plugins') return { plugins: officialPlugins, unavailableReason: null };
    if (command === 'update_official_plugin') {
      return { outcome: 'needsConsent', preview: consentPreview };
    }
    if (command === 'plugin_surfaces') return [];
    return null;
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  currentPlugin = summary();
  answer();
});

describe('Plugins page and official plugins', () => {
  it('shows the Official badge and offers the official update and rollback', async () => {
    render(<PluginManager />);

    expect(await screen.findByTestId('official-badge')).toHaveTextContent('Official');
    expect(screen.getByRole('button', { name: 'Update to 2.0.0' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Roll back to 0.9.0' })).toBeInTheDocument();
  });

  it('asks for consent before an official update and repeats the command with it', async () => {
    render(<PluginManager />);

    fireEvent.click(await screen.findByRole('button', { name: 'Update to 2.0.0' }));

    expect(await screen.findByText('It needs permission to:')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('update_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: false,
    });

    fireEvent.click(screen.getByRole('button', { name: 'Update' }));

    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('update_official_plugin', {
      pluginId: 'agora.skyrim',
      acceptCapabilities: true,
    }));
  });

  it('does not show an Official badge for an unverified install, and says why', async () => {
    currentPlugin = summary({
      official: { state: 'unverified', reason: 'files in the plugin folder changed after install' },
    });
    answer([]);
    render(<PluginManager />);

    expect(await screen.findByTestId('official-status-note')).toHaveTextContent(
      'files in the plugin folder changed after install',
    );
    expect(screen.queryByTestId('official-badge')).toBeNull();
  });
});
