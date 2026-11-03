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
      store: 'steam',
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

const skyrimBases = {
  bases: [{
    baseId: 'skyrim-se-1',
    gameId: 'skyrim-se',
    store: 'steam',
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
    usedBy: [{ instanceId: 'sk-1', name: 'Grounded' }],
  }],
  damaged: [],
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
const railLabels = () => within(sections()).getAllByRole('tab').map((tab) => tab.getAttribute('aria-label'));

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
    expect(railLabels()).toEqual(['General', 'Appearance', 'Accounts', 'Launching', 'Services']);

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
    expect(screen.getByTestId('settings-game-chip')).toHaveTextContent('For Skyrim SE');
    expect(onSwitchToMinecraft).not.toHaveBeenCalled();
  });

  it("opens the game section's deep link as General while Minecraft is selected", async () => {
    const { requestSettingsSection } = await import('../lib/settingsSection');
    requestSettingsSection({ tab: 'game', page: 'bases' });
    renderSettings('minecraft');

    expect(await screen.findByRole('heading', { name: 'Advanced mode' })).toBeInTheDocument();
    expect(within(sections()).getByRole('tab', { name: 'General' })).toHaveAttribute('aria-selected', 'true');
    expect(screen.queryByTestId('settings-game-chip')).toBeNull();
  });

  it('shows today\'s page while the active game has not loaded', async () => {
    renderSettings(undefined);

    expect(await screen.findByRole('heading', { name: 'Advanced mode' })).toBeInTheDocument();
    expect(within(sections()).getByRole('tab', { name: 'Launching' })).toBeInTheDocument();
    expect(screen.queryByTestId('settings-game-chip')).toBeNull();
  });
});

