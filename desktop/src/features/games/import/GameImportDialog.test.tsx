import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider } from '@/components/ui/confirm';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

const setup = {
  iniPath: 'D:/Setups/MO2/ModOrganizer.ini',
  gameName: 'Skyrim Special Edition',
  profiles: ['Default'],
  modCount: 2,
  foundBy: 'the usual place (ModOrganizer folder)',
};

const plan = {
  kind: 'plan',
  setup: setup.iniPath,
  profile: 'Default',
  mo2GameName: 'Skyrim Special Edition',
  gameId: 'skyrim-se',
  installId: 'steam:489830',
  installLocation: 'D:/Games/Skyrim Special Edition',
  setupGamePath: 'D:/Games/Skyrim Special Edition',
  store: 'steam',
  instanceName: 'Default (MO2 import)',
  mods: [
    {
      line: 1,
      folder: 'SkyUI',
      enabled: true,
      files: 12,
      bytes: 2048,
      provenance: 'Nexus mod 3863 (not checked)',
      pluginFiles: ['SkyUI_SE.esp'],
      metaIniExcluded: true,
    },
    {
      line: 2,
      folder: 'Old Patch',
      enabled: false,
      files: 3,
      bytes: 512,
      provenance: 'No meta.ini',
      pluginFiles: [],
      metaIniExcluded: false,
    },
  ],
  modsEnabled: 1,
  modsDisabled: 1,
  files: 15,
  bytes: 2560,
  metaIniExcluded: 1,
  separators: 0,
  unmanaged: [],
  problems: [{ line: 9, entry: 'Ghost Mod', reason: 'the mod’s folder is missing from the mods folder; skipped' }],
  overwrite: { present: false, files: 0, bytes: 0, generated: [], restFiles: 0 },
  plugins: { kept: true, listed: 2, active: 1, locked: [], implicitSkipped: [], notes: [] },
  localSettings: false,
  inis: [],
  saves: { local: false, files: 0, bytes: 0, note: '' },
  warnings: [],
};

const importRun = {
  kind: 'run',
  instanceId: 'inst-9',
  instanceName: 'Default (MO2 import)',
  baseId: 'base-1',
  baseBuilt: true,
  storedMods: 2,
  contentLayers: 2,
  disabledLayers: 1,
  duplicates: [],
  generatedLayers: [],
  pluginsWritten: 2,
  pluginFindings: [],
  inisCopied: [],
  savesCopied: 0,
  savesSkippedExisting: 0,
  savesFolder: null,
  nextSteps: ['line 9: Ghost Mod was not imported: the mod’s folder is missing from the mods folder; skipped'],
};

const api = vi.hoisted(() => ({
  scanMo2Setups: vi.fn(),
  importMo2Profile: vi.fn(),
  listenImportProgress: vi.fn(async () => () => undefined),
  NO_INSTALL_CODE: 'no_install',
}));
vi.mock('./api', () => api);

const settingsSection = vi.hoisted(() => ({ openGameSettings: vi.fn() }));
vi.mock('@/lib/settingsSection', () => settingsSection);

/** An install as discovery lists it. */
function install(installId: string, store: string, version: string, location: string) {
  return {
    installId,
    store,
    name: 'Skyrim Special Edition',
    location,
    storeVersion: version,
    storeBuild: null,
    addOnCount: 0,
    runtime: { status: 'identified', runtime: { game: 'skyrim-se', store, version, build: null }, source: 'executable' },
    installBytes: null,
  };
}

const chooseInstall = {
  kind: 'choose_install',
  gameId: 'skyrim-se',
  gameName: 'The Elder Scrolls V: Skyrim Special Edition',
  setupGamePath: 'C:/Program Files (x86)/Steam/steamapps/common/Skyrim Special Edition',
  installs: [
    install('steam:489830', 'steam', '1.6.1170', 'D:/SteamLibrary/steamapps/common/Skyrim Special Edition'),
    install('gog:1711230643', 'gog', '1.6.1179', 'D:/GOG Games/Skyrim Anniversary Edition'),
  ],
};

