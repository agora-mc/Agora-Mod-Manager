import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import {
  SETTINGS_CARD_AUDIENCE,
  gameDisplayName,
  isMinecraftSelected,
  isSettingsCardVisible,
} from './gameScope';

const CARD_FILES = [
  '../Settings.tsx',
  './AiAssistantCard.tsx',
  './AppearanceSettings.tsx',
  './game/InstallsBasesSettings.tsx',
  './game/NewInstancesSettings.tsx',
  './GameSupportSettings.tsx',
  './RuntimeReclaim.tsx',
  './TemplateSettings.tsx',
];

/** The classified `card="..."` every SettingsSection must carry. */
function cardIdsInSource(source: string): string[] {
  const ids: string[] = [];
  const opening = /<SettingsSection\b([^>]*)>/g;
  let match: RegExpExecArray | null;
  while ((match = opening.exec(source)) !== null) {
    const card = /\bcard="([^"]+)"/.exec(match[1]);
    expect(card, `a SettingsSection has no classified card: ${match[0].slice(0, 120)}`).not.toBeNull();
    ids.push(card![1]);
  }
  return ids;
}

describe('Settings card classification', () => {
  it('classifies every card the Settings page renders', () => {
    const seen = new Set<string>();
    for (const file of CARD_FILES) {
      const source = readFileSync(fileURLToPath(new URL(file, import.meta.url)), 'utf8');
      for (const id of cardIdsInSource(source)) {
        expect(SETTINGS_CARD_AUDIENCE, `card "${id}" is not in SETTINGS_CARD_AUDIENCE`).toHaveProperty(id);
        seen.add(id);
      }
    }
    // A smoke check: the audit covers the page's main cards, not an empty list.
    expect(seen.size).toBeGreaterThan(20);
  });
});

describe('gameScope helpers', () => {
  it('treats Minecraft and an unloaded choice as Minecraft, and any other id as another game', () => {
    expect(isMinecraftSelected(undefined)).toBe(true);
    expect(isMinecraftSelected(null)).toBe(true);
    expect(isMinecraftSelected('minecraft')).toBe(true);
    expect(isMinecraftSelected('skyrim-se')).toBe(false);
    expect(isMinecraftSelected('valheim')).toBe(false);
  });

  it('hides Minecraft-only cards for another game but keeps app-wide ones', () => {
    expect(isSettingsCardVisible('java-runtime', true)).toBe(true);
    expect(isSettingsCardVisible('java-runtime', false)).toBe(false);
    expect(isSettingsCardVisible('game-support', false)).toBe(true);
    expect(isSettingsCardVisible('github-account', false)).toBe(true);
  });

  it("shows the selected game's own cards only while that game is selected", () => {
    expect(isSettingsCardVisible('game-new-instances', false)).toBe(true);
    expect(isSettingsCardVisible('game-installs', false)).toBe(true);
    expect(isSettingsCardVisible('game-new-instances', true)).toBe(false);
    expect(isSettingsCardVisible('game-bases', true)).toBe(false);
  });

  it('prefers a discovered game name and otherwise humanizes the id', () => {
    expect(gameDisplayName('skyrim-se', 'Skyrim Special Edition')).toBe('Skyrim Special Edition');
    expect(gameDisplayName('valheim')).toBe('Valheim');
    expect(gameDisplayName('skyrim-se')).toBe('Skyrim Se');
  });
});
