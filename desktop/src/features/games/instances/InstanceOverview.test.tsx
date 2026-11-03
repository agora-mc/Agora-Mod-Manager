/**
 * The Overview's two choices that must not go wrong silently: a "Launch anyway" tick made for one
 * instance must not carry to another, and a deployment change that was saved must not read as
 * "not changed" because the page could not refresh afterwards. Also: reopening an instance shows its
 * last checks at once while they run again, and never another instance's.
 */
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { GameInstancePage } from '../shell/GameInstancePage';
import { clearInstanceSessionCache } from './instanceSession';
import {
  checkGameInstance,
  getGameInstance,
  setGameInstanceDeployment,
  type InstanceCheck,
  type InstanceDetail,
} from './api';

vi.mock('../content', () => ({ InstanceContentTab: () => null }));
vi.mock('../plugins', () => ({ InstancePluginsTab: () => null }));
vi.mock('../iniSaves', () => ({ InstanceIniSavesTab: () => <p>ini page</p> }));
vi.mock('../tools', () => ({ InstanceToolsTab: () => null }));

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    getGameInstance: vi.fn(),
    checkGameInstance: vi.fn(),
    setGameInstanceDeployment: vi.fn(),
    onInstanceExited: vi.fn(() => Promise.resolve(() => undefined)),
  };
});

function detail(instanceId: string): InstanceDetail {
  return {
    instanceId,
    gameId: 'skyrim-se',
    gameName: 'Skyrim Special Edition',
    name: instanceId === 'grounded-1' ? 'Grounded' : 'Modded',
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
    lastLaunchedAt: null,
    lastPlayedAt: null,
    playTimeMs: 0,
    running: false,
    capabilities: {
      launch: true,
      stop: true,
      rename: true,
      clone: true,
      trash: true,
      openFolder: true,
      playTime: true,
    },
  };
}

const cleanCheck: InstanceCheck = { instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: [] };

function InstanceOverviewTab({ instanceId, page = 'overview' }: { gameId: string; instanceId: string; page?: 'overview' | 'ini-saves' }) {
  return <GameInstancePage gameId="skyrim-se" instanceId={instanceId} page={page} onNavigatePage={() => undefined} onBack={() => undefined} />;
}

beforeEach(() => {
  clearInstanceSessionCache();
  vi.mocked(getGameInstance).mockReset();
  vi.mocked(checkGameInstance).mockReset().mockResolvedValue(cleanCheck);
  vi.mocked(setGameInstanceDeployment).mockReset();
});

describe('Launch anyway', () => {
  it('does not carry a tick from one instance over to the next', async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    const { rerender } = render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await screen.findByTestId('instance-overview');

    fireEvent.click(screen.getByRole('button', { name: 'Advanced options' }));
    fireEvent.click(screen.getByRole('checkbox', { name: /Launch anyway/ }));
    expect(screen.getByRole('checkbox', { name: /Launch anyway/ })).toBeChecked();

    rerender(<InstanceOverviewTab gameId="skyrim-se" instanceId="modded-2" />);
    await screen.findByRole('heading', { name: 'Modded' });
    fireEvent.click(screen.getByRole('button', { name: 'Advanced options' }));
    expect(screen.getByRole('checkbox', { name: /Launch anyway/ })).not.toBeChecked();
  });
});

describe('checks that passed', () => {
  it('lists each passed check and summarises the count', async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    vi.mocked(checkGameInstance).mockResolvedValue({
      instanceId: 'grounded-1',
      checked: true,
      status: 'ok',
      findings: [],
      passed: [
        { kind: 'framework', title: 'Skyrim Script Extender (SKSE)', detail: 'Found skse64_1_6_1170.dll, which matches this game version.' },
        { kind: 'load_order', title: 'Plugin load order', detail: '3 plugins in a valid order.' },
      ],
    });
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await screen.findByTestId('instance-overview');

    await waitFor(() => expect(screen.getByTestId('checks-headline')).toHaveTextContent('All 2 checks passed.'));
    const passed = screen.getAllByTestId('check-passed');
    expect(passed).toHaveLength(2);
    expect(passed[0]).toHaveTextContent('Skyrim Script Extender (SKSE)');
    expect(passed[1]).toHaveTextContent('3 plugins in a valid order.');
  });

  it('names the blocking checks in the Launch anyway option', async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    vi.mocked(checkGameInstance).mockResolvedValue({
      instanceId: 'grounded-1',
      checked: true,
      status: 'findings',
      findings: [
        { kind: 'framework', blocks: true, title: 'Skyrim Script Extender (SKSE)', detail: 'x', why: 'y', repair: null },
        { kind: 'load_order', blocks: true, title: 'Plugin load order', detail: 'x', why: 'y', repair: null },
      ],
      passed: [],
    });
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await screen.findByTestId('instance-overview');

    fireEvent.click(screen.getByRole('button', { name: 'Advanced options' }));
    expect(screen.getByRole('checkbox', { name: /Launch anyway/ }).closest('label')).toHaveTextContent(
      'start the game past Skyrim Script Extender (SKSE) and Plugin load order, even when they would stop it.',
    );
  });
});

