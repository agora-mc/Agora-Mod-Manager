import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const pickOpenFile = vi.hoisted(() => vi.fn());
const openExternalUrl = vi.hoisted(() => vi.fn());
vi.mock('@/lib/tauri', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/tauri')>()),
  pickOpenFile,
  openExternalUrl,
}));
const installCatalogEntry = vi.hoisted(() => vi.fn());
vi.mock('./api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('./api')>()),
  installCatalogEntry,
}));

import { ManualDownloadDialog } from './ManualDownloadDialog';

const entry = {
  id: 'address-library',
  name: 'Address Library for SKSE Plugins',
  author: 'meh321',
  description: 'Version files for SKSE plugins.',
  downloadStrategy: 'manual_link',
  status: 'active',
  pageUrl: 'https://www.nexusmods.com/skyrimspecialedition/mods/32444',
  compatibility: [
    {
      stores: ['steam'],
      gameVersions: ['1.6.1170'],
      requires: [],
      asset: '*-32444-*.zip',
      downloadHint: 'Main file: All in one (1.6.x)',
    },
  ],
};

const FILE = 'D:/Downloads/Address Library-32444-1.6.1170-1700000000.zip';

function renderDialog(props: Partial<Parameters<typeof ManualDownloadDialog>[0]> = {}) {
  return render(
    <ManualDownloadDialog
      entry={entry}
      open
      onOpenChange={() => undefined}
      gameId="skyrim-se"
      instanceId="inst-1"
      watching={false}
      installAnyway={false}
      onInstalled={() => undefined}
      {...props}
    />,
  );
}

describe('ManualDownloadDialog', () => {
  beforeEach(() => {
    pickOpenFile.mockReset();
    openExternalUrl.mockReset();
    installCatalogEntry.mockReset();
  });

  it('says where the mod is, what to do, and offers the page and the file picker', () => {
    renderDialog();
    expect(screen.getByRole('dialog')).toHaveTextContent('Download Address Library for SKSE Plugins yourself');
    expect(screen.getByRole('dialog')).toHaveTextContent(
      "Address Library for SKSE Plugins is on nexusmods.com, which Agora doesn't download from. Open the page, download the file, then choose it here.",
    );
    expect(screen.getByText('Main file: All in one (1.6.x)')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Open page' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Choose file…' })).toBeInTheDocument();
  });

  it('tells the player how watching changes the offer', () => {
    const { rerender } = renderDialog({ watching: true });
    expect(screen.getByText('Agora will also spot it in your Downloads folder.')).toBeInTheDocument();
    rerender(
      <ManualDownloadDialog
        entry={entry}
        open
        onOpenChange={() => undefined}
        gameId="skyrim-se"
        instanceId="inst-1"
        watching={false}
        installAnyway={false}
        onInstalled={() => undefined}
      />,
    );
    expect(
      screen.getByText('Turn on Notice mod downloads in Settings and Agora will spot it for you.'),
    ).toBeInTheDocument();
  });

  it('opens the page through the external-link helper', () => {
    renderDialog();
    fireEvent.click(screen.getByRole('button', { name: 'Open page' }));
    expect(openExternalUrl).toHaveBeenCalledWith(entry.pageUrl);
  });

  it('chooses a file, asks about the missing checksum, then installs it', async () => {
    pickOpenFile.mockResolvedValue(FILE);
    installCatalogEntry
      .mockResolvedValueOnce({
        state: 'needs_checksum',
        itemId: entry.id,
        itemName: entry.name,
        pageUrl: entry.pageUrl,
        asset: '*-32444-*.zip',
        fileName: 'Address Library-32444-1.6.1170-1700000000.zip',
        hint: 'Main file: All in one (1.6.x)',
      })
      .mockResolvedValueOnce({
        state: 'placed',
        itemId: entry.id,
        itemName: entry.name,
        instanceId: 'inst-1',
        source: `the page at ${entry.pageUrl}`,
        fileName: 'Address Library-32444-1.6.1170-1700000000.zip',
        release: null,
        size: 100,
        frameworks: [],
        unmetFrameworks: [],
        warnings: [],
        contentItemId: 'content-1',
        mountPath: 'Data',
        storedSize: 100,
        hash: { sha256: 'a'.repeat(64), verified: false, basis: 'not verified', confirmedPast: null },
      });
    const onInstalled = vi.fn();
    renderDialog({ onInstalled });

    fireEvent.click(screen.getByRole('button', { name: 'Choose file…' }));
    const notice = await screen.findByTestId('manual-no-checksum');
    expect(notice).toHaveTextContent(
      "nexusmods.com doesn't publish a checksum, so Agora couldn't check this file. Only install a file you downloaded from the page above.",
    );
    expect(installCatalogEntry).toHaveBeenLastCalledWith(
      expect.objectContaining({ filePath: FILE, confirmNoChecksum: false }),
    );

    fireEvent.click(screen.getByRole('button', { name: 'Install' }));
    await screen.findByTestId('manual-installed');
    expect(installCatalogEntry).toHaveBeenLastCalledWith(
      expect.objectContaining({ filePath: FILE, confirmNoChecksum: true }),
    );
    expect(onInstalled).toHaveBeenCalled();
  });

  it('asks before installing a file whose name does not match', async () => {
    pickOpenFile.mockResolvedValue('D:/Downloads/something-else.zip');
    installCatalogEntry
      .mockResolvedValueOnce({
        state: 'wrong_file_name',
        itemId: entry.id,
        itemName: entry.name,
        pageUrl: entry.pageUrl,
        asset: '*-32444-*.zip',
        fileName: 'something-else.zip',
        hint: null,
      })
      .mockResolvedValueOnce({
        state: 'placed',
        itemId: entry.id,
        itemName: entry.name,
        instanceId: 'inst-1',
        source: `the page at ${entry.pageUrl}`,
        fileName: 'something-else.zip',
        release: null,
        size: 100,
        frameworks: [],
        unmetFrameworks: [],
        warnings: [],
        contentItemId: 'content-1',
        mountPath: 'Data',
        storedSize: 100,
        hash: { sha256: 'a'.repeat(64), verified: false, basis: 'not verified', confirmedPast: null },
      });
    renderDialog();

    fireEvent.click(screen.getByRole('button', { name: 'Choose file…' }));
    const notice = await screen.findByTestId('manual-wrong-name');
    expect(notice).toHaveTextContent(
      "This doesn't look like Address Library for SKSE Plugins's file. Agora expected a name like *-32444-*.zip.",
    );

    fireEvent.click(screen.getByRole('button', { name: 'Install anyway' }));
    await screen.findByTestId('manual-installed');
    expect(installCatalogEntry).toHaveBeenLastCalledWith(
      expect.objectContaining({ filePath: 'D:/Downloads/something-else.zip', installAnyway: true }),
    );
  });

  it('shows a cancelled picker as nothing, not an error', async () => {
    pickOpenFile.mockResolvedValue(null);
    renderDialog();
    fireEvent.click(screen.getByRole('button', { name: 'Choose file…' }));
    await waitFor(() => expect(pickOpenFile).toHaveBeenCalled());
    expect(installCatalogEntry).not.toHaveBeenCalled();
    expect(screen.queryByTestId('manual-error')).not.toBeInTheDocument();
  });
});
