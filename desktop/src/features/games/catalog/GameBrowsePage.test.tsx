/**
 * A game's Browse page: the curated catalog as Minecraft's result cards, an empty catalog that
 * points to Settings > Plugins, search, the "Install into" choice, and "Install anyway" both as
 * the switch and as the answer to a file that changed since the Agora team checked it.
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
const openSettingsSection = vi.hoisted(() => vi.fn());
vi.mock('@/lib/settingsSection', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/settingsSection')>()),
  openSettingsSection,
}));

import { GameBrowsePage, INSTALL_ANYWAY_COPY } from './index';

const entry = (id: string, name: string, extra: Record<string, unknown> = {}) => ({
  id,
  name,
  author: 'ianpatt',
  description: `${name} does things.`,
  downloadStrategy: 'github_release',
  status: 'ok',
  pageUrl: null,
  compatibility: [{ stores: ['steam'], gameVersions: ['1.6.1170'], requires: [], asset: null }],
  ...extra,
});

const twoEntries = {
  entries: [
    entry('skse', 'SKSE64'),
    entry('ussep', 'Unofficial Patch', {
      compatibility: [{ stores: ['steam'], gameVersions: ['1.6.1170'], requires: [{ framework: 'skse', minVersion: '2.2' }], asset: null }],
    }),
  ],
  instances: [{ instanceId: 'inst-1', name: 'Grounded' }],
};

function listing(reply: unknown) {
  invokeMock.mockImplementation(async (command: string) => {
    if (command === 'games_catalog_list') return reply;
    return null;
  });
}

describe('GameBrowsePage', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    openSettingsSection.mockReset();
  });

  it('says there is nothing to browse yet and opens Settings > Plugins', async () => {
    listing({ entries: [], instances: [] });
    const openLibrary = vi.fn();
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" onOpenLibrary={openLibrary} />);

    const empty = await screen.findByTestId('catalog-empty');
    expect(empty).toHaveTextContent('Nothing to browse for Skyrim SE yet');
    // No catalog: no instance picker, no switch and no search box for an empty list.
    expect(screen.queryByTestId('catalog-target')).not.toBeInTheDocument();
    expect(screen.queryByRole('textbox', { name: 'Search mods' })).not.toBeInTheDocument();

    fireEvent.click(within(empty).getByRole('button', { name: 'Open Settings > Plugins' }));
    expect(openSettingsSection).toHaveBeenCalledWith({ tab: 'services', page: 'plugins' });
    fireEvent.click(within(empty).getByRole('button', { name: 'Your Library' }));
    expect(openLibrary).toHaveBeenCalled();
  });

  it('lists the curated items as result cards, with what each works with and needs', async () => {
    listing(twoEntries);
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);

    const cards = await screen.findAllByTestId('catalog-entry');
    expect(cards).toHaveLength(2);
    expect(cards[0]).toHaveTextContent('SKSE64');
    expect(cards[0]).toHaveTextContent('Curated');
    expect(cards[0]).toHaveTextContent('by ianpatt · Downloads from GitHub');
    expect(cards[0]).toHaveTextContent('Steam 1.6.1170');
    expect(cards[1]).toHaveTextContent('Needs skse 2.2 or newer');
    expect(screen.getByRole('heading', { name: 'Browse' })).toBeInTheDocument();
    expect(screen.getByText('Mods the Agora team have personally reviewed and selected for the catalog.')).toBeInTheDocument();
  });

  it('searches the list, and a search with no match can be cleared', async () => {
    listing(twoEntries);
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
    await screen.findAllByTestId('catalog-entry');

    const search = screen.getByRole('textbox', { name: 'Search mods' });
    fireEvent.change(search, { target: { value: 'unofficial' } });
    expect(screen.getAllByTestId('catalog-entry')).toHaveLength(1);
    expect(screen.getByTestId('catalog-entry')).toHaveTextContent('Unofficial Patch');

    fireEvent.change(search, { target: { value: 'nothing like this' } });
    expect(screen.queryByTestId('catalog-entry')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Clear search' }));
    expect(screen.getAllByTestId('catalog-entry')).toHaveLength(2);
  });

  it('asks for an instance first when the game has none, and cannot install', async () => {
    listing({ ...twoEntries, instances: [] });
    const openInstances = vi.fn();
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" onOpenInstances={openInstances} />);

    const note = await screen.findByTestId('catalog-no-instance');
    expect(note).toHaveTextContent('no Skyrim SE instance yet');
    fireEvent.click(within(note).getByRole('button', { name: 'Go to My Instances' }));
    expect(openInstances).toHaveBeenCalled();
    for (const button of screen.getAllByRole('button', { name: 'Install' })) expect(button).toBeDisabled();
    expect(screen.queryByRole('switch')).not.toBeInTheDocument();
  });

  it('sends the switch with Install, off unless turned on', async () => {
    listing(twoEntries);
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
    const [first] = await screen.findAllByTestId('catalog-entry');

    const toggle = screen.getByRole('switch', { name: INSTALL_ANYWAY_COPY.label });
    expect(toggle).not.toBeChecked();
    expect(toggle).toHaveAccessibleDescription(INSTALL_ANYWAY_COPY.description);

    fireEvent.click(within(first).getByRole('button', { name: 'Install' }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('games_catalog_install', expect.anything()));
    const sent = () =>
      invokeMock.mock.calls.filter(([command]) => command === 'games_catalog_install').map(([, args]) => args.request);
    expect(sent()[0]).toMatchObject({ instanceId: 'inst-1', entryId: 'skse', installAnyway: false, dryRun: false });

    fireEvent.click(toggle);
    fireEvent.click(within(first).getByRole('button', { name: 'Preview' }));
    await waitFor(() => expect(sent()).toHaveLength(2));
    expect(sent()[1]).toMatchObject({ installAnyway: true, dryRun: true });
  });

  it('offers Install anyway on a file that changed since it was checked, and only then', async () => {
    invokeMock.mockImplementation(async (command: string, args: { request?: { installAnyway: boolean } }) => {
      if (command === 'games_catalog_list') return twoEntries;
      if (command === 'games_catalog_install') {
        if (args.request?.installAnyway) return null;
        throw { code: 'core_failed', message: 'The file differs from the curator’s pin.', coreCode: 'ERR_HASH_CONFIRMATION_REQUIRED' };
      }
      return null;
    });
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
    const [first] = await screen.findAllByTestId('catalog-entry');

    fireEvent.click(within(first).getByRole('button', { name: 'Install' }));
    const alert = await within(first).findByRole('alert');
    expect(alert).toHaveTextContent('Not installed: The file differs');
    fireEvent.click(within(alert).getByRole('button', { name: 'Install anyway' }));
    await waitFor(() => {
      const calls = invokeMock.mock.calls.filter(([command]) => command === 'games_catalog_install');
      expect(calls).toHaveLength(2);
      expect(calls[1][1].request).toMatchObject({ entryId: 'skse', installAnyway: true, dryRun: false });
    });
  });

  it('does not offer Install anyway for other refusals', async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'games_catalog_list') return twoEntries;
      if (command === 'games_catalog_install') {
        throw { code: 'core_failed', message: 'The download does not match its published hash.', coreCode: 'ERR_HASH_MISMATCH' };
      }
      return null;
    });
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
    const [first] = await screen.findAllByTestId('catalog-entry');

    fireEvent.click(within(first).getByRole('button', { name: 'Install' }));
    const alert = await within(first).findByRole('alert');
    expect(alert).toHaveTextContent('does not match its published hash');
    expect(within(alert).queryByRole('button', { name: 'Install anyway' })).not.toBeInTheDocument();
  });

  describe('a required framework that is unmet', () => {
    const skseUnmet = {
      framework: 'skse',
      name: 'Skyrim Script Extender',
      minVersion: '2.2',
      problem: 'absent',
      foundVersion: null,
      message: "This mod needs Skyrim Script Extender 2.2 or newer, and this instance doesn't have it. It probably won't work until you add it.",
      catalogEntry: { id: 'skse', name: 'SKSE64' },
    };
    const addressUnmet = {
      framework: 'address-library',
      name: 'Address Library',
      minVersion: null,
      problem: 'undetectable',
      foundVersion: null,
      message: "This mod needs Address Library, and Agora can't tell whether this instance has it. It probably won't work unless you have it.",
      catalogEntry: null,
    };
    const plan = (unmetFrameworks: unknown[]) => ({
      itemId: 'ussep',
      itemName: 'Unofficial Patch',
      instanceId: 'inst-1',
      source: 'GitHub a/b release 1',
      fileName: 'patch.7z',
      release: '1',
      size: 10,
      frameworks: [],
      unmetFrameworks,
      warnings: [],
    });
    const placed = (unmetFrameworks: unknown[]) => ({
      state: 'placed',
      ...plan(unmetFrameworks),
      contentItemId: 'c-1',
      mountPath: 'Data',
      storedSize: 10,
      hash: { sha256: 'ab'.repeat(32), verified: true, basis: 'GitHub published it.', confirmedPast: null },
    });
    const refused = { code: 'core_failed', message: 'needs skse', coreCode: 'ERR_CATALOG_FRAMEWORK_MISSING' };
    const requests = () =>
      invokeMock.mock.calls.filter(([command]) => command === 'games_catalog_install').map(([, args]) => args.request);

    /** The core's behaviour: refuses unless it is a dry run or the override is on. */
    function core(unmet: unknown[] = [skseUnmet]) {
      invokeMock.mockImplementation(async (command: string, args: { request?: Record<string, unknown> }) => {
        if (command === 'games_catalog_list') return twoEntries;
        if (command !== 'games_catalog_install') return null;
        if (args.request?.dryRun) return { state: 'dry_run', ...plan(unmet) };
        if (args.request?.allowMissingFrameworks) return placed(unmet);
        throw refused;
      });
    }

    async function patchCard() {
      const cards = await screen.findAllByTestId('catalog-entry');
      return cards[1];
    }

    it('stops an install, names the framework, and writes nothing until asked', async () => {
      core();
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();

      fireEvent.click(within(card).getByRole('button', { name: 'Install' }));
      const warning = await within(card).findByTestId('catalog-unmet-frameworks');
      expect(warning).toHaveTextContent('This mod needs Skyrim Script Extender 2.2 or newer');
      expect(warning).toHaveTextContent("It probably won't work until you add it.");
      expect(warning).not.toHaveTextContent('Installed anyway');
      expect(within(card).getByTestId('catalog-preview')).toHaveTextContent('Nothing has been downloaded');
      // The refused install, then the plan that names what stopped it. Neither carries the override.
      expect(requests()).toHaveLength(2);
      expect(requests()[0]).toMatchObject({ dryRun: false, allowMissingFrameworks: false });
      expect(requests()[1]).toMatchObject({ dryRun: true, allowMissingFrameworks: false });
    });

    it('goes on only from the button, and says what it went past', async () => {
      core();
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Install' }));
      const warning = await within(card).findByTestId('catalog-unmet-frameworks');

      fireEvent.click(within(warning).getByRole('button', { name: 'Install anyway' }));
      const placedResult = await within(card).findByTestId('catalog-placed');
      expect(requests()[2]).toMatchObject({ dryRun: false, allowMissingFrameworks: true, installAnyway: false });
      expect(placedResult).toHaveTextContent('Installed anyway, as you chose.');
      expect(placedResult).toHaveTextContent('This mod needs Skyrim Script Extender');
      expect(within(placedResult).queryByRole('button', { name: 'Install anyway' })).not.toBeInTheDocument();
    });

    it('the hash switch is not the framework override', async () => {
      core();
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(screen.getByRole('switch', { name: INSTALL_ANYWAY_COPY.label }));
      fireEvent.click(within(card).getByRole('button', { name: 'Install' }));
      await within(card).findByTestId('catalog-unmet-frameworks');
      expect(requests()[0]).toMatchObject({ installAnyway: true, allowMissingFrameworks: false, dryRun: false });
      expect(within(card).queryByTestId('catalog-placed')).not.toBeInTheDocument();
    });

    it('Preview names it too, and can be dismissed', async () => {
      core();
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Preview' }));
      const warning = await within(card).findByTestId('catalog-unmet-frameworks');
      expect(requests()).toHaveLength(1);
      fireEvent.click(within(warning).getByRole('button', { name: 'Not now' }));
      expect(within(card).queryByTestId('catalog-unmet-frameworks')).not.toBeInTheDocument();
      expect(requests()).toHaveLength(1);
    });

    it('names both of two unmet frameworks, with a link only for the one the catalog has', async () => {
      core([skseUnmet, addressUnmet]);
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Preview' }));
      const warning = await within(card).findByTestId('catalog-unmet-frameworks');
      expect(warning).toHaveTextContent('Skyrim Script Extender');
      expect(warning).toHaveTextContent('Address Library');
      expect(within(warning).getAllByRole('button', { name: /^Go to / })).toHaveLength(1);
    });

    it('the link clears the search and takes the user to the framework', async () => {
      core();
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Preview' }));
      await within(card).findByTestId('catalog-unmet-frameworks');
      const search = screen.getByRole('textbox', { name: 'Search mods' });
      fireEvent.change(search, { target: { value: 'unofficial' } });
      expect(screen.getAllByTestId('catalog-entry')).toHaveLength(1);

      fireEvent.click(within(screen.getByTestId('catalog-unmet-frameworks')).getByRole('button', { name: 'Go to SKSE64' }));
      expect(search).toHaveValue('');
      const [framework] = screen.getAllByTestId('catalog-entry');
      await waitFor(() => expect(within(framework).getByRole('button', { name: 'Install' })).toHaveFocus());
    });

    it('does not offer it when something else stands in the way', async () => {
      invokeMock.mockImplementation(async (command: string, args: { request?: Record<string, unknown> }) => {
        if (command === 'games_catalog_list') return twoEntries;
        if (args.request?.dryRun) {
          throw { code: 'core_failed', message: 'No release has a matching file.', coreCode: 'ERR_CATALOG_NO_ASSET' };
        }
        throw refused;
      });
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Install' }));
      const alert = await within(card).findByRole('alert');
      expect(alert).toHaveTextContent('needs skse');
      expect(alert).toHaveTextContent('No release has a matching file.');
      expect(within(card).queryByRole('button', { name: 'Install anyway' })).not.toBeInTheDocument();
    });

    it('keeps the override when a changed file then needs its own answer', async () => {
      invokeMock.mockImplementation(async (command: string, args: { request?: Record<string, unknown> }) => {
        if (command === 'games_catalog_list') return twoEntries;
        if (command !== 'games_catalog_install') return null;
        if (args.request?.dryRun) return { state: 'dry_run', ...plan([skseUnmet]) };
        if (!args.request?.allowMissingFrameworks) throw refused;
        if (!args.request?.installAnyway) {
          throw { code: 'core_failed', message: 'The file differs.', coreCode: 'ERR_HASH_CONFIRMATION_REQUIRED' };
        }
        return placed([skseUnmet]);
      });
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Install' }));
      fireEvent.click(within(await within(card).findByTestId('catalog-unmet-frameworks')).getByRole('button', { name: 'Install anyway' }));
      const stop = await within(card).findByRole('alert');
      expect(stop).toHaveTextContent('The file differs.');
      fireEvent.click(within(stop).getByRole('button', { name: 'Install anyway' }));
      await within(card).findByTestId('catalog-placed');
      expect(requests()[requests().length - 1]).toMatchObject({ installAnyway: true, allowMissingFrameworks: true, dryRun: false });
    });

    it('does not carry the warning over to another instance', async () => {
      invokeMock.mockImplementation(async (command: string, args: { request?: Record<string, unknown> }) => {
        if (command === 'games_catalog_list') {
          return { ...twoEntries, instances: [{ instanceId: 'inst-1', name: 'Grounded' }, { instanceId: 'inst-2', name: 'Second' }] };
        }
        if (args.request?.dryRun) return { state: 'dry_run', ...plan([skseUnmet]) };
        throw refused;
      });
      render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);
      const card = await patchCard();
      fireEvent.click(within(card).getByRole('button', { name: 'Preview' }));
      await within(card).findByTestId('catalog-unmet-frameworks');

      fireEvent.change(screen.getByRole('combobox'), { target: { value: 'inst-2' } });
      expect(within(card).queryByTestId('catalog-unmet-frameworks')).not.toBeInTheDocument();
      fireEvent.change(screen.getByRole('combobox'), { target: { value: 'inst-1' } });
      expect(within(card).getByTestId('catalog-unmet-frameworks')).toBeInTheDocument();
    });
  });

  it('says so when the catalog cannot be read, and tries again', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'bad_game', message: 'Agora does not support that game.' });
    invokeMock.mockResolvedValue({ entries: [], instances: [] });
    render(<GameBrowsePage gameId="skyrim-se" gameName="Skyrim SE" />);

    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Agora does not support that game.');
    expect(screen.queryByTestId('catalog-empty')).not.toBeInTheDocument();
    fireEvent.click(within(alert).getByRole('button', { name: 'Retry' }));
    expect(await screen.findByTestId('catalog-empty')).toBeInTheDocument();
  });
});