describe('deployment change', () => {
  it('says the change was saved when only the refresh afterwards fails', async () => {
    let loads = 0;
    vi.mocked(getGameInstance).mockImplementation(async (id) => {
      loads += 1;
      if (loads > 1) throw new Error('the page could not refresh');
      return detail(id);
    });
    vi.mocked(setGameInstanceDeployment).mockResolvedValue({ instanceId: 'grounded-1', deployment: 'links' });
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await screen.findByTestId('instance-overview');

    fireEvent.click(screen.getByRole('button', { name: 'Linked files' }));

    await waitFor(() => expect(setGameInstanceDeployment).toHaveBeenCalledWith('grounded-1', 'links'));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('the page could not refresh');
    expect(alert).not.toHaveTextContent('was not changed');
  });

  it('says it was not changed when the save itself fails', async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    vi.mocked(setGameInstanceDeployment).mockRejectedValue(new Error('the save was refused'));
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await screen.findByTestId('instance-overview');

    fireEvent.click(screen.getByRole('button', { name: 'Linked files' }));

    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('The way this instance runs was not changed. the save was refused');
  });
});

describe('the last answer', () => {
  const blocking: InstanceCheck = {
    instanceId: 'grounded-1',
    checked: true,
    status: 'findings',
    findings: [{ kind: 'framework', blocks: true, title: 'Skyrim Script Extender (SKSE)', detail: 'x', why: 'y', repair: null }],
    passed: [],
  };

  it('shows the last checks at once when the instance is opened again, then the new ones', async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    vi.mocked(checkGameInstance).mockResolvedValueOnce(blocking);
    const first = render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await waitFor(() => expect(screen.getByTestId('checks-headline')).toHaveTextContent('1 problem stops the game from launching'));
    first.unmount();

    let answer: (check: InstanceCheck) => void = () => undefined;
    vi.mocked(checkGameInstance).mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);

    // Before anything answers: the old result, marked as being checked again.
    expect(screen.getByTestId('check-finding')).toHaveTextContent('Skyrim Script Extender (SKSE)');
    expect(screen.getByText(/Showing the last answer/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Launch Grounded' })).toBeInTheDocument();

    await waitFor(() => expect(checkGameInstance).toHaveBeenCalledTimes(2));
    answer(cleanCheck);
    await waitFor(() => expect(screen.getByTestId('checks-headline')).toHaveTextContent('No launch checks apply to this instance.'));
    expect(screen.queryByTestId('check-finding')).toBeNull();
    expect(screen.queryByText(/Showing the last answer/)).toBeNull();
  });

  it("never shows one instance its neighbour's last checks", async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    vi.mocked(checkGameInstance).mockResolvedValueOnce(blocking);
    const first = render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await waitFor(() => expect(screen.getByTestId('checks-headline')).toHaveTextContent('1 problem stops the game from launching'));
    first.unmount();

    vi.mocked(checkGameInstance).mockImplementationOnce(() => new Promise(() => undefined));
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="modded-2" />);
    expect(screen.getByRole('status')).toHaveTextContent('Loading this instance');
    await screen.findByRole('heading', { name: 'Modded' });
    expect(screen.queryByTestId('check-finding')).toBeNull();
    expect(screen.getByTestId('checks-headline')).toHaveTextContent('Checking this instance');
  });

  it('keeps the last answer and says so when reading the instance again fails', async () => {
    vi.mocked(getGameInstance).mockImplementationOnce(async (id) => detail(id));
    vi.mocked(checkGameInstance).mockResolvedValueOnce(blocking);
    const first = render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    await waitFor(() => expect(screen.getByTestId('checks-headline')).toHaveTextContent('1 problem stops the game from launching'));
    first.unmount();

    vi.mocked(getGameInstance).mockRejectedValueOnce(new Error('the database is locked'));
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" />);
    const alert = await screen.findByText(/could not be read again/);
    expect(alert).toHaveTextContent('the database is locked');
    expect(screen.getByTestId('check-finding')).toBeInTheDocument();
  });

  it('shows the header with Launch on the other pages too', async () => {
    vi.mocked(getGameInstance).mockImplementation(async (id) => detail(id));
    render(<InstanceOverviewTab gameId="skyrim-se" instanceId="grounded-1" page="ini-saves" />);
    expect(await screen.findByRole('button', { name: 'Launch Grounded' })).toBeEnabled();
    expect(screen.getByText('ini page')).toBeInTheDocument();
    expect(screen.queryByTestId('instance-overview')).toBeNull();
  });
});
