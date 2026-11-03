/**
 * The games instance card and the launch it shares with the instance page: Launch on the card,
 * Edit opening the page, Clone and Stop when the instance allows them (never a dead button), play
 * time beside last played, and an answer that needs a decision sending the player to the page
 * with "Review".
 */
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { renderHook } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { GameInstanceCard } from './GameInstanceCard';
import { launchGameInstance, type InstanceCard, type LaunchReply } from './api';
import {
  clearGameLaunches,
  dismissGameLaunch,
  gameLaunchState,
  startGameLaunch,
  useGameLaunch,
} from './launchController';

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return { ...actual, launchGameInstance: vi.fn() };
});

const grounded: InstanceCard = {
  instanceId: 'grounded-1',
  gameId: 'skyrim-se',
  name: 'Grounded',
  store: 'steam',
  version: '1.6.1170',
  pinned: true,
  lastPlayedAt: null, playTimeMs: 0, capabilities: { launch: true, stop: true, rename: true, clone: true, trash: true, openFolder: true, playTime: true },
  deployment: null,
  lastLaunchedAt: '2026-10-30 18:30:00',
  createdAt: '2026-10-01 10:00:00',
  running: false,
};

const reply = (status: LaunchReply['status'], extra: Partial<LaunchReply> = {}): LaunchReply => ({
  status,
  pid: status === 'launched' ? 4242 : null,
  deployment: null,
  notice: null,
  findings: [],
  reason: null,
  next: null,
  ...extra,
});

type Handlers = { onOpen: () => void; onDelete: () => void; onClone: () => void; onStop: () => void; onLaunchSettled: () => void };

function renderCard(card: InstanceCard = grounded, handlers: Partial<Handlers> = {}) {
  const props = {
    onOpen: vi.fn(),
    onDelete: vi.fn(),
    onClone: vi.fn(),
    onStop: vi.fn(),
    onLaunchSettled: vi.fn(),
    ...handlers,
  };
  render(
    <ul>
      <GameInstanceCard card={card} deleting={false} locked={false} {...props} />
    </ul>,
  );
  return { ...props, card: screen.getByTestId('instance-card') };
}

beforeEach(() => {
  clearGameLaunches();
  vi.mocked(launchGameInstance).mockReset();
});

