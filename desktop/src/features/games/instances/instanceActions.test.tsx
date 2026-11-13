/**
 * The game instance actions that ask first or can be refused: Stop asks before closing the game,
 * the header shows only what the instance allows (Rename, Open in Folder, Stop), and Recently
 * deleted asks for a new name when the deleted instance's place was taken, and empties only this
 * game's list.
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider } from '@/components/ui/confirm';
import { GameInstancePage } from '../shell/GameInstancePage';
import { clearInstanceSessionCache } from './instanceSession';
import { GameRecentlyDeleted } from './GameRecentlyDeleted';
import { GameCloneDialog } from './instanceActions';
import {
  checkGameInstance,
  cloneGameInstance,
  getGameInstance,
  listDeletedGameInstances,
  openGameInstanceFolder,
  purgeDeletedGameInstances,
  renameGameInstance,
  restoreDeletedGameInstance,
  stopGameInstance,
  type DeletedEntry,
  type InstanceCapabilities,
  type InstanceDetail,
} from './api';

vi.mock('../content', () => ({ InstanceContentTab: () => null }));
vi.mock('../plugins', () => ({ InstancePluginsTab: () => null }));
vi.mock('../iniSaves', () => ({ InstanceIniSavesTab: () => null }));
vi.mock('../tools', () => ({ InstanceToolsTab: () => null }));

vi.mock('./crashAnswerApi', () => ({
  listCrashAnswers: vi.fn(async () => ({ answers: [], latestVerdict: null, retainedCrashes: 40 })),
  getCrashAnswer: vi.fn(),
  analyzeCrashText: vi.fn(),
  analyzeCrashFile: vi.fn(),
  revealCrashArtifact: vi.fn(),
}));

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    getGameInstance: vi.fn(),
    checkGameInstance: vi.fn(),
    stopGameInstance: vi.fn(),
    renameGameInstance: vi.fn(),
    cloneGameInstance: vi.fn(),
    openGameInstanceFolder: vi.fn(),
    listDeletedGameInstances: vi.fn(),
    restoreDeletedGameInstance: vi.fn(),
    purgeDeletedGameInstances: vi.fn(),
    onInstanceExited: vi.fn(() => Promise.resolve(() => undefined)),
  };
});

const ALL: InstanceCapabilities = {
  launch: true,
  stop: true,
  rename: true,
  clone: true,
  trash: true,
  openFolder: true,
  playTime: true,
};

function detail(extra: Partial<InstanceDetail> = {}): InstanceDetail {
  return {
    instanceId: 'grounded-1',
    gameId: 'skyrim-se',
    gameName: 'Skyrim Special Edition',
    name: 'Grounded',
    pinned: true,
    store: 'steam',
    version: '1.6.1170',
    build: null,
    baseId: 'skyrim-se_steam_1.6.1170',
    installId: null,
    unpinnedReason: null,
    deployment: null,
    saves: 'shared',
    createdAt: '2026-10-01 10:00:00',
    lastLaunchedAt: '2026-10-30 18:30:00',
    lastPlayedAt: '2026-10-30 20:30:00',
    playTimeMs: 26_400_000,
    running: false,
    capabilities: ALL,
    ...extra,
  };
}

function renderPage() {
  return render(
    <ConfirmProvider>
      <GameInstancePage gameId="skyrim-se" instanceId="grounded-1" page="overview" onNavigatePage={() => undefined} onBack={() => undefined} />
    </ConfirmProvider>,
  );
}

beforeEach(() => {
  clearInstanceSessionCache();
  vi.mocked(getGameInstance).mockReset();
  vi.mocked(checkGameInstance).mockResolvedValue({ instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: [] });
  for (const mock of [stopGameInstance, renameGameInstance, cloneGameInstance, openGameInstanceFolder, listDeletedGameInstances, restoreDeletedGameInstance, purgeDeletedGameInstances]) {
    vi.mocked(mock).mockReset();
  }
});

describe('the instance header', () => {
  it('offers Rename, Open in Folder and play time when the instance allows them', async () => {
    vi.mocked(getGameInstance).mockResolvedValue(detail());
    renderPage();
    const hero = await screen.findByTestId('instance-hero');
    expect(within(hero).getByRole('button', { name: 'Rename' })).toBeInTheDocument();
    expect(within(hero).getByRole('button', { name: 'Open in Folder' })).toBeInTheDocument();
    expect(within(hero).getByTestId('instance-facts')).toHaveTextContent('7.3 h played');
  });

  it('shows none of them when the instance does not allow them', async () => {
    vi.mocked(getGameInstance).mockResolvedValue(detail({
      capabilities: { ...ALL, rename: false, openFolder: false, playTime: false },
    }));
    renderPage();
    const hero = await screen.findByTestId('instance-hero');
    expect(within(hero).queryByRole('button', { name: 'Rename' })).toBeNull();
    expect(within(hero).queryByRole('button', { name: 'Open in Folder' })).toBeNull();
    expect(hero).not.toHaveTextContent('h played');
  });

  it('renames through the prompt and shows the new name', async () => {
    vi.mocked(getGameInstance).mockResolvedValueOnce(detail()).mockResolvedValue(detail({ name: 'Grounded II' }));
    vi.mocked(renameGameInstance).mockResolvedValue({ instanceId: 'grounded-1', name: 'Grounded II' });
    renderPage();
    const hero = await screen.findByTestId('instance-hero');
    fireEvent.click(within(hero).getByRole('button', { name: 'Rename' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.change(within(dialog).getByRole('textbox'), { target: { value: 'Grounded II' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Rename' }));
    await waitFor(() => expect(renameGameInstance).toHaveBeenCalledWith('grounded-1', 'Grounded II'));
    expect(await within(hero).findByRole('heading', { name: /Grounded II/ })).toBeInTheDocument();
  });

  it('asks before Stop, and does nothing when the player cancels', async () => {
    vi.mocked(getGameInstance).mockResolvedValue(detail({ running: true }));
    renderPage();
    const hero = await screen.findByTestId('instance-hero');
    expect(within(hero).queryByRole('button', { name: /Launch/ })).toBeNull();
    fireEvent.click(within(hero).getByRole('button', { name: 'Stop Grounded' }));
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveTextContent('Anything you have not saved in the game is lost.');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(stopGameInstance).not.toHaveBeenCalled();
  });

  it('stops after the player agrees, then shows the instance as closed', async () => {
    vi.mocked(getGameInstance).mockResolvedValueOnce(detail({ running: true })).mockResolvedValue(detail());
    vi.mocked(stopGameInstance).mockResolvedValue({ instanceId: 'grounded-1', processesEnded: 1, playTimeMs: 26_400_000 });
    renderPage();
    const hero = await screen.findByTestId('instance-hero');
    fireEvent.click(within(hero).getByRole('button', { name: 'Stop Grounded' }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Stop the game' }));
    await waitFor(() => expect(stopGameInstance).toHaveBeenCalledWith('grounded-1'));
    expect(await within(hero).findByRole('button', { name: 'Launch Grounded' })).toBeInTheDocument();
    expect(hero).toHaveTextContent('Grounded was stopped.');
  });

  it('has no Stop for a game Agora cannot stop, and says why a refused Stop failed', async () => {
    vi.mocked(getGameInstance).mockResolvedValue(detail({ running: true, capabilities: { ...ALL, stop: false } }));
    renderPage();
    const hero = await screen.findByTestId('instance-hero');
    expect(within(hero).queryByRole('button', { name: /Stop/ })).toBeNull();
    expect(hero).toHaveTextContent('Running now');
  });
});

describe('Clone', () => {
  it('leaves the saves behind unless the player turns them on', async () => {
    vi.mocked(cloneGameInstance).mockResolvedValue({ instanceId: 'grounded-1-copy', name: 'Copy of Grounded' });
    const onCloned = vi.fn();
    render(<GameCloneDialog instanceId="grounded-1" name="Grounded" onClose={() => undefined} onCloned={onCloned} />);
    const saves = screen.getByRole('switch', { name: 'Also copy saves' });
    expect(saves).toHaveAttribute('aria-checked', 'false');
    fireEvent.click(screen.getByRole('button', { name: 'Clone' }));
    await waitFor(() => expect(cloneGameInstance).toHaveBeenCalledWith('grounded-1', 'Copy of Grounded', false));
    expect(onCloned).toHaveBeenCalled();
  });

  it('copies the saves when asked, and keeps the dialog open on a refusal', async () => {
    vi.mocked(cloneGameInstance).mockRejectedValue({ code: 'instance_running', message: 'Close the game first.' });
    render(<GameCloneDialog instanceId="grounded-1" name="Grounded" onClose={() => undefined} onCloned={() => undefined} />);
    fireEvent.click(screen.getByRole('switch', { name: 'Also copy saves' }));
    fireEvent.click(screen.getByRole('button', { name: 'Clone' }));
    await waitFor(() => expect(cloneGameInstance).toHaveBeenCalledWith('grounded-1', 'Copy of Grounded', true));
    expect(await screen.findByText('Close the game first.')).toBeInTheDocument();
  });
});

describe('Recently deleted', () => {
  const entry: DeletedEntry = {
    entryId: 'entry-1',
    instanceId: 'grounded-1',
    gameId: 'skyrim-se',
    name: 'Grounded',
    deletedAt: '2026-11-01T10:00:00Z',
    expiresAt: '2026-12-01T10:00:00Z',
    sizeBytes: 3 * 1024 ** 2,
  };

  function renderList(onRestored = vi.fn()) {
    render(
      <ConfirmProvider>
        <GameRecentlyDeleted gameId="skyrim-se" refreshKey={0} onRestored={onRestored} />
      </ConfirmProvider>,
    );
    return onRestored;
  }

  it('shows nothing while the list is empty', async () => {
    vi.mocked(listDeletedGameInstances).mockResolvedValue({ entries: [], retentionDays: 30 });
    renderList();
    await waitFor(() => expect(listDeletedGameInstances).toHaveBeenCalledWith('skyrim-se'));
    expect(screen.queryByRole('region', { name: 'Recently deleted' })).toBeNull();
  });

  it('says how long entries stay, from the player\'s setting', async () => {
    vi.mocked(listDeletedGameInstances).mockResolvedValue({ entries: [entry], retentionDays: 14 });
    renderList();
    const region = await screen.findByRole('region', { name: 'Recently deleted' });
    expect(region).toHaveTextContent('stay here for 14 days');
    expect(region).toHaveTextContent('Grounded');
  });

  it('asks for a new name when the deleted instance\'s place is taken', async () => {
    vi.mocked(listDeletedGameInstances).mockResolvedValueOnce({ entries: [entry], retentionDays: 30 }).mockResolvedValue({ entries: [], retentionDays: 30 });
    vi.mocked(restoreDeletedGameInstance)
      .mockRejectedValueOnce({ code: 'instance_id_taken', message: 'taken' })
      .mockResolvedValueOnce({ instanceId: 'grounded-restored', originalInstanceId: 'grounded-1', renamed: true });
    const onRestored = renderList();
    const region = await screen.findByRole('region', { name: 'Recently deleted' });
    fireEvent.click(within(region).getByRole('button', { name: 'Restore' }));
    const dialog = await screen.findByRole('dialog');
    expect(within(dialog).getByRole('textbox')).toHaveValue('Grounded (restored)');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Restore' }));
    await waitFor(() => expect(restoreDeletedGameInstance).toHaveBeenLastCalledWith('entry-1', 'Grounded (restored)'));
    await waitFor(() => expect(onRestored).toHaveBeenCalled());
    expect(restoreDeletedGameInstance).toHaveBeenNthCalledWith(1, 'entry-1');
  });

  it('empties only this game\'s list, after asking', async () => {
    vi.mocked(listDeletedGameInstances).mockResolvedValueOnce({ entries: [entry], retentionDays: 30 }).mockResolvedValue({ entries: [], retentionDays: 30 });
    vi.mocked(purgeDeletedGameInstances).mockResolvedValue({ purged: 1, failed: 0, kept: 0 });
    renderList();
    const region = await screen.findByRole('region', { name: 'Recently deleted' });
    fireEvent.click(within(region).getByRole('button', { name: 'Empty' }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Empty' }));
    await waitFor(() => expect(purgeDeletedGameInstances).toHaveBeenCalledWith('skyrim-se'));
    await waitFor(() => expect(screen.queryByRole('region', { name: 'Recently deleted' })).toBeNull());
  });

  it('deletes one entry now, after asking, and says when it could not', async () => {
    vi.mocked(listDeletedGameInstances).mockResolvedValue({ entries: [entry], retentionDays: 30 });
    vi.mocked(purgeDeletedGameInstances).mockResolvedValue({ purged: 0, failed: 1, kept: 0 });
    renderList();
    const region = await screen.findByRole('region', { name: 'Recently deleted' });
    fireEvent.click(within(region).getByRole('button', { name: 'Delete now' }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Delete now' }));
    await waitFor(() => expect(purgeDeletedGameInstances).toHaveBeenCalledWith('skyrim-se', 'entry-1'));
    expect(await within(region).findByRole('alert')).toHaveTextContent('could not be moved to the Recycle Bin');
  });
});
