import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { ConfirmProvider } from '@/components/ui/confirm';
import { InstanceIniSavesTab } from './index';

const SAVES = {
  instanceId: 'inst-1',
  choice: 'shared',
  inUse: { path: 'C:/Shared', exists: true, saves: 0, newest: null },
  other: { path: 'C:/Own', exists: false, saves: 0, newest: null },
  settingFile: 'Skyrim.ini',
  setting: 'General/sSaveFolder',
  settingValue: null,
  settingSource: 'nothing',
};

const MOD_INI = {
  path: 'SKSE/Plugins/SSEDisplayTweaks.ini',
  modId: 'aaaa1111',
  modName: 'SSE Display Tweaks',
  overridden: false,
  shadowedBy: null,
  modEnabled: true,
};

const CONTENTS = {
  instancePath: MOD_INI.path,
  source: 'mod',
  entries: [{ section: 'Display', key: 'fGamma', value: '1.0' }],
};

let replies: Record<string, (args: Record<string, unknown>) => unknown>;

function route() {
  invokeMock.mockImplementation(async (command: string, args: Record<string, unknown> = {}) => {
    const reply = replies[command];
    if (!reply) throw new Error(`unmocked command in an INI test: ${command}`);
    return reply(args);
  });
}

function mountTab() {
  return render(
    <ConfirmProvider>
      <InstanceIniSavesTab gameId="skyrim-se" instanceId="inst-1" />
    </ConfirmProvider>,
  );
}

describe('the mod INI section', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    replies = {
      games_saves_get: () => SAVES,
      games_ini_get: () => ({ files: [], contents: null }),
      games_mod_inis_list: () => [MOD_INI],
      games_mod_ini_get: () => CONTENTS,
    };
    route();
  });

  it('opens a mod INI in the same per-key editor as the game INI files', async () => {
    mountTab();
    expect(await screen.findByText('From your mods')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /SKSE\/Plugins\/SSEDisplayTweaks\.ini/ })).toHaveTextContent(
      'SSE Display Tweaks',
    );
    expect(await screen.findByText('[Display]')).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Value of fGamma in [Display]' })).toHaveValue('1.0');
    expect(screen.getByRole('button', { name: 'Save key' })).toBeInTheDocument();
  });

  it('says quietly that no enabled mod ships an INI, rather than failing', async () => {
    replies.games_mod_inis_list = () => [];
    mountTab();
    expect(await screen.findByText("None of this instance's enabled mods ships an INI file.")).toBeInTheDocument();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('saves one key into the instance copy, leaving the mod file alone', async () => {
    replies.games_mod_ini_set = (args) => ({
      ...CONTENTS,
      source: 'override',
      entries: [{ section: 'Display', key: 'fGamma', value: args.value }],
    });
    mountTab();
    const value = await screen.findByRole('textbox', { name: 'Value of fGamma in [Display]' });
    fireEvent.change(value, { target: { value: '2.2' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_mod_ini_set', {
        instanceId: 'inst-1',
        path: MOD_INI.path,
        section: 'Display',
        key: 'fGamma',
        value: '2.2',
      }),
    );
    expect(await screen.findByRole('status')).toHaveTextContent("The mod's own file was not changed.");
    expect(screen.getByText('Edited for this instance')).toBeInTheDocument();
    expect(value).toHaveValue('2.2');
  });

  it('adds a key and removes one through the same editor', async () => {
    replies.games_mod_ini_set = (args) => ({
      ...CONTENTS,
      source: 'override',
      entries: [{ section: args.section as string, key: args.key as string, value: (args.value as string) ?? '' }],
    });
    mountTab();
    await screen.findByRole('textbox', { name: 'Value of fGamma in [Display]' });

    fireEvent.change(screen.getByPlaceholderText('General'), { target: { value: 'Video' } });
    fireEvent.change(screen.getByPlaceholderText('sLanguage'), { target: { value: 'bDoGodRays' } });
    const valueInputs = screen.getAllByRole('textbox');
    fireEvent.change(valueInputs[valueInputs.length - 1], { target: { value: '1' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save key' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith(
        'games_mod_ini_set',
        expect.objectContaining({ section: 'Video', key: 'bDoGodRays', value: '1' }),
      ),
    );

    fireEvent.click(screen.getByRole('button', { name: 'Remove' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove key' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith(
        'games_mod_ini_set',
        expect.objectContaining({ section: 'Video', key: 'bDoGodRays', value: null }),
      ),
    );
  });

  it('lets a turned-off mod be reset but not edited', async () => {
    replies.games_mod_inis_list = () => [{ ...MOD_INI, overridden: true, modEnabled: false }];
    replies.games_mod_ini_get = () => ({ ...CONTENTS, source: 'override' });
    replies.games_mod_ini_reset = () => [];
    mountTab();
    const value = await screen.findByRole('textbox', { name: 'Value of fGamma in [Display]' });
    expect(value).toBeDisabled();
    expect(screen.getByText(/Its mod is turned off/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Save key' })).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: "Reset to the mod's version" }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: "Reset to the mod's version" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_mod_ini_reset', {
        instanceId: 'inst-1',
        path: MOD_INI.path,
      }),
    );
    expect(await screen.findByRole('status')).toHaveTextContent('is back to the version its mod ships');
    expect(await screen.findByText("None of this instance's enabled mods ships an INI file.")).toBeInTheDocument();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('shows a clear message and no editor for a file that is not text', async () => {
    replies.games_mod_ini_get = () =>
      Promise.reject({ code: 'core_failed', message: "'binary.ini' is not a text INI file, so it cannot be edited here" });
    mountTab();
    expect(
      await screen.findByText("'binary.ini' is not a text INI file, so it cannot be edited here"),
    ).toBeInTheDocument();
    expect(screen.queryByRole('textbox', { name: /Value of/ })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Save key' })).toBeNull();
  });

  it('shows a failed list as an error, never as no files', async () => {
    replies.games_mod_inis_list = () =>
      Promise.reject({ code: 'core_failed', message: 'The overrides folder cannot be read.' });
    mountTab();
    expect(await screen.findByText('The overrides folder cannot be read.')).toBeInTheDocument();
    expect(screen.queryByText("None of this instance's enabled mods ships an INI file.")).toBeNull();
  });
});
