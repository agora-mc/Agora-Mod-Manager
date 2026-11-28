/**
 * "Set image" on a game instance's page: the picked image shows in the header and on every tile
 * of the same instance at once, a cancelled picker changes nothing, and a refused file says why.
 */
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { GameInstancePage } from '../shell/GameInstancePage';
import { clearInstanceSessionCache } from './instanceSession';
import { getGameInstance, checkGameInstance, getGameInstanceIcon, setGameInstanceIcon } from './api';
import { GameInstanceTile } from './instanceIdentity';
import { resetGameInstanceIconsForTests } from './instanceIcons';

const pickOpenFile = vi.hoisted(() => vi.fn());
vi.mock('@/lib/tauri', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/tauri')>()),
  pickOpenFile,
}));
vi.mock('../content', () => ({ InstanceContentTab: () => null }));
vi.mock('../plugins', () => ({ InstancePluginsTab: () => null }));
vi.mock('../iniSaves', () => ({ InstanceIniSavesTab: () => null }));
vi.mock('../tools', () => ({ InstanceToolsTab: () => null }));
vi.mock('./InstanceOverview', () => ({ InstanceOverviewTab: () => null }));
vi.mock('./api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./api')>()),
  getGameInstance: vi.fn(),
  checkGameInstance: vi.fn(),
  getGameInstanceIcon: vi.fn(),
  setGameInstanceIcon: vi.fn(),
  onInstanceExited: vi.fn(() => Promise.resolve(() => undefined)),
}));

const IMAGE = 'data:image/png;base64,AAAA';

function renderPage() {
  return render(
    <>
      <GameInstancePage gameId="skyrim-se" instanceId="grounded-1" page="overview" onNavigatePage={() => undefined} onBack={() => undefined} />
      <div data-testid="other-tile"><GameInstanceTile instanceId="grounded-1" name="Grounded" /></div>
    </>,
  );
}

beforeEach(() => {
  clearInstanceSessionCache();
  resetGameInstanceIconsForTests();
  vi.mocked(getGameInstance).mockResolvedValue({
    instanceId: 'grounded-1', gameId: 'skyrim-se', gameName: 'Skyrim Special Edition', name: 'Grounded',
    lastPlayedAt: null, playTimeMs: 0, capabilities: { launch: true, stop: true, rename: true, clone: true, trash: true, openFolder: true, playTime: true }, pinned: true, store: 'steam', version: '1.6.1170', build: null, baseId: null, installId: null,
    unpinnedReason: null, deployment: null, saves: 'shared', createdAt: '2026-10-01 10:00:00',
    lastLaunchedAt: null, running: false,
  });
  vi.mocked(checkGameInstance).mockResolvedValue({ instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: [] });
  vi.mocked(getGameInstanceIcon).mockReset().mockResolvedValue(null);
  vi.mocked(setGameInstanceIcon).mockReset();
  pickOpenFile.mockReset();
});

describe('Set image on a game instance', () => {
  it('shows the generated tile until an image is set, then the image everywhere', async () => {
    pickOpenFile.mockResolvedValue('D:/art/grounded.png');
    vi.mocked(setGameInstanceIcon).mockResolvedValue(IMAGE);
    const { container } = renderPage();

    fireEvent.click(await screen.findByRole('button', { name: 'Set instance image' }));

    await waitFor(() => expect(container.querySelectorAll(`img[src="${IMAGE}"]`)).toHaveLength(2));
    expect(setGameInstanceIcon).toHaveBeenCalledWith('grounded-1', 'D:/art/grounded.png');
    expect(screen.getByRole('button', { name: 'Change instance image' })).toBeInTheDocument();
    expect(pickOpenFile).toHaveBeenCalledWith('Choose instance image', ['png', 'jpg', 'jpeg', 'webp', 'gif']);
  });

  it('reads a stored image for the header and the card', async () => {
    vi.mocked(getGameInstanceIcon).mockResolvedValue(IMAGE);
    const { container } = renderPage();
    await waitFor(() => expect(container.querySelectorAll(`img[src="${IMAGE}"]`)).toHaveLength(2));
    // One read serves both tiles.
    expect(getGameInstanceIcon).toHaveBeenCalledTimes(1);
  });

  it('changes nothing when the picker is cancelled', async () => {
    pickOpenFile.mockResolvedValue(null);
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'Set image' }));
    await waitFor(() => expect(pickOpenFile).toHaveBeenCalled());
    expect(setGameInstanceIcon).not.toHaveBeenCalled();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('says why a refused file was not used', async () => {
    pickOpenFile.mockResolvedValue('D:/art/fake.png');
    vi.mocked(setGameInstanceIcon).mockRejectedValue({ code: 'core_failed', message: 'The selected file is not a valid image of its stated type.' });
    renderPage();
    fireEvent.click(await screen.findByRole('button', { name: 'Set image' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('The image was not set. The selected file is not a valid image of its stated type.');
  });
});
