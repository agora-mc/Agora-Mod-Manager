import { fireEvent, render, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const importVortexProfile = vi.hoisted(() => vi.fn());
const scanVortex = vi.hoisted(() => vi.fn());
vi.mock('./api', () => ({
  importVortexProfile,
  importVortexProfiles: vi.fn(),
  suggestVortexProfiles: vi.fn(),
  scanVortex,
}));

vi.mock('@/components/ui/confirm', () => ({
  useConfirm: () => ({ confirm: vi.fn(async () => true) }),
}));

import { VortexImportFlow } from './VortexImportFlow';

const scanReply = {
  store: 'C:/Vortex/state.v2',
  warnings: [],
  games: [
    {
      vortexGame: 'skyrimse',
      game: 'skyrim-se',
      gameName: 'Skyrim Special Edition',
      supported: true,
      reason: null,
      discoveredPath: null,
      install: 'steam:489830',
      needsFolderAdd: false,
      detection: null,
      staging: 'C:/Vortex/mods',
      stagingError: null,
      stagingMarker: true,
      mods: 2,
      profiles: [
        {
          id: 'p1',
          name: 'Default',
          modsEnabled: 2,
          modsTotal: 2,
          pluginsTxt: true,
          loadorderTxt: true,
          localSettings: true,
          localSaves: true,
        },
      ],
      manifestFound: false,
      manifestFiles: 0,
      manifestSearched: [],
    },
  ],
};

const planReply = {
  kind: 'plan' as const,
  store: 'C:/Vortex/state.v2',
  vortexGame: 'skyrimse',
  gameId: 'skyrim-se',
  gameName: 'Skyrim Special Edition',
  profile: 'Default',
  instanceName: 'Default (Vortex)',
  installId: 'steam:489830',
  installLocation: 'C:/Games/Skyrim',
  wouldAddFolder: false,
  staging: 'C:/Vortex/mods',
  mods: [],
  modsEnabled: 0,
  modsDisabled: 0,
  modsLeftOut: [],
  files: 0,
  bytes: 0,
  manifest: { path: '', found: false, files: 0, matched: 0, damaged: null, searched: [] },
  order: [],
  disagreements: [],
  plugins: { kept: false, listed: 0, active: 0, notes: [] },
  userlist: { path: null, rules: [], refused: [], notImported: [] },
  localSettings: true,
  inis: [],
  localSaves: true,
  saves: { local: true, files: 1, bytes: 10, note: '' },
  problems: [],
  warnings: [],
};

const runReply = {
  kind: 'run' as const,
  instanceId: 'inst-1',
  instanceName: 'Default (Vortex)',
  storedMods: 0,
  contentLayers: 0,
  disabledLayers: 0,
  duplicates: [],
  pluginsWritten: null,
  pluginFindings: [],
  rulesAdded: 0,
  rulesRefused: [],
  inisCopied: [],
  savesCopied: 0,
  savesSkippedExisting: 0,
  savesFolder: null,
  nextSteps: [],
};

beforeEach(() => {
  importVortexProfile.mockReset();
  scanVortex.mockReset();
});

describe('VortexImportFlow', () => {
  it('reports completion once with a stable report', async () => {
    scanVortex.mockResolvedValue(scanReply);
    importVortexProfile.mockResolvedValueOnce(planReply).mockResolvedValueOnce(runReply);
    const onStateChange = vi.fn();
    const { findByTestId, getByRole, rerender } = render(
      <VortexImportFlow onStateChange={onStateChange} />,
    );

    fireEvent.click(await findByTestId('vortex-profile-p1'));
    await findByTestId('vortex-plan-page');
    fireEvent.click(getByRole('button', { name: 'Import as a new instance' }));

    await waitFor(() => {
      expect(onStateChange).toHaveBeenCalledWith(
        expect.objectContaining({
          step: 'done',
          running: false,
          report: { instanceId: 'inst-1', instanceName: 'Default (Vortex)' },
        }),
      );
    });
    const doneCalls = onStateChange.mock.calls.filter(
      ([state]) => state.step === 'done' && state.report?.instanceId === 'inst-1',
    );
    expect(doneCalls).toHaveLength(1);

    rerender(<VortexImportFlow onStateChange={onStateChange} />);
    rerender(<VortexImportFlow onStateChange={onStateChange} />);
    await waitFor(() => {
      const after = onStateChange.mock.calls.filter(
        ([state]) => state.step === 'done' && state.report?.instanceId === 'inst-1',
      );
      expect(after).toHaveLength(1);
    });
  });

  it('previews and runs with the same allMods/copySaves the player approved', async () => {
    scanVortex.mockResolvedValue(scanReply);
    importVortexProfile
      .mockResolvedValueOnce(planReply)
      .mockResolvedValueOnce(planReply)
      .mockResolvedValueOnce(runReply);
    const { findByTestId, getByRole } = render(<VortexImportFlow />);

    fireEvent.click(await findByTestId('vortex-profile-p1'));
    await findByTestId('vortex-plan-page');
    expect(importVortexProfile).toHaveBeenNthCalledWith(
      1,
      expect.objectContaining({ dryRun: true, copySaves: false, allMods: false }),
    );

    fireEvent.click(getByRole('switch', { name: 'Copy the profile’s saves into the new instance' }));
    await waitFor(() => expect(importVortexProfile).toHaveBeenCalledTimes(2));
    expect(importVortexProfile).toHaveBeenNthCalledWith(
      2,
      expect.objectContaining({ dryRun: true, copySaves: true, allMods: false }),
    );

    fireEvent.click(getByRole('button', { name: 'Import as a new instance' }));
    await waitFor(() => expect(importVortexProfile).toHaveBeenCalledTimes(3));
    expect(importVortexProfile).toHaveBeenNthCalledWith(
      3,
      expect.objectContaining({ dryRun: false, copySaves: true, allMods: false }),
    );
  });
});
