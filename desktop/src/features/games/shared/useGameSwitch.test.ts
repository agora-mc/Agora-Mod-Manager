import { act, renderHook } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { useGameSwitch } from './useGameSwitch';

function setup() {
  const setActiveGame = vi.fn();
  const navigateToTab = vi.fn();
  const navigateToGameTab = vi.fn();
  const { result } = renderHook(() => useGameSwitch({ setActiveGame, navigateToTab, navigateToGameTab }));
  return { switchGame: result.current, setActiveGame, navigateToTab, navigateToGameTab };
}

describe('useGameSwitch', () => {
  it('changes the game without navigating when the caller asks to stay put', () => {
    const { switchGame, setActiveGame, navigateToTab, navigateToGameTab } = setup();

    act(() => switchGame('minecraft', { navigate: false }));

    expect(setActiveGame).toHaveBeenCalledWith('minecraft');
    expect(navigateToTab).not.toHaveBeenCalled();
    expect(navigateToGameTab).not.toHaveBeenCalled();
  });

  it('opens Home when switching to Minecraft normally', () => {
    const { switchGame, setActiveGame, navigateToTab } = setup();

    act(() => switchGame('minecraft'));

    expect(setActiveGame).toHaveBeenCalledWith('minecraft');
    expect(navigateToTab).toHaveBeenCalledWith('home');
  });

  it('opens the other game\'s Home when switching normally', () => {
    const { switchGame, navigateToGameTab } = setup();

    act(() => switchGame('skyrim-se'));

    expect(navigateToGameTab).toHaveBeenCalledWith('skyrim-se', 'home');
  });
});