import { GameImportDialog } from './GameImportDialog';
import { importPercent, phaseWords } from './ImportProgressView';
import type { ImportProgress } from './api';

const progress = (fields: Partial<ImportProgress>): ImportProgress => ({
  phase: 'storing',
  currentMod: null,
  modsDone: 0,
  modsTotal: 107,
  bytesDone: 0,
  bytesTotal: 1000,
  ...fields,
});

describe('the import’s progress in numbers', () => {
  it('is a percentage only while bytes are being stored', () => {
    expect(importPercent(progress({ bytesDone: 256 }))).toBe(25);
    expect(importPercent(progress({ bytesDone: 1000, phase: 'instance' }))).toBeNull();
    expect(importPercent(null)).toBeNull();
    expect(importPercent(progress({ bytesTotal: 0 }))).toBeNull();
    expect(importPercent(progress({ bytesDone: 5000 }))).toBe(100);
  });

  it('has words for every stage, and for one it does not know', () => {
    expect(phaseWords(null)).toBe('Getting ready');
    expect(phaseWords(progress({ phase: 'layers' }))).toBe('Adding the mods to the instance');
    expect(phaseWords(progress({ phase: 'later' as ImportProgress['phase'] }))).toBe('Working');
  });
});

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(null);
  api.scanMo2Setups.mockResolvedValue({ setups: [setup], warnings: [] });
  api.importMo2Profile.mockImplementation(async (request: { dryRun: boolean }) => (request.dryRun ? plan : importRun));
});

function renderDialog() {
  const onOpenChange = vi.fn();
  const onImported = vi.fn();
  const onOpenInstance = vi.fn();
  render(
    <ConfirmProvider>
      <GameImportDialog gameId="skyrim-se" open onOpenChange={onOpenChange} onImported={onImported} onOpenInstance={onOpenInstance} />
    </ConfirmProvider>,
  );
  return { onOpenChange, onImported, onOpenInstance };
}

function currentStep(): string {
  const steps = screen.getByRole('list', { name: 'Import steps' });
  return within(steps).getAllByRole('listitem').find((item) => item.getAttribute('aria-current') === 'step')?.textContent ?? '';
}

