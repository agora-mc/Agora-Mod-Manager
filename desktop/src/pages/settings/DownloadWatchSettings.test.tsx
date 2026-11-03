import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { DownloadWatchSettings } from './DownloadWatchSettings';

function answer(overrides: Record<string, unknown> = {}) {
  const values: Record<string, unknown> = {
    notice_mod_downloads: false,
    mod_downloads_watch_folder: null,
    mod_downloads_toast: true,
    ...overrides,
  };
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'get_setting':
        return Object.prototype.hasOwnProperty.call(values, String(args?.key))
          ? values[String(args?.key)]
          : null;
      case 'set_setting':
        values[String(args?.key)] = args?.value;
        return null;
      case 'downloads_default_folder':
        return 'C:\\Users\\me\\Downloads';
      case 'downloads_scan':
        return 0;
      case 'pick_directory':
        return 'D:\\Mods Inbox';
      default:
        return null;
    }
  });
  return values;
}

describe('DownloadWatchSettings', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    answer();
  });

  it('is off by default and shows the folder it would watch', async () => {
    render(<DownloadWatchSettings />);

    await waitFor(() => expect(screen.getByRole('switch', { name: 'Notice mod downloads' })).toBeEnabled());
    expect(screen.getByText('Watch for new mods')).toBeInTheDocument();
    expect(
      screen.getByRole('switch', { name: 'Notice mod downloads' }),
    ).not.toBeChecked();
    expect(
      screen.getByRole('switch', { name: 'Tell me when a new mod arrives' }),
    ).toBeChecked();
    expect(await screen.findByText('C:\\Users\\me\\Downloads')).toBeInTheDocument();
  });

  it('turns watching on and records the folder baseline', async () => {
    render(<DownloadWatchSettings />);
    await waitFor(() => expect(screen.getByRole('switch', { name: 'Notice mod downloads' })).toBeEnabled());

    fireEvent.click(screen.getByRole('switch', { name: 'Notice mod downloads' }));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_setting', {
        key: 'notice_mod_downloads',
        value: true,
      }),
    );
    expect(invokeMock).toHaveBeenCalledWith('downloads_scan');
    await waitFor(() => expect(screen.getByRole('switch', { name: 'Notice mod downloads' })).toBeChecked());
  });

  it('can turn the toast off without turning watching off', async () => {
    render(<DownloadWatchSettings />);
    await waitFor(() => expect(screen.getByRole('switch', { name: 'Notice mod downloads' })).toBeEnabled());

    fireEvent.click(
      screen.getByRole('switch', { name: 'Tell me when a new mod arrives' }),
    );

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_setting', {
        key: 'mod_downloads_toast',
        value: false,
      }),
    );
  });

  it('changes the watched folder with the folder picker', async () => {
    render(<DownloadWatchSettings />);
    await screen.findByText('C:\\Users\\me\\Downloads');

    fireEvent.click(screen.getByRole('button', { name: /Change folder/ }));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_setting', {
        key: 'mod_downloads_watch_folder',
        value: 'D:\\Mods Inbox',
      }),
    );
    expect(await screen.findByText('D:\\Mods Inbox')).toBeInTheDocument();
  });
});
