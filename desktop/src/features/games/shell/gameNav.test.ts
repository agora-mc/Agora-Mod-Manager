import { describe, expect, it } from 'vitest';
import type { Tab } from '@/lib/useDestination';
import { gameTabForSidebarTab, sidebarTabForGameTab } from './gameNav';

describe('one sidebar for every game', () => {
  it("opens the game's own page for each shared entry, and the closest page where it has none", () => {
    expect(gameTabForSidebarTab('instances')).toBe('instances');
    expect(gameTabForSidebarTab('library')).toBe('library');
    expect(gameTabForSidebarTab('home')).toBe('instances');
    expect(gameTabForSidebarTab('browse')).toBe('library');
  });

  it('leaves the app-wide entries to the app', () => {
    for (const tab of ['settings', 'guide', 'governance', 'field-guide', 'living-background', 'about', 'lab'] as Tab[]) {
      expect(gameTabForSidebarTab(tab)).toBeNull();
    }
  });

  it("highlights a game's page under the entry Minecraft uses for the same idea", () => {
    expect(sidebarTabForGameTab('instances')).toBe('instances');
    expect(sidebarTabForGameTab('library')).toBe('library');
  });
});
