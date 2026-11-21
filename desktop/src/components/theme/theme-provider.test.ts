import { describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

import { mutedTextHsl } from './theme-provider';

describe('mutedTextHsl', () => {
  it('makes secondary text dimmer than the custom body colour', () => {
    // The "Civic Gold" body colour: #f4ead4.
    const body = { h: 41, s: 59, l: 89 };
    expect(mutedTextHsl(body, true)).toBe('41 44% 63%');
    expect(mutedTextHsl(body, true)).not.toBe('41 59% 89%');
  });

  it('keeps a dark body colour readable as secondary text in the light theme', () => {
    const body = { h: 215, s: 75, l: 15 };
    expect(mutedTextHsl(body, false)).toBe('215 56% 39%');
    expect(mutedTextHsl(body, false)).not.toBe('215 75% 15%');
  });
});
