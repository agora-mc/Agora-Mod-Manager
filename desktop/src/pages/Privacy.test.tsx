import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

vi.mock('../lib/tauri', () => ({
  getSetting: vi.fn(async () => true),
  setSetting: vi.fn(async () => undefined),
  getConnectivityStatus: vi.fn(async () => ({ state: 'unknown', last_outcome_at: null })),
  getProxySetting: vi.fn(async () => ({ setting: { mode: 'system' }, password_stored: false })),
}));

import { Privacy } from './Privacy';

describe('Privacy', () => {
  it('gives every network switch its own accessible name', async () => {
    render(<Privacy />);

    const switches = await screen.findAllByRole('switch');
    expect(switches.length).toBeGreaterThan(0);
    for (const control of switches) {
      expect(control).toHaveAccessibleName();
      expect(control.getAttribute('aria-label')).toBeTruthy();
    }
  });
});
