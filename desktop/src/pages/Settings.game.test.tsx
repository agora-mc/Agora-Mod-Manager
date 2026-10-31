import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useState, type ComponentProps } from 'react';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));
vi.mock('@tauri-apps/api/app', () => ({ getVersion: vi.fn(async () => '1.0.0') }));
vi.mock('@tauri-apps/plugin-updater', () => ({ check: vi.fn(async () => null) }));

import { Settings } from './Settings';
import { takePendingSettingsSection } from '../lib/settingsSection';

const skyrimListing = {
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  installs: [
    {
      installId: 'steam:489830',
      store: 'Steam',
      name: 'Skyrim Special Edition',
      location: 'D:/Steam/steamapps/common/Skyrim Special Edition',
      storeVersion: null,
      storeBuild: null,
      addOnCount: 0,
      runtime: { status: 'unidentified', reasons: [] },
      installBytes: null,
    },
  ],
};

function answer(replies: Record<string, unknown> = {}) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command in replies) {
      const reply = replies[command];
      if (reply instanceof Error) throw reply;
      return typeof reply === 'function' ? (reply as (a?: unknown) => unknown)(args) : reply;
    }
    if (command === 'games_support_list') return [];
    if (command === 'games_list') return { games: [] };
    if (command === 'games_base_list') return { bases: [], damaged: [] };
    if (command === 'list_instances') return [];
    if (command === 'msa_list_accounts') return [];
    if (command === 'list_java_runtimes') return [];
    return null;
  });
}

function renderSettings(activeGame: string | undefined, extra: Partial<ComponentProps<typeof Settings>> = {}) {
  return render(
    <Settings
      onResetLayout={() => undefined}
      activeGame={activeGame}
      {...extra}
    />,
  );
}

const sections = () => screen.getByRole('navigation', { name: 'Settings sections' });
const pages = () => screen.getByRole('tablist', { name: / pages$/ });

beforeEach(() => {
  invokeMock.mockReset();
  answer();
  window.localStorage.clear();
  takePendingSettingsSection();
});

