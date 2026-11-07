/**
 * Generated instance tiles.
 *
 * The placeholder is derived rather than stored, so the properties that make
 * it usable are properties of the derivation: the same instance must always
 * produce the same tile, instances on the same loader must stay in that
 * loader's hue family, and two instances on the same loader must still differ.
 */

import { describe, expect, it } from 'vitest';
import { instanceFamilyHue, instanceInitials, instanceTint, loaderHue } from './instanceIdentity';

/** Shortest arc between two hues on the colour wheel. */
function hueDistance(a: number, b: number): number {
  const d = Math.abs(a - b) % 360;
  return d > 180 ? 360 - d : d;
}

describe('instanceTint', () => {
  it('is stable for the same instance', () => {
    expect(instanceTint('inst-abc', 'fabric')).toEqual(instanceTint('inst-abc', 'fabric'));
  });

  it('keeps an instance inside its loader hue family', () => {
    for (const loader of ['vanilla', 'fabric', 'quilt', 'forge', 'neoforge']) {
      for (const seed of ['a', 'inst-1', 'a-much-longer-instance-id', '9f2c']) {
        const { hueA } = instanceTint(seed, loader);
        expect(hueDistance(hueA, loaderHue(loader))).toBeLessThanOrEqual(11);
      }
    }
  });

  it('separates instances that share a loader', () => {
    const seeds = ['inst-1', 'inst-2', 'inst-3', 'inst-4', 'inst-5'];
    const tints = seeds.map((seed) => JSON.stringify(instanceTint(seed, 'fabric')));
    expect(new Set(tints).size).toBe(seeds.length);
  });

  it('never lets two loader families overlap', () => {
    // Fabric and NeoForge once shared a hue range, so a NeoForge tile could
    // come out the same tan as a Fabric one. The gap between every pair of
    // families has to stay wider than the jitter can reach from both sides.
    const loaders = ['vanilla', 'fabric', 'quilt', 'forge', 'neoforge'];
    for (const a of loaders) {
      for (const b of loaders) {
        if (a === b) continue;
        expect(hueDistance(loaderHue(a), loaderHue(b))).toBeGreaterThan(22);
      }
    }
  });

  it('gives unknown loaders their own family rather than another loader’s', () => {
    const unknown = instanceTint('inst-1', 'some-future-loader').hueA;
    for (const loader of ['vanilla', 'fabric', 'quilt', 'forge', 'neoforge']) {
      expect(hueDistance(unknown, loaderHue(loader))).toBeGreaterThan(11);
    }
  });

  it('produces angles and hues inside CSS-legal ranges', () => {
    const { hueA, hueB, angle } = instanceTint('inst-1', 'quilt');
    for (const hue of [hueA, hueB]) {
      expect(hue).toBeGreaterThanOrEqual(0);
      expect(hue).toBeLessThan(360);
    }
    expect(angle).toBeGreaterThanOrEqual(110);
    expect(angle).toBeLessThan(180);
  });
});

describe('instanceInitials', () => {
  it('takes one letter from each of the first two words', () => {
    expect(instanceInitials('Sky Factory')).toBe('SF');
    expect(instanceInitials('All The Mods 9')).toBe('AT');
  });

  it('takes two letters from a single word', () => {
    expect(instanceInitials('Optimized')).toBe('OP');
    expect(instanceInitials('X')).toBe('X');
  });

  it('ignores punctuation and spacing', () => {
    expect(instanceInitials('  ~ modpack! ')).toBe('MO');
    expect(instanceInitials('my-cool-pack')).toBe('MC');
  });

  it('falls back rather than rendering nothing', () => {
    expect(instanceInitials('')).toBe('?');
    expect(instanceInitials('---')).toBe('?');
  });
});

describe('instanceFamilyHue', () => {
  it('replaces the loader family, so instances of one game do not all share a colour', () => {
    // Every instance of another game has no loader; before, they were all the teal fallback.
    const seeds = ['skyrim-1', 'skyrim-2', 'skyrim-3', 'skyrim-4', 'skyrim-5', 'skyrim-6'];
    const families = seeds.map(instanceFamilyHue);
    expect(new Set(families).size).toBe(seeds.length);
    const spread = Math.max(...families) - Math.min(...families);
    expect(spread).toBeGreaterThan(90);
  });

  it('keeps the tile inside the family it gives, and stable', () => {
    const family = instanceFamilyHue('grounded-1');
    expect(family).toBe(instanceFamilyHue('grounded-1'));
    expect(hueDistance(instanceTint('grounded-1', null, family).hueA, family)).toBeLessThanOrEqual(11);
  });

  it('leaves Minecraft tiles unchanged when no family is given', () => {
    expect(instanceTint('inst-abc', 'fabric', undefined)).toEqual(instanceTint('inst-abc', 'fabric'));
    expect(instanceTint('inst-abc', 'fabric', null)).toEqual(instanceTint('inst-abc', 'fabric'));
  });
});