describe('Settings for another game', () => {
  it('puts the game in the rail where Launching sits and hides each Minecraft-only card', async () => {
    answer({ games_list: { games: [skyrimListing] }, games_base_list: skyrimBases });
    const onSwitchToMinecraft = vi.fn();
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition', onSwitchToMinecraft });

    expect(await screen.findByRole('heading', { name: 'Advanced mode' })).toBeInTheDocument();
    expect(railLabels()).toEqual(['General', 'Appearance', 'Accounts', 'Skyrim SE', 'Services']);
    expect(screen.getByTestId('settings-game-chip')).toHaveTextContent('For Skyrim SE');
    expect(screen.queryByRole('heading', { name: 'Launch Mode' })).toBeNull();
    expect(screen.queryByRole('heading', { name: 'Java Runtime Management' })).toBeNull();
    // Nothing of the game's is stacked above the normal layout any more.
    expect(screen.queryByRole('heading', { name: 'New instances' })).toBeNull();
    expect(screen.queryByRole('heading', { name: 'Install folders' })).toBeNull();

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

  it("gives the game's installs, bases and recovery a page each, with their full controls", async () => {
    answer({
      games_list: { games: [skyrimListing] },
      games_discover: { folders: [], warnings: [] },
      games_base_list: skyrimBases,
      games_user_files_status: { sessions: [] },
      games_support_list: [{ gameId: 'skyrim-se', name: 'Skyrim Special Edition', source: { kind: 'builtIn' }, enabled: true, canDisable: true }],
    });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Skyrim SE' }));
    expect(screen.getByText('Your choices for Skyrim Special Edition.')).toBeInTheDocument();
    expect(within(pages()).getAllByRole('tab').map((tab) => tab.textContent)).toEqual(['New instances', 'Installs', 'Bases', 'Recovery']);

    fireEvent.click(within(pages()).getByRole('tab', { name: 'Installs' }));
    expect(await screen.findByText('D:/Steam/steamapps/common/Skyrim Special Edition')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Rescan' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add a game folder…' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Game support' })).toBeInTheDocument();

    fireEvent.click(within(pages()).getByRole('tab', { name: 'Bases' }));
    expect(await screen.findByText((_, element) => element?.tagName === 'P'
      && /^1 base for Skyrim SE, using 2\.0 KB of their own\.$/.test(element.textContent ?? ''))).toBeInTheDocument();
    expect(screen.getByText('E:/AgoraBases/skyrim-se-1')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Build a base' })).toBeInTheDocument();

    fireEvent.click(within(pages()).getByRole('tab', { name: 'Recovery' }));
    expect(await screen.findByTestId('recovery-empty')).toBeInTheDocument();
  });

  it('opens a deep link to the game section for whichever game is selected', async () => {
    const { requestSettingsSection } = await import('../lib/settingsSection');
    requestSettingsSection({ tab: 'game', page: 'bases' });
    answer({ games_list: { games: [] } });
    renderSettings('valheim', { activeGameName: 'Valheim' });

    expect(within(sections()).getByRole('tab', { name: 'Valheim' })).toHaveAttribute('aria-selected', 'true');
    expect(within(pages()).getByRole('tab', { name: 'Bases' })).toHaveAttribute('aria-selected', 'true');
    expect(await screen.findByRole('heading', { name: 'Bases' })).toBeInTheDocument();
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
    expect(within(sections()).queryByRole('tab', { name: 'Skyrim SE' })).toBeNull();
  });

  it('treats an unknown plugin game id like any other non-Minecraft game', async () => {
    answer({ games_list: { games: [] } });
    renderSettings('valheim');

    expect(await screen.findByTestId('settings-game-chip')).toHaveTextContent('For Valheim');
    expect(within(sections()).queryByRole('tab', { name: 'Launching' })).toBeNull();
    expect(within(sections()).getByRole('tab', { name: 'Valheim' })).toBeInTheDocument();
  });

  it('says plainly when the game has no found install and no bases', async () => {
    answer({ games_list: { games: [] }, games_discover: { folders: [], warnings: [] }, games_base_list: { bases: [], damaged: [] } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Skyrim SE' }));
    fireEvent.click(within(pages()).getByRole('tab', { name: 'Installs' }));
    expect(await screen.findByText('Agora did not find this game on this computer.')).toBeInTheDocument();
    fireEvent.click(within(pages()).getByRole('tab', { name: 'Bases' }));
    expect(await screen.findByText('No pinned bases yet.')).toBeInTheDocument();
  });

  it("shows this game's saved defaults, marks the recommended choice apart from them, and saves under the game id", async () => {
    answer({ games_list: { games: [skyrimListing] }, get_setting: (args?: unknown) => {
      const key = String((args as { key?: unknown } | undefined)?.key ?? '');
      return key === 'game.skyrim-se.baseType' ? 'copied' : null;
    } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });
    fireEvent.click(within(sections()).getByRole('tab', { name: 'Skyrim SE' }));

    const base = await screen.findByLabelText('Base type');
    expect(base).toHaveValue('copied');
    // The saved choice is selected; the recommendation stays on Linked.
    expect(within(base).getByRole('option', { name: 'Linked base (recommended)' })).toBeInTheDocument();
    expect(within(base).getByRole('option', { name: 'Copied base' })).toBeInTheDocument();
    expect(screen.getByText('Agora recommends Linked base.')).toBeInTheDocument();
    const deployment = screen.getByLabelText('How new instances run');
    expect(deployment).toHaveValue('auto');
    expect(within(deployment).getByRole('option', { name: 'Automatic (recommended)' })).toBeInTheDocument();
    expect(screen.getByLabelText('Saves when importing a profile')).toHaveValue('off');
    // Only the setting that differs from the advice says so.
    expect(screen.getAllByText(/^Agora recommends/)).toHaveLength(1);

    fireEvent.change(deployment, { target: { value: 'links' } });
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('set_setting', {
      key: 'game.skyrim-se.deployment',
      value: 'links',
    }));
  });

  it("keeps Skyrim's saved default away from another game", async () => {
    answer({ games_list: { games: [skyrimListing] }, get_setting: (args?: unknown) => {
      const key = String((args as { key?: unknown } | undefined)?.key ?? '');
      return key === 'game.skyrim-se.baseType' ? 'copied' : null;
    } });
    const { rerender } = renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });
    fireEvent.click(within(sections()).getByRole('tab', { name: 'Skyrim SE' }));
    expect(await screen.findByLabelText('Base type')).toHaveValue('copied');

    rerender(
      <Settings
        onResetLayout={() => undefined}
        activeGame="valheim"
        activeGameName="Valheim"
      />,
    );

    expect(within(sections()).getByRole('tab', { name: 'Valheim' })).toHaveAttribute('aria-selected', 'true');
    await waitFor(() => expect(screen.getByLabelText('Base type')).toHaveValue('linked'));
  });

  it('opens the Game support page from the game section', async () => {
    answer({ games_list: { games: [] }, games_discover: { folders: [], warnings: [] } });
    renderSettings('skyrim-se', { activeGameName: 'Skyrim Special Edition' });

    fireEvent.click(within(sections()).getByRole('tab', { name: 'Skyrim SE' }));
    fireEvent.click(within(pages()).getByRole('tab', { name: 'Installs' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Game support' }));
    await waitFor(() =>
      expect(within(sections()).getByRole('tab', { name: 'General' })).toHaveAttribute('aria-selected', 'true'));
    expect(within(pages()).getByRole('tab', { name: 'Game support' })).toHaveAttribute('aria-selected', 'true');
  });
});
