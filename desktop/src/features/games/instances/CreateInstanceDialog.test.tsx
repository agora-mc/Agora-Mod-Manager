import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));

import { CreateInstanceDialog } from './CreateInstanceDialog';

const skyrim = {
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  installs: [{
    installId: 'steam:489830',
    store: 'Steam',
    name: 'Skyrim Special Edition',
    location: 'D:/Steam/steamapps/common/Skyrim Special Edition',
    storeVersion: '1.6.1170',
    storeBuild: null,
    addOnCount: 0,
    runtime: { status: 'identified', runtime: { game: 'skyrim-se', store: 'steam', version: '1.6.1170', build: null }, source: 'executable' },
    installBytes: null,
  }],
};
const valheim = { ...skyrim, gameId: 'valheim', name: 'Valheim' };

const enabledSkyrim = {
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  source: { kind: 'builtIn' },
  enabled: true,
  canDisable: true,
};

/** Answers the reads the dialog makes; `settings` are the keys the user already saved. */
function answer(settings: Record<string, unknown> = {}, support: unknown = [enabledSkyrim]) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === 'get_setting') {
      const key = String((args as { key?: unknown } | undefined)?.key ?? '');
      return Object.prototype.hasOwnProperty.call(settings, key) ? settings[key] : null;
    }
    if (command === 'games_list') return { games: [skyrim, valheim] };
    if (command === 'games_support_list') return support;
    if (command === 'games_instance_create') {
      return { instanceId: 'skyrim-se-1', name: 'Grounded', base: 'pinned', reusedBase: false, baseId: null, unpinnedReason: null };
    }
    throw new Error(`unexpected command ${command}`);
  });
}

function renderDialog(gameId = 'skyrim-se') {
  return render(
    <CreateInstanceDialog open gameId={gameId} onOpenChange={() => undefined} onCreated={() => undefined} />,
  );
}

async function waitForInstalls() {
  await screen.findByRole('heading', { name: /^New instance of/ });
  await screen.findByLabelText('Name');
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe('CreateInstanceDialog per-game defaults', () => {
  it('starts from the safe choices when the game has no settings', async () => {
    answer();
    renderDialog();
    await waitForInstalls();

    expect(screen.getByLabelText('How it will run')).toHaveValue('auto');
    expect(screen.getByRole('radio', { name: /Linked base/ })).toHaveAttribute('aria-checked', 'true');

    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Grounded' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create' }));

    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('games_instance_create', expect.objectContaining({
      gameId: 'skyrim-se',
      baseMode: 'linked',
      deployment: 'auto',
    })));
  });

  it('preselects the deployment saved for this game and sends it with the new instance', async () => {
    answer({ 'game.skyrim-se.deployment': 'copies' });
    renderDialog();
    await waitForInstalls();

    expect(screen.getByLabelText('How it will run')).toHaveValue('copies');

    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Grounded' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create' }));

    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('games_instance_create', expect.objectContaining({
      deployment: 'copies',
    })));
  });

  it('preselects the saved base type for the new instance', async () => {
    answer({ 'game.skyrim-se.baseType': 'copied' });
    renderDialog();
    await waitForInstalls();

    expect(screen.getByRole('radio', { name: /Copied base/ })).toHaveAttribute('aria-checked', 'true');
  });

  it('marks the recommended choices, even when the saved default is another one', async () => {
    answer({ 'game.skyrim-se.baseType': 'copied', 'game.skyrim-se.deployment': 'copies' });
    renderDialog();
    await waitForInstalls();

    const linked = screen.getByRole('radio', { name: /Linked base/ });
    expect(linked).toHaveAttribute('aria-checked', 'false');
    expect(linked).toHaveTextContent('Recommended');
    expect(screen.getByRole('radio', { name: /Copied base/ })).not.toHaveTextContent('Recommended');
    expect(screen.getByRole('option', { name: 'Automatic (recommended)' })).toBeInTheDocument();
    expect(screen.getByRole('option', { name: 'Copied files' })).toBeInTheDocument();
  });

  it('says what is missing until there is a name', async () => {
    answer();
    renderDialog();
    await waitForInstalls();

    expect(screen.getByLabelText('Name')).toHaveAttribute('placeholder', 'e.g. Survival playthrough');
    expect(screen.getByText('Type a name to enable Create.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Create' })).toBeDisabled();
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Grounded' } });
    expect(screen.queryByText('Type a name to enable Create.')).toBeNull();
    expect(screen.getByRole('button', { name: 'Create' })).toBeEnabled();
  });

  it('does not use another game\'s deployment for this one', async () => {
    answer({
      'game.skyrim-se.deployment': 'copies',
      'game.valheim.deployment': 'virtual',
    }, [enabledSkyrim, { ...enabledSkyrim, gameId: 'valheim', name: 'Valheim' }]);
    renderDialog('valheim');
    await waitForInstalls();

    expect(screen.getByLabelText('How it will run')).toHaveValue('virtual');
  });

  it('hides the form and says why when the game is turned off', async () => {
    answer({}, [{ ...enabledSkyrim, enabled: false }]);
    renderDialog();
    await screen.findByRole('heading', { name: /^New instance of/ });

    expect(await screen.findByText(/This game is turned off/)).toBeInTheDocument();
    expect(screen.queryByLabelText('Name')).toBeNull();
  });

  it('still offers the game when the support list failed, because a failed list hides nothing', async () => {
    answer({}, new Error('games_support_list did not return a list of games.'));
    renderDialog();
    await waitForInstalls();
    expect(screen.getByLabelText('Name')).toBeInTheDocument();
  });
});