describe('GameInstanceCard', () => {
  it('offers Launch, Edit, Clone and Delete in Minecraft order, and nothing games cannot do', () => {
    const { card } = renderCard();
    const buttons = within(card).getAllByRole('button');
    expect(buttons.map((button) => button.textContent)).toEqual(['Launch', 'Edit', 'Clone', 'Delete']);
    for (const name of ['Open', 'Group', 'Stop', 'Troubleshoot']) {
      expect(within(card).queryByRole('button', { name })).toBeNull();
    }
    for (const button of buttons) expect(button).toBeEnabled();
  });

  it('leaves out Clone and Launch when the instance does not allow them', () => {
    const { card } = renderCard({
      ...grounded,
      capabilities: { ...grounded.capabilities, clone: false, launch: false },
    });
    expect(within(card).getAllByRole('button').map((button) => button.textContent)).toEqual(['Edit', 'Delete']);
  });

  it('opens Clone from the card', () => {
    const { card, onClone } = renderCard();
    fireEvent.click(within(card).getByRole('button', { name: 'Clone Grounded' }));
    expect(onClone).toHaveBeenCalledTimes(1);
  });

  it('shows play time next to last played, as Minecraft does', () => {
    const { card } = renderCard({ ...grounded, playTimeMs: 26_400_000, lastPlayedAt: '2026-10-31 20:00:00' });
    expect(card).toHaveTextContent(/Last played Oct 31, 2026.* · 7\.3 h played/);
  });

  it('leaves play time out when the instance does not track it', () => {
    const { card } = renderCard({
      ...grounded,
      playTimeMs: 26_400_000,
      capabilities: { ...grounded.capabilities, playTime: false },
    });
    expect(card).not.toHaveTextContent('played ·');
    expect(card).not.toHaveTextContent('h played');
  });

  it('shows the runtime chip, when it was played and when it was made, without seconds', () => {
    const { card } = renderCard();
    expect(card).toHaveTextContent('Steam 1.6.1170');
    expect(card).toHaveTextContent('Pinned base');
    expect(card).toHaveTextContent(/Last played .*2026/);
    expect(card).not.toHaveTextContent(/:00 [AP]M|:\d\d:\d\d/);
    expect(card).toHaveTextContent(/Created Oct 1, 2026|Created .*2026/);
  });

  it('leaves the created line out for a card from a backend that does not send it', () => {
    const { createdAt: _unused, ...older } = grounded;
    const { card } = renderCard(older);
    expect(card).not.toHaveTextContent('Created');
  });

  it('opens the instance from Edit', () => {
    const { card, onOpen } = renderCard();
    fireEvent.click(within(card).getByRole('button', { name: 'Edit Grounded' }));
    expect(onOpen).toHaveBeenCalledTimes(1);
  });

  it('shows Stop in place of Launch while the game runs, and says it is running', () => {
    const { card, onStop } = renderCard({ ...grounded, running: true });
    expect(within(card).queryByRole('button', { name: /Launch/ })).toBeNull();
    expect(card).toHaveTextContent('Running now');
    expect(card).toHaveClass('instance-card--running');
    expect(within(card).getByRole('button', { name: 'Delete Grounded' })).toBeDisabled();
    expect(within(card).getAllByRole('button')[0]).toHaveTextContent('Stop');
    fireEvent.click(within(card).getByRole('button', { name: 'Stop Grounded' }));
    expect(onStop).toHaveBeenCalledTimes(1);
  });

  it('has no Stop for a running game Agora cannot stop', () => {
    const { card } = renderCard({ ...grounded, running: true, capabilities: { ...grounded.capabilities, stop: false } });
    expect(within(card).queryByRole('button', { name: /Stop/ })).toBeNull();
    expect(within(card).queryByRole('button', { name: /Launch/ })).toBeNull();
    expect(card).toHaveTextContent('Running now');
  });

  it('launches from the card, then lets the list reload; a plain start leaves no banner', async () => {
    vi.mocked(launchGameInstance).mockResolvedValue(reply('launched'));
    const { card, onLaunchSettled } = renderCard();
    fireEvent.click(within(card).getByRole('button', { name: 'Launch Grounded' }));
    await waitFor(() => expect(onLaunchSettled).toHaveBeenCalledTimes(1));
    expect(launchGameInstance).toHaveBeenCalledWith('grounded-1', { launchAnyway: false, deployment: undefined });
    expect(within(card).queryByRole('status')).toBeNull();
  });

  it('sends a refused launch to the instance page with Review', async () => {
    vi.mocked(launchGameInstance).mockResolvedValue(reply('refused'));
    const { card, onOpen } = renderCard();
    fireEvent.click(within(card).getByRole('button', { name: 'Launch Grounded' }));
    const banner = await within(card).findByTestId('card-launch-refused');
    expect(banner).toHaveTextContent('Nothing was started.');
    fireEvent.click(within(banner).getByRole('button', { name: 'Review Grounded' }));
    expect(onOpen).toHaveBeenCalled();
  });

  it('asks the virtual file system question on the page, not on the card', async () => {
    vi.mocked(launchGameInstance).mockResolvedValue(reply('vfs_unavailable', { reason: 'no driver', next: 'links' }));
    const { card } = renderCard();
    fireEvent.click(within(card).getByRole('button', { name: 'Launch Grounded' }));
    const banner = await within(card).findByTestId('card-launch-question');
    expect(within(banner).getByRole('button', { name: 'Review Grounded' })).toBeInTheDocument();
    expect(within(card).queryByRole('button', { name: /Run once/ })).toBeNull();
  });

  it('says why a launch failed outright, and the message can be dismissed', async () => {
    vi.mocked(launchGameInstance).mockRejectedValue(new Error('the base is missing'));
    const { card } = renderCard();
    fireEvent.click(within(card).getByRole('button', { name: 'Launch Grounded' }));
    const alert = await within(card).findByRole('alert');
    expect(alert).toHaveTextContent('The game did not start. the base is missing');
    fireEvent.click(within(alert).getByRole('button', { name: 'Dismiss' }));
    expect(within(card).queryByRole('alert')).toBeNull();
  });
});

describe('launchController', () => {
  it('shares one instance\'s launch between every view of it', async () => {
    let answer: (value: LaunchReply) => void = () => undefined;
    vi.mocked(launchGameInstance).mockReturnValue(new Promise((resolve) => { answer = resolve; }));
    const first = renderHook(() => useGameLaunch('grounded-1'));
    const second = renderHook(() => useGameLaunch('grounded-1'));
    const other = renderHook(() => useGameLaunch('modded-2'));

    let started: Promise<LaunchReply | null> = Promise.resolve(null);
    act(() => { started = startGameLaunch('grounded-1'); });
    expect(first.result.current.busy).toBe(true);
    expect(second.result.current.busy).toBe(true);
    expect(other.result.current.busy).toBe(false);

    await act(async () => {
      answer(reply('refused'));
      await started;
    });
    expect(second.result.current).toMatchObject({ busy: false, reply: { status: 'refused' } });
  });

  it('does not start a second launch of an instance that is already starting', async () => {
    vi.mocked(launchGameInstance).mockReturnValue(new Promise(() => undefined));
    void startGameLaunch('grounded-1');
    expect(await startGameLaunch('grounded-1')).toBeNull();
    expect(launchGameInstance).toHaveBeenCalledTimes(1);
  });

  it('passes launch-anyway and a one-off deployment through', async () => {
    vi.mocked(launchGameInstance).mockResolvedValue(reply('launched'));
    await startGameLaunch('grounded-1', { launchAnyway: true, deployment: 'links' });
    expect(launchGameInstance).toHaveBeenCalledWith('grounded-1', { launchAnyway: true, deployment: 'links' });
  });

  it('says plainly when the game is already running', async () => {
    vi.mocked(launchGameInstance).mockRejectedValue({ code: 'instance_running', message: 'running' });
    await startGameLaunch('grounded-1');
    expect(gameLaunchState('grounded-1').error).toBe('The game is already running from this instance.');
    dismissGameLaunch('grounded-1');
    expect(gameLaunchState('grounded-1')).toMatchObject({ busy: false, reply: null, error: null });
  });
});