describe('the Mod Organizer 2 import dialog', () => {
  it('walks every step, then offers the new instance', async () => {
    const { onImported, onOpenInstance, onOpenChange } = renderDialog();
    expect(screen.getByRole('heading', { name: 'Import from Mod Organizer 2' })).toBeInTheDocument();
    expect(currentStep()).toContain('Choose a setup');

    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    expect(currentStep()).toContain('Choose a profile');

    fireEvent.click(screen.getByRole('button', { name: 'Preview the import' }));
    expect(await screen.findByTestId('import-plan')).toBeInTheDocument();
    expect(currentStep()).toContain('Preview');

    fireEvent.click(screen.getByRole('button', { name: 'Import as a new instance' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Import profile' }));

    expect(await screen.findByTestId('import-done')).toHaveTextContent('Imported as “Default (MO2 import)”');
    expect(currentStep()).toContain('Imported');
    await waitFor(() => expect(onImported).toHaveBeenCalledTimes(1));

    fireEvent.click(screen.getByRole('button', { name: 'Open “Default (MO2 import)”' }));
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(onOpenInstance).toHaveBeenCalledWith('inst-9');
  });

  it('cannot be closed while the files are being copied', async () => {
    let finish: (value: unknown) => void = () => undefined;
    api.importMo2Profile.mockImplementation((request: { dryRun: boolean }) => (request.dryRun
      ? Promise.resolve(plan)
      : new Promise((resolve) => { finish = resolve; })));
    const { onOpenChange } = renderDialog();

    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    fireEvent.click(screen.getByRole('button', { name: 'Preview the import' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Import as a new instance' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Import profile' }));

    expect(await screen.findByTestId('import-progress')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeDisabled();
    expect(screen.queryByRole('alert')).toBeNull();
    fireEvent.keyDown(screen.getByTestId('game-import-dialog'), { key: 'Escape' });
    expect(onOpenChange).not.toHaveBeenCalled();
    // It says why it stayed, rather than ignoring the key.
    expect(await screen.findByRole('alert')).toHaveTextContent('can’t be stopped once it has started');
    fireEvent.click(screen.getByRole('button', { name: 'Close' }));
    expect(onOpenChange).not.toHaveBeenCalled();

    finish(importRun);
    expect(await screen.findByTestId('import-done')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Done' }));
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  async function startImport() {
    let finish: (value: unknown) => void = () => undefined;
    let fail: (reason: unknown) => void = () => undefined;
    let report: (progress: ImportProgress) => void = () => undefined;
    api.listenImportProgress.mockImplementation(async (handler?: (progress: ImportProgress) => void) => {
      if (handler) report = handler;
      return () => undefined;
    });
    api.importMo2Profile.mockImplementation((request: { dryRun: boolean }) => (request.dryRun
      ? Promise.resolve(plan)
      : new Promise((resolve, reject) => { finish = resolve; fail = reject; })));
    const handles = renderDialog();
    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    fireEvent.click(screen.getByRole('button', { name: 'Preview the import' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Import as a new instance' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Import profile' }));
    await screen.findByTestId('import-progress');
    await waitFor(() => expect(api.listenImportProgress).toHaveBeenCalled());
    return { ...handles, finish: (value: unknown) => finish(value), fail: (reason: unknown) => fail(reason), report: (value: ImportProgress) => act(() => report(value)) };
  }

  it('moves to a step of its own: a bar by bytes, the counts in words, the mod, and the read-only note', async () => {
    const { report, finish } = await startImport();
    expect(currentStep()).toContain('Importing');
    expect(screen.queryByTestId('import-plan')).not.toBeInTheDocument();
    expect(screen.getByTestId('import-read-only-note')).toHaveTextContent('Your Mod Organizer 2 setup is only read');
    expect(screen.getByTestId('import-quit-note')).toHaveTextContent('If you quit Agora now');
    expect(screen.getByTestId('import-elapsed')).toHaveTextContent(/\ds/);

    // No event yet: nothing is measured, so no number is shown.
    expect(screen.getByTestId('import-bar-indeterminate')).toBeInTheDocument();
    expect(screen.getByRole('progressbar')).not.toHaveAttribute('aria-valuenow');
    expect(screen.queryByTestId('import-percent')).toBeNull();

    report(progress({ currentMod: 'SkyUI', modsDone: 2, bytesDone: 400 }));
    expect(screen.getByTestId('import-percent')).toHaveTextContent('40%');
    expect(screen.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '40');
    expect(screen.getByTestId('import-current-mod')).toHaveTextContent('Now copying SkyUI');
    expect(screen.getByTestId('import-mods')).toHaveTextContent('2 of 107 mods');
    expect(screen.getByTestId('import-bytes')).toHaveTextContent(/400 B of 1(000 B|\.0 KB)/);

    // Past the mods, the stages cannot be measured: the bar stops claiming a percentage.
    report(progress({ phase: 'instance', modsDone: 107, bytesDone: 1000 }));
    expect(screen.queryByTestId('import-percent')).toBeNull();
    expect(screen.getByTestId('import-bar-indeterminate')).toBeInTheDocument();
    expect(screen.getByTestId('import-phase')).toHaveTextContent('Making the new instance');

    finish(importRun);
    expect(await screen.findByTestId('import-done')).toBeInTheDocument();
    expect(currentStep()).toContain('Imported');
  });

  it('a failed import shows the error, can be closed, and goes back to a fresh preview', async () => {
    const { fail, report, onOpenChange } = await startImport();
    report(progress({ currentMod: 'SkyUI', modsDone: 2, bytesDone: 400 }));
    const previews = api.importMo2Profile.mock.calls.length;

    fail({ code: 'core_failed', message: 'The disk is full.' });
    const failed = await screen.findByTestId('import-failed');
    expect(failed).toHaveTextContent('The import did not finish');
    expect(failed).toHaveTextContent('The disk is full.');
    expect(failed).toHaveTextContent('Your Mod Organizer 2 setup was only read');
    expect(screen.queryByTestId('import-progress')).toBeNull();
    expect(currentStep()).toContain('Importing');
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeEnabled();

    fireEvent.click(screen.getByRole('button', { name: '← Back to the preview' }));
    expect(await screen.findByTestId('import-plan')).toBeInTheDocument();
    expect(currentStep()).toContain('Preview');
    expect(api.importMo2Profile.mock.calls.length).toBe(previews + 1);
    expect(api.importMo2Profile).toHaveBeenLastCalledWith(expect.objectContaining({ dryRun: true }));

    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it('moves to the profile page on "Use this setup", and Back returns to the setups', async () => {
    renderDialog();
    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    expect(screen.queryByRole('list', { name: 'Setups found' })).not.toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Choose a profile' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '← Setups' }));
    expect(screen.getByRole('list', { name: 'Setups found' })).toBeInTheDocument();
    expect(currentStep()).toContain('Choose a setup');
  });

  it('describes a setup in words, without "(s)"', async () => {
    api.scanMo2Setups.mockResolvedValue({
      setups: [{ ...setup, gameName: null, profiles: [], modCount: null }, { ...setup, iniPath: 'E:/MO2/ModOrganizer.ini', profiles: ['A'], modCount: 1 }],
      warnings: [],
    });
    renderDialog();
    const list = await screen.findByRole('list', { name: 'Setups found' });
    expect(list).toHaveTextContent('Game not named');
    expect(list).toHaveTextContent('No profiles · mods folder unreadable');
    expect(list).toHaveTextContent('1 profile · 1 mod · found');
    expect(list.textContent).not.toContain('(s)');
  });

  it('asks which install to import onto when the setup’s game folder is not one Agora found', async () => {
    api.importMo2Profile.mockImplementation(async (request: { dryRun: boolean; install: string | null }) => {
      if (!request.dryRun) return importRun;
      return request.install === null
        ? chooseInstall
        : { ...plan, installId: request.install, installLocation: 'D:/GOG Games/Skyrim Anniversary Edition', setupGamePath: chooseInstall.setupGamePath };
    });
    renderDialog();
    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    fireEvent.click(screen.getByRole('button', { name: 'Preview the import' }));

    const choice = await screen.findByTestId('import-install-choice');
    expect(currentStep()).toContain('Preview');
    expect(choice).toHaveTextContent('C:/Program Files (x86)/Steam/steamapps/common/Skyrim Special Edition');
    expect(within(choice).getAllByRole('radio').every((radio) => radio.getAttribute('aria-checked') === 'false')).toBe(true);
    expect(screen.queryByTestId('import-plan')).not.toBeInTheDocument();

    fireEvent.click(within(choice).getByRole('radio', { name: /GOG 1\.6\.1179/ }));
    expect(await screen.findByTestId('import-plan-setup-path')).toHaveTextContent('Chosen instead of the setup’s own game folder');
    expect(api.importMo2Profile).toHaveBeenLastCalledWith(expect.objectContaining({ dryRun: true, install: 'gog:1711230643' }));

    fireEvent.click(screen.getByRole('button', { name: 'Import as a new instance' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Import profile' }));
    expect(await screen.findByTestId('import-done')).toBeInTheDocument();
    expect(api.importMo2Profile).toHaveBeenLastCalledWith(expect.objectContaining({ dryRun: false, install: 'gog:1711230643' }));
  });

  it('sends the user to Installs in Settings, never to a command, when the game has no install', async () => {
    api.importMo2Profile.mockRejectedValue({
      code: 'no_install',
      message: 'Agora has not found Skyrim on this computer. The setup expects it in C:/Games/Skyrim.',
    });
    renderDialog();
    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    fireEvent.click(screen.getByRole('button', { name: 'Preview the import' }));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Agora has not found Skyrim');
    expect(alert.textContent).not.toMatch(/agora games/);
    fireEvent.click(within(alert).getByRole('button', { name: 'Open Installs in Settings' }));
    expect(settingsSection.openGameSettings).toHaveBeenCalledWith('installs');
  });
});
