import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { Switch } from './switch';

function cssFile(): string {
  return readFileSync(resolve(process.cwd(), 'src/index.css'), 'utf8');
}

describe('Switch', () => {
  it('is off with a neutral track and on with the theme accent', () => {
    const { rerender } = render(<Switch aria-label="Music" checked={false} />);
    const off = screen.getByRole('switch', { name: 'Music' });
    expect(off).toHaveAttribute('aria-checked', 'false');
    expect(off).toHaveClass('data-[state=unchecked]:bg-[hsl(var(--switch-off))]');

    rerender(<Switch aria-label="Music" checked />);
    const on = screen.getByRole('switch', { name: 'Music' });
    expect(on).toHaveAttribute('aria-checked', 'true');
    expect(on).toHaveClass('data-[state=checked]:bg-primary');
  });

  it('reports the new state when clicked', async () => {
    const user = userEvent.setup();
    const onCheckedChange = vi.fn();
    render(<Switch aria-label="Music" checked={false} onCheckedChange={onCheckedChange} />);

    await user.click(screen.getByRole('switch', { name: 'Music' }));

    expect(onCheckedChange).toHaveBeenCalledWith(true);
  });

  it('toggles from the keyboard with Space and Enter', async () => {
    const user = userEvent.setup();
    const onCheckedChange = vi.fn();
    render(<Switch aria-label="Music" checked={false} onCheckedChange={onCheckedChange} />);
    const control = screen.getByRole('switch', { name: 'Music' });

    control.focus();
    await user.keyboard(' ');
    expect(onCheckedChange).toHaveBeenLastCalledWith(true);

    await user.keyboard('{Enter}');
    expect(onCheckedChange).toHaveBeenCalledTimes(2);
  });

  it('takes its accessible name from a label', () => {
    render(
      <>
        <label htmlFor="ambient-sounds">Ambient sounds</label>
        <Switch id="ambient-sounds" checked={false} />
      </>,
    );
    expect(screen.getByRole('switch', { name: 'Ambient sounds' })).toBeInTheDocument();
  });

  it('is dimmed and not interactive when disabled', async () => {
    const user = userEvent.setup();
    const onCheckedChange = vi.fn();
    render(<Switch aria-label="Music" checked={false} disabled onCheckedChange={onCheckedChange} />);
    const control = screen.getByRole('switch', { name: 'Music' });

    expect(control).toBeDisabled();
    expect(control).toHaveClass('disabled:opacity-50');
    await user.click(control);
    expect(onCheckedChange).not.toHaveBeenCalled();
  });

  it('animates the track and knob, and the app-wide reduced-motion rules stop it', () => {
    render(<Switch aria-label="Music" checked={false} />);
    const control = screen.getByRole('switch', { name: 'Music' });
    const thumb = control.firstElementChild;

    expect(control).toHaveClass('transition-colors');
    expect(thumb).toHaveClass('transition-transform');

    // The app honours reduced motion globally (the user's motion setting and
    // the OS preference); this is the same mechanism every other animation uses.
    const css = cssFile();
    expect(css).toContain(':root[data-motion="reduced"] *');
    expect(css).toContain('transition-duration: 0.01ms !important');
    expect(css).toContain('@media (prefers-reduced-motion: reduce)');
  });
});
