import { describe, expect, it } from 'vitest';
import { gameShortName } from './gameNames';

describe('gameShortName', () => {
  it('drops a series prefix and abbreviates the edition', () => {
    expect(gameShortName('The Elder Scrolls V: Skyrim Special Edition')).toBe('Skyrim SE');
    expect(gameShortName('Skyrim Special Edition')).toBe('Skyrim SE');
    expect(gameShortName('Fallout 4: Game of the Year Edition')).toBe('Fallout 4 GOTY');
  });

  it('keeps names that are already short, and never returns an empty name', () => {
    expect(gameShortName('Valheim')).toBe('Valheim');
    expect(gameShortName('Minecraft')).toBe('Minecraft');
    expect(gameShortName('Fallout 4')).toBe('Fallout 4');
    expect(gameShortName('Odd:')).toBe('Odd:');
    expect(gameShortName('')).toBe('');
  });
});
