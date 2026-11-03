import { beforeEach, describe, expect, it, vi } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { GameSwitcher } from './GameSwitcher';

const loadGameIconMock = vi.hoisted(() => vi.fn());
vi.mock('../shared/icons', () => ({ loadGameIcon: loadGameIconMock }));

const options = [
  { gameId: 'minecraft', label: 'Minecraft' },
  { gameId: 'skyrim-se', label: 'Skyrim Special Edition' },
];

function renderSwitcher(collapsed = false) {
  return render(
    <GameSwitcher
      options={options}
      shownGame="skyrim-se"
      onSwitch={() => undefined}
      collapsed={collapsed}
    />,
  );
}

async function openCollapsedSwitcher() {
  const user = userEvent.setup();
  const rendered = renderSwitcher(true);
  await user.click(screen.getByRole('button', { name: /Game: Skyrim Special Edition/ }));
  return rendered;
}

describe('GameSwitcher icons', () => {
  beforeEach(() => {
    loadGameIconMock.mockReset();
  });

  it('shows the game icon when the command returns one', async () => {
    loadGameIconMock.mockResolvedValue('data:image/png;base64,U0tZUklN');
    const { container } = renderSwitcher();
    await waitFor(() => {
      expect(container.querySelector('img[src="data:image/png;base64,U0tZUklN"]')).toBeTruthy();
    });
    expect(loadGameIconMock).toHaveBeenCalledWith('skyrim-se');
    // Minecraft is never asked; it has no discovery install to take an exe from.
    expect(loadGameIconMock).not.toHaveBeenCalledWith('minecraft');
  });

  it('shows the icon instead of the letter in a collapsed sidebar', async () => {
    loadGameIconMock.mockResolvedValue('data:image/png;base64,U0tZUklN');
    const { container } = await openCollapsedSwitcher();
    await waitFor(() => {
      expect(container.querySelector('img[src="data:image/png;base64,U0tZUklN"]')).toBeTruthy();
    });
  });

  it('keeps the letter when the command returns null', async () => {
    loadGameIconMock.mockResolvedValue(null);
    await openCollapsedSwitcher();
    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Skyrim Special Edition' }).textContent).toContain('S');
    });
    expect(screen.getByRole('button', { name: 'Minecraft' }).textContent).toContain('M');
  });

  it('keeps the letter while the request is pending', async () => {
    loadGameIconMock.mockReturnValue(new Promise(() => undefined));
    await openCollapsedSwitcher();
    expect(screen.getByRole('button', { name: 'Skyrim Special Edition' }).textContent).toContain('S');
    expect(screen.getByRole('button', { name: 'Minecraft' }).textContent).toContain('M');
  });

  it('keeps the letter when the call rejects', async () => {
    loadGameIconMock.mockRejectedValue(new Error('games_icon did not return an icon.'));
    await openCollapsedSwitcher();
    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Skyrim Special Edition' }).textContent).toContain('S');
    });
  });
});

describe('GameSwitcher names', () => {
  beforeEach(() => {
    loadGameIconMock.mockReset();
    loadGameIconMock.mockResolvedValue(null);
  });

  it('shows the short name and keeps the full one as the accessible name and tooltip', async () => {
    const user = userEvent.setup();
    render(
      <GameSwitcher
        options={[
          { gameId: 'minecraft', label: 'Minecraft' },
          { gameId: 'skyrim-se', label: 'The Elder Scrolls V: Skyrim Special Edition', shortLabel: 'Skyrim SE' },
        ]}
        shownGame="skyrim-se"
        onSwitch={() => undefined}
        collapsed={false}
      />,
    );
    const current = screen.getByRole('button', { name: 'Game: The Elder Scrolls V: Skyrim Special Edition. Choose a game' });
    expect(current).toHaveTextContent('Skyrim SE');
    expect(current).toHaveAttribute('title', 'The Elder Scrolls V: Skyrim Special Edition');

    await user.click(current);
    const option = screen.getByRole('button', { name: 'The Elder Scrolls V: Skyrim Special Edition' });
    expect(option).toHaveTextContent('Skyrim SE');
    expect(screen.getByRole('button', { name: 'Minecraft' })).toHaveTextContent('Minecraft');
  });
});
