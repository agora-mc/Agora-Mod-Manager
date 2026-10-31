import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('./api', () => ({
  scanMo2Setups: vi.fn(async () => ({
    setups: [{ iniPath: 'D:/MO2/ModOrganizer.ini', gameName: 'Skyrim Special Edition', profiles: ['Default'], modCount: 3, foundBy: 'a usual place' }],
    warnings: [],
  })),
  importMo2Profile: vi.fn(),
  listenImportProgress: vi.fn(async () => () => undefined),
}));

import { GameImportPage } from './index';

function answer(settings: Record<string, unknown> = {}) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === 'get_setting') {
      const key = String((args as { key?: unknown } | undefined)?.key ?? '');
      return Object.prototype.hasOwnProperty.call(settings, key) ? settings[key] : null;
    }
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe('Import saves default', () => {
  it('leaves imported saves where they are unless the game asks for a copy', async () => {
    answer();
    render(<GameImportPage gameId="skyrim-se" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    expect(await screen.findByRole('checkbox')).not.toBeChecked();
  });

  it('starts with the copy box ticked when this game saved that choice', async () => {
    answer({ 'game.skyrim-se.importCopySaves': 'on' });
    render(<GameImportPage gameId="skyrim-se" />);

    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('get_setting', {
      key: 'game.skyrim-se.importCopySaves',
    }));
    fireEvent.click(await screen.findByRole('button', { name: 'Use this setup' }));
    expect(await screen.findByRole('checkbox')).toBeChecked();
  });
});
