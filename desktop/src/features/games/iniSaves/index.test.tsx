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
  path: MOD_INI.path,
  source: 'mod',
  contents: '[Display]\nfGamma=1.0\n',
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

  it('lists each mod INI with its path and mod name, and opens its contents', async () => {
    mountTab();
    expect(await screen.findByText('From your mods')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /SKSE\/Plugins\/SSEDisplayTweaks\.ini/ })).toHaveTextContent(
      'SSE Display Tweaks',
    );
    const editor = await screen.findByRole('textbox', { name: `Contents of ${MOD_INI.path}` });
    expect(editor).toHaveValue(CONTENTS.contents);
  });

  it('says quietly that no enabled mod ships an INI, rather than failing', async () => {
    replies.games_mod_inis_list = () => [];
    mountTab();
    expect(await screen.findByText("None of this instance's enabled mods ships an INI file.")).toBeInTheDocument();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('saves the whole file as the instance copy', async () => {
    replies.games_mod_ini_set = (args) => ({
      path: MOD_INI.path,
      source: 'override',
      contents: args.contents,
    });
    mountTab();
    const editor = await screen.findByRole('textbox', { name: `Contents of ${MOD_INI.path}` });
    fireEvent.change(editor, { target: { value: '[Display]\nfGamma=2.2\n' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save for this instance' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_mod_ini_set', {
        instanceId: 'inst-1',
        path: MOD_INI.path,
        contents: '[Display]\nfGamma=2.2\n',
      }),
    );
    expect(await screen.findByRole('status')).toHaveTextContent("Saved this instance's copy");
    expect(screen.getByText('Edited for this instance')).toBeInTheDocument();
  });

  it('lets a turned-off mod be reset but not edited', async () => {
    replies.games_mod_inis_list = () => [
      { ...MOD_INI, overridden: true, modEnabled: false },
    ];
    replies.games_mod_ini_get = () => ({ ...CONTENTS, source: 'override' });
    replies.games_mod_ini_reset = () => [];
    mountTab();
    const editor = await screen.findByRole('textbox', { name: `Contents of ${MOD_INI.path}` });
    expect(editor).toBeDisabled();
    expect(screen.getByText(/Its mod is turned off/)).toBeInTheDocument();

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

  it('shows a failed list as an error, never as no files', async () => {
    replies.games_mod_inis_list = () =>
      Promise.reject({ code: 'core_failed', message: 'The overrides folder cannot be read.' });
    mountTab();
    expect(await screen.findByText('The overrides folder cannot be read.')).toBeInTheDocument();
    expect(screen.queryByText("None of this instance's enabled mods ships an INI file.")).toBeNull();
  });
});
