import { describe, expect, it } from 'vitest';
import type { Tab } from '@/lib/useDestination';
import { GAME_ONLY_TABS, gameTabForSidebarTab, sidebarTabForGameTab, withGameOnlyEntries } from './gameNav';

describe('one sidebar for every game', () => {
  it("opens the game's own page for each shared entry, and the closest page where it has none", () => {
    expect(gameTabForSidebarTab('instances')).toBe('instances');
    expect(gameTabForSidebarTab('library')).toBe('library');
    expect(gameTabForSidebarTab('home')).toBe('instances');
    expect(gameTabForSidebarTab('browse')).toBe('library');
    expect(gameTabForSidebarTab('game-import')).toBe('import');
    expect(gameTabForSidebarTab('game-setup')).toBe('setup');
  });

  it('leaves the app-wide entries to the app', () => {
    for (const tab of ['settings', 'guide', 'governance', 'field-guide', 'living-background', 'about', 'lab'] as Tab[]) {
      expect(gameTabForSidebarTab(tab)).toBeNull();
    }
  });

  it("highlights a game's page under the entry Minecraft uses for the same idea", () => {
    expect(sidebarTabForGameTab('instances')).toBe('instances');
    expect(sidebarTabForGameTab('library')).toBe('library');
    expect(sidebarTabForGameTab('import')).toBe('game-import');
    expect(sidebarTabForGameTab('setup')).toBe('game-setup');
  });

  it('puts the game-only entries right after My Instances', () => {
    const shared = ['home', 'browse', 'library', 'instances', 'guide', 'settings'].map((id) => ({ id: id as Tab }));
    const extra = GAME_ONLY_TABS.map((entry) => ({ id: entry.id }));
    expect(withGameOnlyEntries(shared, extra).map((entry) => entry.id))
      .toEqual(['home', 'browse', 'library', 'instances', 'game-import', 'game-setup', 'guide', 'settings']);
    // The palette's entries are keyed by `tab`; a list without My Instances gets them at the end.
    expect(withGameOnlyEntries([{ tab: 'settings' as Tab }], [{ tab: 'game-setup' as Tab }]).map((entry) => entry.tab))
      .toEqual(['settings', 'game-setup']);
  });
});
