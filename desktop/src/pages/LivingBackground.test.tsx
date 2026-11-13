import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';

const ambience = {
  soundOn: false, setSoundOn: vi.fn(), soundVolume: 0.5, setSoundVolume: vi.fn(),
  musicVolume: 0.35, setMusicVolume: vi.fn(), musicOn: true, setMusicOn: vi.fn(),
  clearBackground: true, setClearBackground: vi.fn(),
  setTod: vi.fn(), setWeather: vi.fn(), setTodLocked: vi.fn(), setWeatherLocked: vi.fn(),
  readClock: () => ({ tod: 0.5, weather: 'rain', todLocked: true, weatherLocked: true }),
  readView: () => ({ zoom: 1.6, tx: 0, ty: 0 }),
  setZoom: vi.fn(), setRainbow: vi.fn(), isRainbowPinned: () => false,
  setTrack: vi.fn(), setInstrument: vi.fn(), currentTrackId: () => null,
  setMusicAuto: vi.fn(), shuffleMusic: vi.fn(), setBuddy: vi.fn(), moveCursorBy: vi.fn(), panBy: vi.fn(), clickAtCursor: vi.fn(), dropCarried: vi.fn(() => false), setCursorVisible: vi.fn(), ready: true,
};
vi.mock('../features/ambience/AmbienceProvider', () => ({ useAmbience: () => ambience }));
const reloadPage = vi.fn();
vi.mock('../lib/reloadPage', () => ({ reloadPage: () => reloadPage() }));

import { LivingBackground } from './LivingBackground';

describe('LivingBackground reset scene', () => {
  it('shows the zoom readout and reloads the page without touching settings', () => {
    reloadPage.mockClear();
    render(<LivingBackground />);
    expect(screen.getByTestId('living-bg-zoom-readout').textContent).toBe('1.6×');

    fireEvent.click(screen.getByTestId('living-bg-reset'));

    expect(reloadPage).toHaveBeenCalledTimes(1);
    // A refresh, not a settings reset: nothing saved is changed on the way.
    expect(ambience.setClearBackground).not.toHaveBeenCalled();
    expect(ambience.setZoom).not.toHaveBeenCalled();
  });

  it('moves the zoom slider to a floating top-right control while the panel is hidden', () => {
    reloadPage.mockClear();
    render(<LivingBackground />);
    expect(screen.queryByTestId('living-bg-zoom-float')).toBeNull();
    expect(screen.getAllByText('(click and drag to move the view)')).toHaveLength(1);

    fireEvent.click(screen.getByTestId('living-bg-hide'));

    expect(screen.queryByTestId('living-bg-controls')).toBeNull();
    expect(screen.getByTestId('living-bg-zoom-float').classList.contains('fixed')).toBe(true);
    expect(screen.getByTestId('living-bg-zoom-float-readout').textContent).toBe('1.6×');
    expect(screen.getAllByText('(click and drag to move the view)')).toHaveLength(1);

    fireEvent.click(screen.getByTestId('living-bg-zoom-float-reset'));

    expect(reloadPage).toHaveBeenCalledTimes(1);
  });
});
