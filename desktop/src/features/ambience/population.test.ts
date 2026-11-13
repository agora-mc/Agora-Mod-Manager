/**
 * The world must keep breathing.
 *
 * Runs an hour of world time and checks that animals keep arriving and that
 * nothing takes up residence for good. The original spawner "evicted" a
 * resident by clearing the timestamp that makes it leave, so every eviction
 * left an immortal duck or owl behind, and after about twenty minutes the
 * walk-on animals stopped coming entirely.
 */
import { describe, expect, it } from 'vitest';
import { createEngineState } from './engine/state';
import { createWorld } from './engine/world';
import { SPECIES_BY_KEY } from './engine/species';
import { groundYWorld } from './engine/terrain';
import type { WorldState } from './engine/types';

function run(seed: number): { world: WorldState; oldest: number; walkersSeen: number[]; stacked: number } {
  const state = createEngineState(false, seed);
  state.W = 1280; state.H = 720;
  const world = createWorld(state, { blip() {}, burst() {}, emit() {} });
  state.world = world;
  let oldest = 0, stacked = 0;
  const walkersSeen: number[] = [];
  for (let s = 0; s < 3600; s++) {
    world.dl = Math.max(0, Math.sin(((s / 900) % 1) * Math.PI));
    for (let i = 0; i < 30; i++) world.update(1 / 30);
    world.entities.forEach((e) => { if (e.sp.speed > 8) oldest = Math.max(oldest, e.t); });
    const seats = world.entities.filter((e) => e.perch).map((e) => e.perch!.id);
    if (new Set(seats).size !== seats.length) stacked++;
    if (s % 300 === 299) {
      walkersSeen.push(world.entities.filter((e) => e.layer !== 'pond' && !e.perch).length);
    }
  }
  return { world, oldest, walkersSeen, stacked };
}

describe('world population', () => {
  it.each([1, 2, 3])('keeps animals arriving for an hour (seed %i)', (seed) => {
    const r = run(seed);
    // sampled every five minutes: there is always somebody passing through
    expect(r.walkersSeen.every((n) => n > 0)).toBe(true);
    // nothing but the slowest crawlers stays longer than a few minutes
    expect(r.oldest).toBeLessThan(240);
    // two birds never share a perch
    expect(r.stacked).toBe(0);
  });

  it('keeps swimmers in the water, facing the way they swim', () => {
    const state = createEngineState(false, 7);
    state.W = 1280; state.H = 720;
    const world = createWorld(state, { blip() {}, burst() {}, emit() {} });
    state.world = world;
    world.dl = 0.8;
    const pond = world.pondProp()!;
    const box = world.boxOf({ kind: 'prop', obj: pond });
    for (let s = 0; s < 1800; s++) {
      for (let i = 0; i < 30; i++) world.update(1 / 30);
      for (const e of world.entities) {
        if (e.layer !== 'pond' || e.state === 'jump') continue;
        expect(e.x).toBeGreaterThanOrEqual(box.x - 1);
        expect(e.x).toBeLessThanOrEqual(box.x + box.w + 1);
        if (e.vx !== 0 && e.state === 'walk' && !e.data.hop) expect(Math.sign(e.vx)).toBe(e.dir);
      }
    }
  });

  it('gives every ground prop its own patch of dry ground', () => {
    for (const seed of [1, 2, 3, 4, 5, 6, 7, 8]) {
      const state = createEngineState(false, seed);
      state.W = 1280; state.H = 720;
      const world = createWorld(state, { blip() {}, burst() {}, emit() {} });
      state.world = world;
      const pondBox = world.boxOf({ kind: 'prop', obj: world.pondProp()! });
      const ground = world.props.filter((p) => p.layer === 2
        && !['pond', 'oak', 'hive', 'cattails', 'mushroom', 'snowpile'].includes(p.key));
      for (const p of ground) {
        const half = (p.w as number) / 2;
        // not standing in the pond
        expect(p.x + half < pondBox.x || p.x - half > pondBox.x + pondBox.w, `${p.key} in the pond (seed ${seed})`).toBe(true);
        for (const q of ground) {
          if (q === p) continue;
          const gap = Math.abs(q.x - p.x) - half - (q.w as number) / 2;
          expect(gap, `${p.key} overlaps ${q.key} (seed ${seed})`).toBeGreaterThanOrEqual(0);
        }
      }
    }
  });

  it('walks over the terrain instead of through it', () => {
    // An animal stands on the highest ground under its body, nose to heels.
    // It used to follow the column under its middle, so it walked half-way
    // into a step before popping up onto it.
    const state = createEngineState(false, 11);
    state.W = 1280; state.H = 720;
    const world = createWorld(state, { blip() {}, burst() {}, emit() {} });
    state.world = world;
    world.dl = 0.8;
    for (const fromLeft of [true, false]) {
      const fox = world.spawn(SPECIES_BY_KEY.fox, { x: fromLeft ? 520 : 1180, fromLeft });
      fox.layer = 2;
      let worst = 0;
      for (let f = 0; f < 60 * 20; f++) {
        world.spawnTimer = 99; world.sinceLastSpawn = 0;   // just the fox
        world.update(1 / 60);
        if (fox.state !== 'walk' && fox.state !== 'idle') break;
        const feet = world.anchorOf(fox).gy;
        const sd = fox.sp.shape!;
        const front = fox.x + fox.dir * ((sd.bw as number) * 0.5 + (sd.hw as number) * 0.5) * fox.scale;
        const back = fox.x - fox.dir * (sd.bw as number) * 0.4 * fox.scale;
        const highest = Math.min(groundYWorld(state, fox.x, 2), groundYWorld(state, front, 2), groundYWorld(state, back, 2));
        worst = Math.max(worst, feet - highest);
      }
      // a few px of easing while it hops a 12px step; never a whole block deep
      expect(worst).toBeLessThan(5);
      world.despawn(fox);
    }
  });
});