describe('Settings for Minecraft', () => {
  it('still shows the Minecraft cards, including the Launching section', async () => {
    renderSettings('minecraft');

    expect(await screen.findByRole('heading', { name: 'Advanced mode' })).toBeInTheDocument();
    expect(within(sections()).getByRole('tab', { name: 'Launching' })).toBeInTheDocument();

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Launching' }));
    expect(await screen.findByRole('heading', { name: 'Launch Mode' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'When the game starts' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Launcher Path' })).toBeInTheDocument();

    fireEvent.click(within(pages()).getByRole('tab', { name: 'Java' }));
    expect(await screen.findByRole('heading', { name: 'Java Runtime Management' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'JVM Defaults' })).toBeInTheDocument();

    fireEvent.click(within(pages()).getByRole('tab', { name: 'Game window' }));
    expect(await screen.findByRole('heading', { name: 'Game window' })).toBeInTheDocument();

    fireEvent.click(within(sections()).getByRole('tab', { name: 'General' }));
    expect(within(pages()).getByRole('tab', { name: 'Walkthrough' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Templates' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Storage' })).toBeInTheDocument();

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Accounts' }));
    expect(await screen.findByRole('heading', { name: 'Microsoft Account' })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'GitHub Account' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Skin & cape' })).toBeInTheDocument();

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Services' }));
    expect(within(pages()).getByRole('tab', { name: 'Content sources' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'AI assistant' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Plugins' })).toBeInTheDocument();
  });

  it('opens a Minecraft-only deep link as General, without switching the game', async () => {
    const { requestSettingsSection } = await import('../lib/settingsSection');
    requestSettingsSection({ tab: 'launching', page: 'java' });
    const onSwitchToMinecraft = vi.fn();
    answer({ games_list: { games: [] } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition', onSwitchToMinecraft });

    expect(within(sections()).getByRole('tab', { name: 'General' })).toHaveAttribute('aria-selected', 'true');
    expect(within(sections()).queryByRole('tab', { name: 'Launching' })).toBeNull();
    expect(screen.getByText(/Showing settings for Skyrim Special Edition/)).toBeInTheDocument();
    expect(onSwitchToMinecraft).not.toHaveBeenCalled();
  });

  it('shows today\'s page while the active game has not loaded', async () => {
    renderSettings(undefined);

    expect(await screen.findByRole('heading', { name: 'Advanced mode' })).toBeInTheDocument();
    expect(within(sections()).getByRole('tab', { name: 'Launching' })).toBeInTheDocument();
    expect(screen.queryByText(/Minecraft settings appear when Minecraft is selected/)).toBeNull();
  });
});

describe('Settings for another game', () => {
  it('hides each Minecraft-only card and the emptied Launching section', async () => {
    answer({
      games_list: { games: [skyrimListing] },
      games_base_list: {
        bases: [{
          baseId: 'skyrim-se-1',
          gameId: 'skyrim-se',
          store: 'Steam',
          version: '1.6.1170',
          build: null,
          mode: 'copied',
          location: 'E:/AgoraBases/skyrim-se-1',
          present: true,
          fileCount: 3,
          linkedFileCount: 0,
          linkedBytes: 0,
          copiedBytes: 2048,
          skippedCount: 0,
          createdUnixMs: 1,
          usedBy: [],
        }],
        damaged: [],
      },
    });
    const onSwitchToMinecraft = vi.fn();
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition', onSwitchToMinecraft });

    expect(await screen.findByText(/Showing settings for Skyrim Special Edition\. Minecraft settings appear when Minecraft is selected\./)).toBeInTheDocument();
    expect(within(sections()).queryByRole('tab', { name: 'Launching' })).toBeNull();
    expect(screen.queryByRole('heading', { name: 'Launch Mode' })).toBeNull();
    expect(screen.queryByRole('heading', { name: 'Java Runtime Management' })).toBeNull();

    const gameSection = await screen.findByRole('heading', { name: 'Skyrim Special Edition' });
    expect(gameSection).toBeInTheDocument();
    expect(await screen.findByText(/D:\/Steam\/steamapps\/common\/Skyrim Special Edition/)).toBeInTheDocument();
    expect(await screen.findByText(/1 base for Skyrim Special Edition, using 2.0 KB/)).toBeInTheDocument();
    expect(screen.getByText(/E:\/AgoraBases\/skyrim-se-1/)).toBeInTheDocument();

    expect(within(pages()).getByRole('tab', { name: 'Walkthrough' })).toBeInTheDocument();
    expect(within(pages()).queryByRole('tab', { name: 'Templates' })).toBeNull();
    expect(within(pages()).getByRole('tab', { name: 'Storage' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Game support' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Updates' })).toBeInTheDocument();

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Accounts' }));
    expect(await screen.findByRole('heading', { name: 'GitHub Account' })).toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: 'Microsoft Account' })).toBeNull();
    expect(screen.queryByRole('tab', { name: 'Skin & cape' })).toBeNull();

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Services' }));
    expect(within(pages()).getByRole('tab', { name: 'Content sources' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'AI assistant' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Plugins' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Switch to Minecraft' }));
    expect(onSwitchToMinecraft).toHaveBeenCalledTimes(1);
  });

  it('keeps the app-wide Walkthrough, Storage and Content sources pages for another game', async () => {
    answer({ games_list: { games: [] } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });

    expect(await screen.findByRole('heading', { name: 'Advanced mode' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Walkthrough' })).toBeInTheDocument();
    expect(within(pages()).getByRole('tab', { name: 'Storage' })).toBeInTheDocument();
    fireEvent.click(within(sections()).getByRole('tab', { name: 'Services' }));
    expect(within(pages()).getByRole('tab', { name: 'Content sources' })).toBeInTheDocument();
  });

  it('stays on the same Settings tab when the game switches to Minecraft', async () => {
    answer({ games_list: { games: [] } });
    function Harness() {
      const [game, setGame] = useState<string>('skyrim-se');
      return (
        <Settings
          onResetLayout={() => undefined}
          activeGame={game}
          activeGameName="Skyrim Special Edition"
          onSwitchToMinecraft={() => setGame('minecraft')}
        />
      );
    }
    render(<Harness />);

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Accounts' }));
    expect(await screen.findByRole('heading', { name: 'GitHub Account' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Switch to Minecraft' }));
    expect(await screen.findByRole('heading', { name: 'GitHub Account' })).toBeInTheDocument();
    expect(within(sections()).getByRole('tab', { name: 'Accounts' })).toHaveAttribute('aria-selected', 'true');
    expect(within(sections()).getByRole('tab', { name: 'Launching' })).toBeInTheDocument();
  });

  it('treats an unknown plugin game id like any other non-Minecraft game', async () => {
    answer({ games_list: { games: [] } });
    renderSettings('valheim');

    expect(await screen.findByText(/Showing settings for Valheim\./)).toBeInTheDocument();
    expect(within(sections()).queryByRole('tab', { name: 'Launching' })).toBeNull();
    expect(await screen.findByRole('heading', { name: 'Valheim' })).toBeInTheDocument();
  });

  it('says plainly when the game has no found install and no bases', async () => {
    answer({ games_list: { games: [] }, games_base_list: { bases: [], damaged: [] } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });

    expect(await screen.findByText('Agora has not found an install of Skyrim Special Edition on this computer.')).toBeInTheDocument();
    expect(await screen.findByText('No bases built yet.')).toBeInTheDocument();
  });

  it('opens the Game support page from the game section', async () => {
    answer({ games_list: { games: [] } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });

    fireEvent.click(await screen.findByRole('button', { name: 'Game support toggles' }));
    await waitFor(() =>
      expect(within(sections()).getByRole('tab', { name: 'General' })).toHaveAttribute('aria-selected', 'true'));
    expect(within(pages()).getByRole('tab', { name: 'Game support' })).toHaveAttribute('aria-selected', 'true');
  });
});
