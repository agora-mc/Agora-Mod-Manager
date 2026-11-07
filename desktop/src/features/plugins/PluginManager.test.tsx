// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { PluginManager } from './PluginManager';
import type { InstallPreview, PluginSummary } from './types';

const mocks = vi.hoisted(() => ({
  plugins: [] as unknown[],
  refresh: vi.fn(async () => {}),
  stop: vi.fn(),
  setStop: vi.fn(async () => {}),
  setScripts: vi.fn(async () => ({})),
  preview: vi.fn(),
  install: vi.fn(),
  invoke: vi.fn(),
}));

vi.mock('./PluginProvider', () => ({
  usePlugins: () => ({ plugins: mocks.plugins, refresh: mocks.refresh, ready: true }),
}));
vi.mock('./api', () => ({
  addPluginDevelopmentFolder: vi.fn(),
  applyPluginUpdate: vi.fn(),
  checkPluginUpdate: vi.fn(),
  disableAllPlugins: vi.fn(),
  installPluginPackage: mocks.install,
  pluginsEmergencyStop: mocks.stop,
  previewPluginFolder: vi.fn(),
  previewPluginPackage: mocks.preview,
  readPluginLog: vi.fn(),
  restorePluginData: vi.fn(),
  setPluginEnabled: vi.fn(),
  setPluginScriptsAllowed: mocks.setScripts,
  setPluginsEmergencyStop: mocks.setStop,
  uninstallPlugin: vi.fn(),
}));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
vi.mock('@/lib/tauri', () => ({
  formatError: (e: unknown) => String(e),
  getSetting: vi.fn(async () => false),
  setSetting: vi.fn(async () => {}),
}));
vi.mock('@/components/Toast', () => ({ showToast: vi.fn() }));
vi.mock('@/components/ui/confirm', () => ({ useConfirm: () => ({ confirm: vi.fn() }) }));
vi.mock('./PluginTheme', () => ({ PluginThemeSelect: () => null }));
vi.mock('./PluginSurfacePicker', () => ({ PluginSurfacePicker: () => null }));
vi.mock('./PluginSettingsForm', () => ({ PluginSettingsForm: () => null }));

function summary(over: Partial<PluginSummary>): PluginSummary {
  return {
    id: 'acme.one',
    name: 'One',
    version: '1.0.0',
    description: null,
    license: 'MIT',
    sourceUrl: null,
    enabled: true,
    running: false,
    development: false,
    status: { state: 'ready' },
    statusText: 'Ready',
    capabilities: [],
    declaredHosts: [],
    contributions: [],
    definitions: {},
    installedAt: '',
    updatedAt: '',
    updateSource: null,
    lastUpdateCheck: null,
    restorableData: null,
    droppedEvents: 0,
    runsCode: true,
    scriptsAllowed: false,
    ...over,
  } as unknown as PluginSummary;
}

function preview(over: Partial<InstallPreview>): InstallPreview {
  return {
    manifest: { name: 'Scripted', version: '1.0.0', license: 'MIT', network: { hosts: [] } },
    requiredCapabilities: [],
    optionalCapabilities: [],
    unsupportedCapabilities: [],
    replacesVersion: null,
    addedCapabilities: [],
    addedHosts: [],
    updateSource: null,
    migratesData: false,
    fileCount: 2,
    uncompressedBytes: 10,
    ...over,
  } as unknown as InstallPreview;
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.plugins = [];
  mocks.stop.mockResolvedValue(false);
});

describe('the emergency stop', () => {
  it('is a plain "Stop all plugin code" switch, off by default, that engages the stop', async () => {
    render(<PluginManager />);
    const stop = await screen.findByRole('switch', { name: 'Stop all plugin code' });
    await waitFor(() => expect(stop.getAttribute('aria-checked')).toBe('false'));

    fireEvent.click(stop);

    await waitFor(() => expect(mocks.setStop).toHaveBeenCalledWith(true));
  });

  it('shows as engaged when it cannot be read', async () => {
    mocks.stop.mockRejectedValue(new Error('no'));
    render(<PluginManager />);
    const stop = await screen.findByRole('switch', { name: 'Stop all plugin code' });
    await waitFor(() => expect(stop.getAttribute('aria-checked')).toBe('true'));
  });
});

describe('per-plugin code permission', () => {
  it('has its own switch for a plugin with code, and none for a data-only plugin', async () => {
    mocks.plugins = [
      summary({ id: 'acme.one', name: 'One', scriptsAllowed: false }),
      summary({ id: 'acme.data', name: 'Data', runsCode: false, scriptsAllowed: false }),
    ];
    render(<PluginManager />);

    const allow = await screen.findByRole('switch', { name: 'Allow One to run code' });
    expect(screen.queryByRole('switch', { name: 'Allow Data to run code' })).toBeNull();

    fireEvent.click(allow);

    await waitFor(() => expect(mocks.setScripts).toHaveBeenCalledWith('acme.one', true));
  });
});

describe('the install prompt for a plugin with code', () => {
  async function openPrompt(runsCode: boolean) {
    mocks.invoke.mockResolvedValue('C:/plugin.zip');
    mocks.preview.mockResolvedValue(preview({ runsCode, addsCode: runsCode }));
    mocks.install.mockResolvedValue({ name: 'Scripted', version: '1.0.0' });
    render(<PluginManager />);
    fireEvent.click(await screen.findByRole('button', { name: /Install from file/ }));
  }

  it('asks once, and installing without running code sends allowScripts = false', async () => {
    await openPrompt(true);
    expect(await screen.findByText('Scripted runs code. Allow it?')).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: 'Install without running code' }));

    await waitFor(() =>
      expect(mocks.install).toHaveBeenCalledWith('C:/plugin.zip', true, false),
    );
  });

  it('allowing it sends allowScripts = true', async () => {
    await openPrompt(true);
    fireEvent.click(await screen.findByRole('button', { name: 'Install and allow code' }));
    await waitFor(() =>
      expect(mocks.install).toHaveBeenCalledWith('C:/plugin.zip', true, true),
    );
  });

  it('does not ask about a data-only plugin', async () => {
    await openPrompt(false);
    await screen.findByRole('button', { name: 'Install' });
    expect(screen.queryByText(/runs code/)).toBeNull();
    expect(screen.queryByRole('button', { name: /without running code/ })).toBeNull();
  });
});
