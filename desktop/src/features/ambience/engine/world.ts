/**
 * The living world engine core.
 *
 * WORLD owns entities, props, items, carrying, the spawner, the update/draw
 * state machine, and the interaction dispatch. Everything stores WORLD x
 * coordinates and transforms to screen only at draw/hit time (F7), so
 * entities, props and terrain share one transform and can never drift apart
 * under mouse parallax.
 *
 * The prototype reached its helper functions (findEgg, dropGroundItem,
 * playVoice, burst, blip) as globals; here they arrive bound through `opts`
 * and the GameApi so the method bodies stay the same.
 */

import type { GameApi } from './api';
import { chord, blip as sfxBlip } from './audio/sfx';
import { makeFindEgg, checkAchievements } from './eggs';
import { ITEMS } from './items';
import { buildProps, shakeProp } from './props';
import type { EngineState, AmbienceEvent } from './state';
import { regenerateTerrain } from './sky';
import { drawItemSprite, drawLilyPad, drawPropSprite, LILY_FRACTIONS, shader } from './sprites';
import { paraX, paraY, groundYWorld, screenOf, stepFor, waterSurfaceY, waterSpan } from './terrain';
import { SPECIES, SPECIES_BY_KEY, buildSpecies, drawBird, drawFlutter, drawFrog, drawQuad, P } from './species';
import type { Box, Entity, Palette, PerchSpot, Prop, Species, WorldState, WorldItem } from './types';

/**
 * How long a rainbow the WEATHER put up stays before it fades, in seconds.
 *
 * A rainbow used to be permanent: rain→sun spawned one and nothing ever took it
 * down again, so a single shower left an arc across the sky for the rest of the
 * session. A real one outlives the shower by a little and then it is gone. The
 * one exception is a rainbow the player asked for by hand — see `rainbowPinned`.
 */
const RAINBOW_SECONDS = 10;

/** World helpers the species/props handlers close over (prototype globals). */
export function reactState(e: Entity, fx: string, dur?: number): void {
  e.state = 'react'; e.fx = fx; e.t = 0; e.reactDur = dur || 1.2;
}

export function fleeState(e: Entity, mul?: number): void {
  e.state = 'flee';
  e.dir = e.x < 0 ? 1 : (e.x > 1 ? 1 : e.dir);
  e.vx = Math.abs(e.vx || 24) * (mul || 2.4) * (e.x > 0.5 ? -1 : 1) * (e.dir || 1) / Math.abs(e.dir || 1);
}

export function boltState(e: Entity): void {
  // a bird on a perch cannot run; it stays put
  if (e.perch) return;
  e.state = 'flee'; e.vx = (e.dir || (Math.random() < 0.5 ? 1 : -1)) * 140;
}

export function exitToward(e: Entity, tx: number): void {
  e.state = 'exit';
  e.dir = tx > e.x ? 1 : -1;
  e.vx = Math.abs(e.vx || 24) * e.dir * 1.6;
  e.data.exitTarget = tx;
}

export function dropGroundItem(state: EngineState, itemId: string, x: number, layer = 2, height = 0): void {
  if (!ITEMS[itemId]) return;
  const world = state.world as WorldState;
  // F7: items store a WORLD x; the draw/hit passes derive screen y via screenOf
  world.items.push({ id: itemId, x: x, y: 0, t: 0, layer: layer, h: height, vh: 0 });
  if (world.items.length > 16) world.items.shift();
}

export function playVoice(state: EngineState, v: { seq?: number[]; freqs?: number[]; d?: number; t?: OscillatorType; v?: number; gap?: number } | null | undefined): void {
  if (!v) return;
  if (v.seq) {
    v.seq.forEach((f, i) => {
      setTimeout(() => sfxBlip(state, f, v.d || 0.14, v.t || 'triangle', v.v || 0.08), i * (v.gap || 90));
    });
  } else chord(state, v.freqs || [440], v.gap || 70);
}

export interface WorldOpts {
  blip: (f: number, d?: number, t?: OscillatorType, v?: number) => void;
  burst: (x: number, y: number, color: string, n?: number, spread?: number) => void;
  emit: (ev: AmbienceEvent) => void;
}

let uidc = 1;

/** Hover/tap name tags: a small fixed-pitch face so they read as part of the pixel world. */
const TAG_FONT = 'bold 10px ui-monospace, "Cascadia Mono", Consolas, monospace';

/** How many pond dwellers and perched birds the world keeps at once. */
const MAX_POND = 3;
const MAX_PERCHED = 3;
/** Wanderers crossing the scene; the watchdog may go a little past this. */
const MAX_WALKERS = 9;

/**
 * Build the whole living world: the WORLD object, the species catalogue, the
 * fixed props. Called once by the engine when the canvas is sized.
 */
export function createWorld(state: EngineState, opts: WorldOpts): WorldState {
  /** Daylight shader for the frame being drawn (see sprites.ts). */
  let sh: (c: string) => string = (c) => c;

  const shadePal = (pal: Palette): Palette => ({
    body: sh(pal.body),
    accent: pal.accent ? sh(pal.accent) : undefined,
    dark: sh(pal.dark),
    light: pal.light ? sh(pal.light) : undefined,
    mid: pal.mid ? sh(pal.mid) : undefined,
    belly: pal.belly ? sh(pal.belly) : undefined,
    eye: pal.eye,
    beak: pal.beak ? sh(pal.beak) : undefined,
    head: pal.head ? sh(pal.head) : undefined,
  });

  const layerScale = (layer: number | 'sky' | 'pond'): number =>
    layer === 0 ? 0.55 : layer === 1 ? 0.75 : layer === 2 ? 1 : layer === 'sky' ? 0.85 : 1;

  /** Is the layer-2 ground at this world x under the pond's water? */
  const submerged = (wx: number): boolean => {
    const b = state.BASIN;
    if (!b || !state.R3) return false;
    const i = Math.floor((wx + state.OVER) / 12);
    if (i < b.c - b.half || i > b.c + b.half || i < 0 || i >= state.R3.length) return false;
    return state.R3[i] > state.WATER_LEVEL;
  };

  /**
   * The ground a walking animal stands on, in world y (no parallax).
   *
   * An animal is wider than a terrain column, so it stands on the HIGHEST
   * ground under its body, from its heels to its nose. Sampling only the
   * column under its middle meant it kept walking at the old height until
   * half of it was inside the next block, then popped up — and since the head
   * leads, climbing looked worse than descending.
   */
  const footing = (e: Entity): number => {
    const layer = e.layer;
    const here = groundYWorld(state, e.x, layer);
    if (e.sp.kind !== 'quad' || e.perch) return here;
    const sd = e.sp.shape || {};
    const dir = (e.dir || 1) >= 0 ? 1 : -1;
    const back = e.x - dir * (sd.bw || 16) * 0.45 * e.scale;
    // look a little ahead of the nose, further the faster it is going, so the
    // hop onto a step has finished by the time the animal reaches it
    const lead = e.state === 'idle' || e.state === 'react' || e.state === 'sleep' ? 0 : Math.abs(e.vx || 0) * 0.12;
    const front = e.x + dir * (((sd.bw || 16) * 0.5 + (sd.hw || 8) * 0.55) * e.scale + lead);
    // every column the body covers, not just its ends: a one-column bump
    // between two samples would otherwise be walked straight through
    const lo = Math.min(back, front), hi = Math.max(back, front), st = stepFor(layer);
    let top = Math.min(here, groundYWorld(state, hi, layer));
    for (let wx = lo; wx < hi; wx += st) top = Math.min(top, groundYWorld(state, wx, layer));
    return top;
  };

  /** World x of each lily pad's centre. */
  const lilyXs = (): number[] => {
    const span = waterSpan(state);
    const width = Math.max(0, span.x1 + 12 - span.x0 - 14);
    return LILY_FRACTIONS.map((f) => span.x0 + 7 + width * f);
  };

  /** A fixed prop's drawn foot position. A hanging prop stands on its host's ground. */
  const propAnchor = (p: Prop): { sx: number; gy: number } => {
    const layer = p.layer;
    const sx = p.x + paraX(state, layer);
    if (layer === 'sky') return { sx, gy: p.y !== undefined ? p.y : state.H * 0.16 };
    if (layer === 'pond') return { sx, gy: waterSurfaceY(state) };
    const host = p.host as Prop | undefined;
    const gy = p.y !== undefined ? p.y + paraY(state, layer) : groundYWorld(state, host ? host.x : p.x, layer) + paraY(state, layer);
    return { sx, gy };
  };

  /** Where an item is drawn: on its layer's ground, less however far it still has to fall. */
  const itemPos = (it: WorldItem): { x: number; y: number; k: number } => {
    const L = it.layer === undefined ? 2 : it.layer;
    const s = screenOf(state, it.x, L);
    return { x: s.x, y: s.y - (it.h || 0), k: layerScale(L) };
  };

  /** Every seat a bird can take, derived from the props that have one. */
  const perchSpots = (): PerchSpot[] => {
    const out: PerchSpot[] = [];
    world.props.forEach((p, i) => {
      if (p.key === 'oak') {
        // the left of the trunk: the hive hangs on the right
        out.push({ id: i + ':trunk', prop: p, dx: -7, dy: 16, for: ['woodpecker'] });
        out.push({ id: i + ':top', prop: p, dx: -6, dy: 69, for: ['crow', 'owl'] });
      } else if (p.key === 'pine') {
        out.push({ id: i + ':top', prop: p, dx: 0, dy: 75, for: ['owl'] });
        out.push({ id: i + ':bough', prop: p, dx: 10, dy: 35, for: ['crow'] });
      } else if (p.key === 'scarecrow') {
        out.push({ id: i + ':arm', prop: p, dx: 13, dy: 29, for: ['crow'] });
        out.push({ id: i + ':hat', prop: p, dx: 0, dy: 44, for: ['crow', 'owl'] });
      }
    });
    return out;
  };

  const world: WorldState = {
    t: 0, entities: [], props: [], items: [], carry: null, flags: {}, found: {}, hover: null, hoverIsProp: false,
    dl: 0.5, spawnTimer: 3, sinceLastSpawn: 0, worldFx: [], bits: [],
    layerScale,
    isNight: function () { return this.dl < 0.35; },
    isDay: function () { return this.dl >= 0.35; },
    eligible: function (sp) {
      if (sp.t === 'D' && !this.isDay()) return false;
      if (sp.t === 'N' && !this.isNight()) return false;
      if (sp.gate && !sp.gate()) return false;
      return true;
    },
    freePerches: function (key) {
      const taken: Record<string, boolean> = {};
      this.entities.forEach(function (e) { if (e.perch) taken[e.perch.id] = true; });
      return perchSpots().filter(function (s) { return s.for.indexOf(key) >= 0 && !taken[s.id]; });
    },
    spawn: function (sp, opts2) {
      opts2 = opts2 || {};
      const fromLeft = opts2.fromLeft !== undefined ? opts2.fromLeft : Math.random() < 0.5;
      // F6: species may declare a layerRange so the same animal appears at
      // varying depths (and sizes) across spawns
      const layer = sp.layerRange
        ? sp.layerRange[0] + Math.floor(Math.random() * (sp.layerRange[1] - sp.layerRange[0] + 1))
        : sp.layer;
      const scale = layerScale(layer) * (sp.size || 1);
      const data: Record<string, unknown> = opts2.data || {};
      if (sp.variants && sp.variants.length) data.pal = sp.variants[Math.floor(Math.random() * sp.variants.length)];
      const e: Entity = {
        sp: sp, layer: layer, dir: fromLeft ? 1 : -1, vx: (sp.speed || 18) * (fromLeft ? 1 : -1),
        scale: scale, state: 'walk', t: 0, stateT: 0, phase: Math.random() * 6.28, fx: null, data: data,
        hb: { w: 26 * scale, h: 22 * scale }, removeAt: null, x: 0, y: 0,
      };
      const fly = (x: number, y: number): void => {
        e.layer = 'sky'; e.scale = layerScale('sky') * (sp.size || 1);
        e.x = x; e.y = y;
        e.vx = Math.max(sp.speed || 0, 46) * (fromLeft ? 1 : -1);
      };
      if (opts2.airborne) {
        // a startled bird: already in the air where it was flushed from, climbing away
        fly(opts2.airborne.x - paraX(state, 'sky'), opts2.airborne.y);
        e.vy = -(40 + Math.random() * 40);
        e.data.climbTo = state.H * (0.08 + Math.random() * 0.2);
      } else if (sp.perch) {
        // Perching birds take a seat of their own. Two birds never share one:
        // they used to pile onto the same spot beside a tree and read as a
        // single many-winged creature.
        const free = this.freePerches(sp.key);
        let spot: PerchSpot | undefined;
        if (free.length) {
          if (opts2.x !== undefined) {
            const want = opts2.x;
            spot = free.slice().sort(function (a, b) { return Math.abs(a.prop.x - want) - Math.abs(b.prop.x - want); })[0];
          } else spot = free[Math.floor(Math.random() * free.length)];
        }
        if (spot) {
          const ps = layerScale(spot.prop.layer);
          e.perch = spot; e.layer = spot.prop.layer; e.scale = ps * (sp.size || 1) * 0.9;
          e.x = spot.prop.x + spot.dx * ps; e.vx = 0;
          // a bird on a trunk faces the bark; one out on a limb faces outward
          if (sp.key === 'woodpecker') e.dir = spot.dx > 0 ? -1 : 1;
          else if (spot.dx !== 0) e.dir = spot.dx > 0 ? 1 : -1;
          e.residentUntil = this.t + 30 + Math.random() * 40;
        } else {
          // nowhere to sit: it simply flies through
          fly(opts2.x !== undefined ? opts2.x : (fromLeft ? -60 : state.W + 60), state.H * (0.12 + Math.random() * 0.2));
        }
      } else if (layer === 'sky') {
        e.x = fromLeft ? -60 : state.W + 60; e.y = state.H * (0.08 + Math.random() * 0.28);
        e.vx = (sp.speed || 24) * (fromLeft ? 1 : -1);
      } else if (layer === 'pond') {
        const pond = this.pondProp();
        const pr = pond ? (pond.rw as number) * 0.7 : 120;
        const px0 = pond ? pond.x : state.W * 0.30;
        const span = waterSpan(state);
        // F9: pond creatures spawn inside the actual water, not a fixed ellipse
        const pad = Math.min(16, (span.x1 - span.x0) / 4);
        e.x = span ? (span.x0 + pad + Math.random() * Math.max(1, span.x1 - span.x0 - pad * 2)) : (px0 + (Math.random() * 2 - 1) * pr);
        e.pondCX = px0; e.pondR = pr;
        e.vx = (sp.speed || 8) * (fromLeft ? 1 : -1); e.y = 0;
        e.data.fadeIn = true;
        if (sp.style === 'frog') {
          // frogs sit on a lily pad and hop between them
          const pads = lilyXs();
          const used = this.entities.filter(function (o) { return o.sp.style === 'frog'; }).map(function (o) { return o.data.pad as number; });
          const open = pads.map(function (_x, i) { return i; }).filter(function (i) { return used.indexOf(i) < 0; });
          const pick = open.length ? open[Math.floor(Math.random() * open.length)] : Math.floor(Math.random() * pads.length);
          e.data.pad = pick; e.x = pads[pick]; e.vx = 0;
          e.data.nextHop = 4 + Math.random() * 6;
        }
        e.residentUntil = this.t + 25 + Math.random() * 45;
      } else {
        e.x = fromLeft ? -60 : state.W + 60;
        e.y = 0;
      }
      if (opts2.x !== undefined && !e.perch && !opts2.airborne) {
        e.x = opts2.x;
        if (sp.style === 'frog') e.data.pad = -1;
      }
      this.entities.push(e);
      return e;
    },
    pondProp: function () {
      if (!this._pond) this._pond = this.props.filter(function (p) { return p.key === 'pond'; })[0];
      return this._pond;
    },
    despawn: function (e) { const i = this.entities.indexOf(e); if (i >= 0) this.entities.splice(i, 1); },
    anchorOf: function (e) {
      const sp = e.sp, scale = e.scale;
      if (e.perch) {
        const p = e.perch.prop, ps = layerScale(p.layer), a = propAnchor(p);
        const shk = (!state.reduce && (p.shakeT as number)) ? Math.sin((p.shakeT as number) * 40) * 3 * (p.shakeT as number) : 0;
        const cy = a.gy - e.perch.dy * ps;
        return { sx: a.sx + e.perch.dx * ps + shk, gy: cy, cy: cy, swim: false };
      }
      if (e.layer === 'sky') {
        const sx = e.x + paraX(state, 'sky');
        return { sx: sx, gy: e.y, cy: e.y, swim: false };
      }
      const wy = waterSurfaceY(state);
      if (e.layer === 'pond') {
        const sx = e.x + paraX(state, 'pond');
        if (e.state === 'jump') { const jy = e.jy || wy; return { sx: sx, gy: jy, cy: jy, swim: false }; }
        const sink = e.state === 'dive' ? e.t * 14 : 0;
        if (sp.kind === 'bird') return { sx: sx, gy: wy, cy: wy - 3 * scale + sink, swim: true };
        if (sp.kind === 'flutter') return { sx: sx, gy: wy, cy: wy - 14 * scale, swim: false };
        if (sp.style === 'frog') {
          const hop = e.data.hop as { k: number } | undefined;
          const lift = hop ? Math.sin(Math.PI * hop.k) * 16 : 0;
          const gy = wy - 2 - lift + sink;
          return { sx: sx, gy: gy, cy: gy - 5 * scale, swim: false };
        }
        // an otter (or anything else on four legs) swims low in the water
        const sh2 = sp.shape || {};
        const gy = wy + ((sh2.lh || 6) + (sh2.bh || 10) * 0.5) * scale + sink;
        return { sx: sx, gy: gy, cy: wy - 3 * scale + sink, swim: true };
      }
      const sx = e.x + paraX(state, e.layer);
      const eased = e.data.g as number | undefined;
      const ground = (eased !== undefined && !state.reduce ? eased : footing(e)) + paraY(state, e.layer);
      const wet = e.layer === 2 && submerged(e.x);
      if (sp.kind === 'flutter') { const base = wet ? wy : ground; return { sx: sx, gy: base, cy: base - 14 * scale, swim: false }; }
      if (sp.kind === 'glow') {
        const base = wet ? wy : ground;
        const hover = e.data.isSwarm ? 26 : 12 + Math.sin(this.t * 1.3 + e.phase) * 5;
        return { sx: sx, gy: base, cy: base - hover, swim: false };
      }
      if (wet && sp.kind === 'quad') {
        // wading: the pond is deeper than most animals are tall, so they swim
        // across it at the surface instead of walking along the bottom
        const sh2 = sp.shape || {};
        const gy = wy + ((sh2.lh || 6) + (sh2.bh || 10) * 0.45) * scale;
        return { sx: sx, gy: gy, cy: wy - e.hb.h * 0.25, swim: true };
      }
      return { sx: sx, gy: ground, cy: ground - e.hb.h * 0.5, swim: false };
    },
    boxOf: function (hit) {
      if (hit.kind === 'item') {
        const ip = itemPos(hit.obj as WorldItem);
        return { x: ip.x - 12 * ip.k, y: ip.y - 20 * ip.k, w: 24 * ip.k, h: 26 * ip.k };
      }
      if (hit.kind === 'entity') {
        const e = hit.obj as Entity, a = this.anchorOf(e), m = 6;
        return { x: a.sx - e.hb.w / 2 - m, y: a.cy - e.hb.h / 2 - m, w: e.hb.w + m * 2, h: e.hb.h + m * 2 };
      }
      const p = hit.obj as Prop;
      if (p.key === 'pond') {
        // The pond is the water itself: from the surface down to the deepest
        // part of the basin, across the submerged columns. Both the highlight
        // and the click test read this one box, so they cannot disagree.
        const span = waterSpan(state), wy = waterSurfaceY(state);
        let deep = wy + 8;
        const b = state.BASIN;
        if (b && state.R3) {
          for (let i = b.c - b.half; i <= b.c + b.half; i++) {
            if (i >= 0 && i < state.R3.length) deep = Math.max(deep, state.R3[i] + paraY(state, 2));
          }
        }
        return { x: span.x0 + paraX(state, 2), y: wy, w: span.x1 + 12 - span.x0, h: deep - wy };
      }
      const a = propAnchor(p);
      const s = p.layer === 'sky' || p.layer === 'pond' ? 1 : layerScale(p.layer);
      const w = (p.hb ? p.hb.w : 30) * s, h = (p.hb ? p.hb.h : 30) * s, lift = ((p.lift as number) || 0) * s;
      return { x: a.sx + ((p.boxDx as number) || 0) * s - w / 2 - 6, y: a.gy - lift - h - 6, w: w + 12, h: h + 12 };
    },
    hit: function (px, py) {
      const inside = (b: Box): boolean => px > b.x && px < b.x + b.w && py > b.y && py < b.y + b.h;
      for (let k = this.items.length - 1; k >= 0; k--) {
        const it = this.items[k];
        if (inside(this.boxOf({ kind: 'item', obj: it }))) return { kind: 'item' as const, obj: it };
      }
      for (let i = this.entities.length - 1; i >= 0; i--) {
        const e = this.entities[i]; if (e.state === 'gone' || e.state === 'dive') continue;
        if (inside(this.boxOf({ kind: 'entity', obj: e }))) return { kind: 'entity' as const, obj: e };
      }
      for (let j = this.props.length - 1; j >= 0; j--) {
        const p = this.props[j]; if (p.visibleIf && !p.visibleIf()) continue;
        if (!inside(this.boxOf({ kind: 'prop', obj: p }))) continue;
        // only the wet columns of the basin are pond; the banks are just ground
        if (p.key === 'pond' && !submerged(px - paraX(state, 2))) continue;
        return { kind: 'prop' as const, obj: p };
      }
      return null;
    },
    nearestProp: function (x, key) {
      let best: Prop | null = null, bd = 1e9;
      this.props.forEach(function (p) {
        if (p.key !== key) return;
        const d = Math.abs(p.x - x); if (d < bd) { bd = d; best = p; }
      });
      return best;
    },
    nearPond: function (x) { const pd = this.pondProp(); return !!pd && Math.abs(x - pd.x) < (pd.rw as number); },
    pet: function (species) {
      const petted = (this.flags.petted = (this.flags.petted as Record<string, boolean>) || {});
      petted[species] = true;
      if (Object.keys(petted).length >= 5) this.emit?.({ type: 'achievement', icon: '🤲', name: 'Gentle Hand' });
    },
    dropCarry: function () {
      this.carry = null;
      this.emit?.({ type: 'drop-carry' });
    },
    pickUp: function (itemId, x, y) {
      if (this.carry) { this.items.push({ id: this.carry, x: x, y: y, t: 0 }); }
      this.carry = itemId;
      const carried = (this.flags.carriedTypes = (this.flags.carriedTypes as Record<string, boolean>) || {});
      carried[itemId] = true;
      if (Object.keys(carried).length >= 14) this.emit?.({ type: 'achievement', icon: '🎒', name: 'Collector' });
      // carry tag (prototype's #carryTag) is the React shell's job via the event
      const seen = (this.flags.seenTag = (this.flags.seenTag as Record<string, boolean>) || {});
      const firstTime = !seen[itemId];
      seen[itemId] = true;
      this.lastPickup = { itemId: itemId, name: ITEMS[itemId] ? ITEMS[itemId].name : itemId, firstTime: firstTime, at: this.t };
      this.emit?.({ type: 'carry', itemId: itemId, name: ITEMS[itemId] ? ITEMS[itemId].name : itemId, firstTime: firstTime });
      opts.blip(760, 0.09);
    },
    spawnFish: function (pond) {
      const waterY = waterSurfaceY(state);
      const f: Entity = {
        sp: SPECIES_BY_KEY.fish, layer: 'pond', x: pond.x + (Math.random() * 80 - 40),
        y: 0, jy: waterY, vy: -330, waterY: waterY, vx: (Math.random() < 0.5 ? -1 : 1) * 26,
        dir: 1, scale: 1.4, state: 'jump', t: 0, stateT: 0, fx: null, hb: { w: 26, h: 22 }, data: {}, phase: 0, removeAt: null,
      };
      this.entities.push(f); this.flags.fishJumping = f;
      this.splash(f.x + paraX(state, 'pond'), waterY);
      if ((this.flags.pondClicks as number) >= 2 && this.flags.truffleThrown) this._findEgg?.('truffle-pond');
      return f;
    },
    splash: function (x, y) {
      opts.blip(300, 0.18, 'sine', 0.05);
      this.puff(x, y, '#BFE6FF', 9, { spread: 46, up: 70, g: 260, life: 0.7 });
    },
    puff: function (x, y, color, n, o) {
      if (state.reduce) return;
      const spread = (o && o.spread) || 30, up = (o && o.up) || 30, g = o && o.g !== undefined ? o.g : 120;
      const life = (o && o.life) || 0.9, size = (o && o.size) || 2;
      for (let i = 0; i < (n || 6); i++) {
        const l = life * (0.6 + Math.random() * 0.6);
        this.bits.push({
          x: x + (Math.random() - 0.5) * 6, y: y, vx: (Math.random() - 0.5) * spread, vy: -up * (0.4 + Math.random() * 0.8),
          g: g, life: l, max: l, size: size, color: color,
        });
      }
      if (this.bits.length > 160) this.bits.splice(0, this.bits.length - 160);
    },
    takeOff: function (e) {
      const a = this.anchorOf(e), wasPond = e.layer === 'pond';
      e.perch = undefined; e.layer = 'sky';
      e.x = a.sx - paraX(state, 'sky'); e.y = a.cy;
      e.dir = Math.random() < 0.5 ? -1 : 1;
      e.vx = e.dir * Math.max(46, (e.sp.speed || 0) * 1.6);
      e.vy = -(36 + Math.random() * 30);
      e.data.climbTo = state.H * (0.08 + Math.random() * 0.22);
      e.state = 'walk'; e.fx = null; e.residentUntil = undefined;
      if (wasPond) this.puff(a.sx, a.gy, '#BFE6FF', 5, { spread: 30, up: 40, g: 220, life: 0.5 });
    },
    spawnRainbow: function () {
      if (this.flags.rainbowUp) return; this.flags.rainbowUp = true; this.flags.rainbowEnds = {}; this.flags.rainbowT = 0;
      // Wall-clock deadline alongside the frame-counted `rainbowT`: frames can
      // stall far below real time (occluded window, heavy page, tab hidden),
      // and a shower's rainbow that outlives the shower by MINUTES reads as
      // stuck. The pinned branch ignores this deadline on purpose.
      this.flags.rainbowExpiresAt = Date.now() + RAINBOW_SECONDS * 1000;
      const lx = state.W * 0.22, rx = state.W * 0.78;
      // F-rainbow: anchor the arc's ends to the terrain so the rainbow reads
      // as rising from BEHIND the hills, not floating high in the sky. The
      // draw uses `gy` (= this prop's y) as the arc's baseline; placing both
      // ends at the lower of the two ground heights tucks them behind the
      // nearest ridge.
      const gy = Math.max(groundYWorld(state, lx, 2), groundYWorld(state, rx, 2));
      this.props.push({
        // fx/pairFx are the authority: absolute x is derived from the CURRENT
        // width so the arc re-spans the viewport on resize instead of staying
        // frozen at whatever the window was when it appeared.
        key: 'rainbow-end', side: 'l', name: 'Rainbow (left end)', layer: 'sky',
        x: lx, y: gy, pairX: rx, fx: 0.22, pairFx: 0.78, w: 16, h: 16, hb: { w: 30, h: 36 },
        reaction: function () {
          (world.flags.rainbowEnds as Record<string, boolean>).l = true; opts.blip(700, 0.15); world.checkRainbow();
        },
      });
      this.props.push({
        key: 'rainbow-end', side: 'r', name: 'Rainbow (right end)', layer: 'sky',
        x: rx, y: gy, pairX: lx, fx: 0.78, pairFx: 0.22, w: 16, h: 16, hb: { w: 30, h: 36 },
        reaction: function () {
          (world.flags.rainbowEnds as Record<string, boolean>).r = true; opts.blip(760, 0.15); world.checkRainbow();
        },
      });
    },
    checkRainbow: function () {
      const ends = this.flags.rainbowEnds as Record<string, boolean> | undefined;
      if (ends && ends.l && ends.r && !this.found['rainbow']) {
        this._findEgg?.('rainbow');
        dropGroundItem(state, 'coin', state.W * 0.5);
        this.clearRainbow();
      }
    },
    clearRainbow: function () {
      this.flags.rainbowUp = false;
      this.flags.rainbowT = 0;
      this.flags.rainbowExpiresAt = 0;
      this.props = this.props.filter(function (p) { return p.key !== 'rainbow-end'; });
    },
    spawnSwarm: function () {
      const s: Entity = {
        sp: SWARM_SP, layer: 2, x: state.W * 0.5, y: 0, vx: 0, dir: 1, scale: 2,
        state: 'idle', fx: null, t: 0, stateT: 9999, hb: { w: 60, h: 30 }, data: { isSwarm: true }, phase: 0, removeAt: null,
      };
      this.entities.push(s);
    },
    campfireClick: function (e, key) {
      reactState(e, 'happy', 1);
      playVoice(state, { freqs: [600, 760, 900], gap: 80 });
      const got = (this.flags.campfireClicked = (this.flags.campfireClicked as Record<string, boolean>) || {});
      got[key] = true;
      if (got.fox && got.rabbit && got.deer) this._findEgg?.('campfire-tales');
    },
    startCampfireTales: function (fire) {
      const order = ['fox', 'rabbit', 'deer'];
      this.flags.campfireGathered = [];
      order.forEach((key, i) => {
        setTimeout(() => {
          const side = i - 1;
          const e = this.spawn(SPECIES_BY_KEY[key], { x: fire.x + side * 40, fromLeft: side <= 0 });
          // They sit facing the fire for a while, then wander off on their own.
          e.layer = 2; e.scale = layerScale(2) * (e.sp.size || 1);
          e.state = 'idle'; e.stateT = 50 + i * 4; e.data.atCampfire = true; e.vx = 0;
        }, i * 1600);
      });
    },
    ringHit: function (idx) {
      const hits = (this.flags.ringHits = (this.flags.ringHits as Record<string, number>) || {});
      hits[idx] = this.t;
      const keys = Object.keys(hits), self = this;
      const recent = keys.filter(function (k) { return self.t - (hits[k] as number) < 6; });
      if (recent.length >= 7) this._findEgg?.('fairy-ring');
    },
    update: function (dt) {
      this.t += dt;
      // The rainbow is a shower's parting gift, not scenery: one the weather put
      // up fades after RAINBOW_SECONDS. `rainbowPinned` is the player asking for
      // one by hand, and that one stays — including after the egg consumes the
      // arc, which is why this re-spawns rather than only counting down.
      if (this.flags.rainbowPinned) {
        if (!this.flags.rainbowUp) this.spawnRainbow();
      } else if (this.flags.rainbowUp) {
        // Expire on wall-clock time too (see spawnRainbow): the frame-counted
        // age below assumes frames arrive at real-time pace, which an occluded
        // or loaded window does not guarantee.
        const expiresAt = this.flags.rainbowExpiresAt as number | undefined;
        if (expiresAt !== undefined && expiresAt > 0 && Date.now() >= expiresAt) {
          this.clearRainbow();
        } else {
          const age = ((this.flags.rainbowT as number) || 0) + dt;
          this.flags.rainbowT = age;
          if (age >= RAINBOW_SECONDS) this.clearRainbow();
        }
      }
      // The sun/moon and the clickable cloud are drawn by the sky pass; keep
      // their invisible hit targets on top of what is actually on screen.
      for (let pi = 0; pi < this.props.length; pi++) {
        const p = this.props[pi];
        if (p.key === 'sunmoon') {
          p.x = state.W * (1 - state.tod);
          p.y = state.H * 0.72 - Math.sin(state.tod * Math.PI) * state.H * 0.6 + 17;
        } else if (p.key === 'clouds') {
          const cw = 224;
          p.x = ((state.lastTs / 38 + 2 * 260) % (state.W + 420)) - 210 + cw / 2;
          p.y = 74 + 17 + (state.my - 0.5) * -18;
        }
      }
      if (state.reduce) { return; } // props stay static, no wanderers, no ambient motion
      const TIMERFIELDS = ['shakeT', 'ripple', 'blink', 'spin', 'rainT', 'beetleT', 'antT'];
      this.props.forEach(function (p) {
        TIMERFIELDS.forEach(function (f) { if ((p[f] as number) > 0) p[f] = Math.max(0, (p[f] as number) - dt); });
        if (p.rolled && (p.rollT as number) < 1) p.rollT = Math.min(1, (p.rollT as number) + dt * 1.5);
      });

      // ── spawner ──────────────────────────────────────────────────────
      // Wanderers and residents are budgeted separately. The world used to
      // count them together and "evict" a resident whenever nothing had spawned
      // for a while — by clearing the very timestamp that makes a resident
      // leave. Each eviction therefore turned a duck or a perched owl immortal,
      // and once nine of them had piled up no animal ever walked in again.
      this.sinceLastSpawn = (this.sinceLastSpawn || 0) + dt;
      this.spawnTimer -= dt;
      const overdue = this.sinceLastSpawn > 20;
      if ((this.spawnTimer <= 0 || overdue) && this.entities.length < 22) {
        this.spawnTimer = 4 + Math.random() * 5;
        if (overdue) this.sinceLastSpawn = 12;
        const self = this;
        let walkers = 0, ponders = 0, perched = 0;
        const byKey: Record<string, number> = {};
        this.entities.forEach(function (e) {
          byKey[e.sp.key] = (byKey[e.sp.key] || 0) + 1;
          if (e.layer === 'pond') ponders++;
          else if (e.perch) perched++;
          else if (e.state !== 'sleep') walkers++;
        });
        const span = waterSpan(state);
        const room = function (sp: Species): boolean {
          if (sp.layer === 'pond') {
            if (ponders >= MAX_POND || span.x1 - span.x0 < 24) return false;
            return (byKey[sp.key] || 0) < (sp.style === 'frog' || sp.style === 'dragonfly' ? 2 : 1);
          }
          if (sp.perch) return perched < MAX_PERCHED && self.freePerches(sp.key).length > 0;
          return walkers < MAX_WALKERS + (overdue ? 3 : 0);
        };
        // F11: gated species (w:0 + gate) spawn directly — the cat can finally appear
        const special = SPECIES.filter(function (sp2) { return sp2.w === 0 && sp2.gate && sp2.gate(); });
        let made = false;
        if (special.length && !byKey[special[0].key]) {
          this.spawn(special[0], {}); made = true;
        } else {
          const pool = SPECIES.filter(function (sp2) { return sp2.w > 0 && self.eligible(sp2) && room(sp2); });
          const totalW = pool.reduce(function (s, sp) { return s + sp.w; }, 0);
          if (totalW > 0) {
            let r = Math.random() * totalW, sp = pool[0];
            for (let i = 0; i < pool.length; i++) { r -= pool[i].w; if (r <= 0) { sp = pool[i]; break; } }
            made = true;
            if (sp.group) {
              const lead = this.spawn(sp, {});
              const members: Entity[] = [lead];
              for (let g = 0; g < 4 + Math.floor(Math.random() * 3); g++) {
                // a loose V behind the leader
                const m = this.spawn(sp, { x: lead.x - (g + 1) * 22 * lead.dir, fromLeft: lead.dir > 0 });
                m.y = lead.y + (g % 2 ? 1 : -1) * Math.ceil((g + 1) / 2) * 9;
                m.phase = lead.phase + g * 0.7;
                m.groupOffset = (g + 1);
                members.push(m);
              }
              members.forEach(function (m) { m.groupMembers = members; });
            } else {
              const e = this.spawn(sp, {});
              if (sp.key === 'duck') {
                e.data.ducklings = [];
                for (let d = 0; d < 3; d++) (e.data.ducklings as Array<{ ox: number; oy: number }>).push({ ox: -(d + 1) * 10, oy: 0 });
              }
            }
          }
        }
        if (made) this.sinceLastSpawn = 0;
      }

      // ── entities ─────────────────────────────────────────────────────
      const ws = waterSpan(state);
      for (let idx = this.entities.length - 1; idx >= 0; idx--) {
        const e = this.entities[idx]; e.t += dt; e.uid = e.uid || ('e' + (uidc++)); e.data = e.data || {};
        const sp = e.sp;
        // Ease onto the ground under it, so a step is a quick hop up or down
        // and not a teleport. Fast enough that it never lags into the block.
        if (typeof e.layer === 'number' && !e.perch) {
          const want = footing(e), cur = e.data.g as number | undefined;
          // up quickly, down a touch more gently
          e.data.g = cur === undefined || Math.abs(want - cur) > 60 ? want : cur + (want - cur) * Math.min(1, dt * (want < cur ? 30 : 18));
        }
        // a climbing bird keeps climbing whatever else it is doing
        if (e.layer === 'sky' && e.vy) {
          e.y += e.vy * dt;
          const top = e.data.climbTo as number | undefined;
          if (top === undefined || e.y <= top) { e.vy = 0; e.data.climbTo = undefined; }
        }
        // a frog mid-hop finishes its hop
        const hop = e.data.hop as { from: number; to: number; k: number; dur: number } | undefined;
        if (hop) {
          hop.k += dt / hop.dur;
          e.x = hop.from + (hop.to - hop.from) * Math.min(1, hop.k);
          if (hop.k >= 1) {
            e.data.hop = undefined;
            const a = this.anchorOf(e);
            this.puff(a.sx, waterSurfaceY(state), '#BFE6FF', 3, { spread: 20, up: 24, g: 200, life: 0.4 });
          }
        }
        if (e.state === 'react') {
          if (sp.style === 'frog' && e.data.jumpTo !== undefined && !e.data.hop) {
            // a poked frog leaps to the lily pad nearest where it was headed
            const pads = lilyXs(), want = e.data.jumpTo as number;
            let best = 0;
            pads.forEach(function (px, i) { if (Math.abs(px - want) < Math.abs(pads[best] - want)) best = i; });
            e.data.jumpTo = undefined;
            e.dir = pads[best] >= e.x ? 1 : -1;
            e.data.pad = best;
            e.data.hop = { from: e.x, to: pads[best], k: 0, dur: 0.55 };
          }
          if (e.t >= (e.reactDur || 1.2)) {
            if (e.fx === 'curl') { e.state = 'sleep'; e.sleepStart = this.t; e.sleepFor = 30 + Math.random() * 30; }
            else { e.state = 'walk'; e.fx = null; }
          }
        } else if (e.state === 'sleep') {
          // F8a: sleeping animals wake and wander off — nothing is terminal
          if (this.t - (e.sleepStart || 0) > (e.sleepFor || 30)) { e.state = 'walk'; e.fx = null; e.sleepFor = 0; }
        } else if (e.state === 'walk') {
          e.phase += dt * 6;
          if (e.perch) {
            // sitting still
          } else if (e.layer === 'sky') {
            e.x += e.vx * dt; e.y += Math.sin(e.t * 1.4 + e.phase) * 6 * dt * 10 * 0.02;
          } else if (e.layer === 'pond') {
            if (sp.style === 'frog') {
              e.data.nextHop = ((e.data.nextHop as number) || 0) - dt;
              if ((e.data.nextHop as number) <= 0 && !e.data.hop) {
                e.data.nextHop = 5 + Math.random() * 8;
                const pads = lilyXs();
                const taken = this.entities.filter(function (o) { return o !== e && o.sp.style === 'frog'; }).map(function (o) { return o.data.pad as number; });
                const open = pads.map(function (_x, i) { return i; }).filter(function (i) { return i !== e.data.pad && taken.indexOf(i) < 0; });
                if (open.length) {
                  const to = open[Math.floor(Math.random() * open.length)];
                  e.dir = pads[to] >= e.x ? 1 : -1;
                  e.data.pad = to;
                  e.data.hop = { from: e.x, to: pads[to], k: 0, dur: 0.6 };
                }
              }
            } else {
              // Swimmers stay in the water and face the way they are going: they
              // used to keep their spawn heading and paddle backwards after the
              // first turn at the bank.
              e.x += e.vx * dt * 0.4;
              const pad = Math.min(14, (ws.x1 - ws.x0) / 4);
              if (e.x > ws.x1 + 6 - pad) { e.x = ws.x1 + 6 - pad; e.vx = -Math.abs(e.vx || 8); }
              if (e.x < ws.x0 + pad) { e.x = ws.x0 + pad; e.vx = Math.abs(e.vx || 8); }
              e.dir = e.vx >= 0 ? 1 : -1;
            }
          } else {
            e.x += e.vx * dt;
            if (e.vx) e.dir = e.vx > 0 ? 1 : -1;
            if (sp.kind === 'quad' && Math.random() < dt * 0.15) { e.state = 'idle'; e.stateT = 0.6 + Math.random() * 1.4; }
          }
          if (sp.behave) sp.behave(e, dt);
        } else if (e.state === 'idle') {
          if (e.layer === 'sky') e.phase += dt * 6;   // a hovering bird still beats its wings
          e.stateT -= dt;
          if (e.stateT <= 0) {
            e.state = 'walk';
            if (e.data.atCampfire) {
              // the gathering breaks up: each guest heads for the nearer edge
              e.data.atCampfire = false;
              e.dir = e.x < state.W / 2 ? -1 : 1;
              e.vx = (sp.speed || 18) * e.dir;
            }
          }
        } else if (e.state === 'flee') {
          e.x += e.vx * dt; e.phase += dt * 12;
          if (e.vx) e.dir = e.vx > 0 ? 1 : -1;
        } else if (e.state === 'jump') { // F3: fish arc — gravity, then splash back down
          e.vy = (e.vy || 0) + 900 * dt; e.jy = (e.jy || 0) + (e.vy || 0) * dt; e.x += e.vx * dt;
          if ((e.jy || 0) >= (e.waterY || 0) && (e.vy || 0) > 0) {
            this.splash(e.x + paraX(state, 'pond'), e.waterY || 0);
            e.state = 'gone';
            if (this.flags.fishJumping === e) this.flags.fishJumping = null;
          }
        } else if (e.state === 'dive') {
          if (e.t > 0.7) e.state = 'gone';
        } else if (e.state === 'exit') {
          const tx = e.data.exitTarget as number;
          const dir = tx > e.x ? 1 : -1;
          e.dir = dir;
          e.x += Math.abs(e.vx) * dir * dt;
          e.phase += dt * 8;
          if (Math.abs(e.x - tx) < 6) {
            if (e.data.fetchQuest) {
              e.data.fetched = true; e.state = 'react'; e.fx = 'fetch'; e.t = 0; e.reactDur = 1;
              setTimeout(() => {
                this.flags.haveKey = true;
                // F5: sample the ground at the key's own world x, not x=0
                const cave = this.props.filter(function (p) { return p.key === 'cave'; })[0];
                dropGroundItem(state, 'key', cave ? cave.x - 30 : 0);
              }, 900);
            } else { e.state = 'gone'; }
          }
        }
        // F8a: residents leave on their own — nothing should live forever. Birds
        // and insects fly off; swimmers slip under. Nobody walks out across the
        // bank any more, which is what "walking through walls" was.
        if (e.residentUntil && this.t > e.residentUntil && e.state !== 'react' && !e.data.hop) {
          e.residentUntil = undefined;
          if (sp.kind === 'bird' || sp.kind === 'flutter') this.takeOff(e);
          else {
            const a = this.anchorOf(e);
            e.state = 'dive'; e.t = 0;
            this.puff(a.sx, waterSurfaceY(state), '#BFE6FF', 6, { spread: 30, up: 40, g: 220, life: 0.5 });
          }
        }
        if (e.data.isSwarm && e.t > 90) e.state = 'gone';
        // Out of bounds, heading further out (F7: the parallax-shifted x). An
        // animal that is outside but walking IN is on its way, not lost — the
        // tail of a flock used to be deleted the moment it was spawned.
        const sxe = e.x + paraX(state, e.layer);
        if (e.layer !== 'pond' && !e.perch && e.state !== 'sleep') {
          if ((sxe < -140 && e.vx <= 0) || (sxe > state.W + 140 && e.vx >= 0)) e.state = 'gone';
          if (e.layer === 'sky' && e.y < -80) e.state = 'gone';
        }
        if (e.data.willVanish && e.t > 1.6) e.state = 'gone';
        if (e.state === 'gone') this.entities.splice(idx, 1);
      }
      // ground items: gentle bob + despawn after 60s
      for (let ii = this.items.length - 1; ii >= 0; ii--) {
        const it = this.items[ii];
        it.t += dt;
        if (it.h && it.h > 0) {
          // still falling to the ground of its layer
          it.vh = (it.vh || 0) + 520 * dt;
          it.h = Math.max(0, it.h - it.vh * dt);
        }
        if (it.t > 60) this.items.splice(ii, 1);
      }
      // world fx (Zzz, hearts, notes)
      for (let fi = this.worldFx.length - 1; fi >= 0; fi--) {
        const wf = this.worldFx[fi];
        wf.life -= dt * (wf.plate ? 0.35 : 0.7);
        wf.y += wf.vy * dt;
        if (wf.life <= 0) this.worldFx.splice(fi, 1);
      }
      for (let bi = this.bits.length - 1; bi >= 0; bi--) {
        const b = this.bits[bi];
        b.life -= dt;
        if (b.life <= 0) { this.bits.splice(bi, 1); continue; }
        b.vy += b.g * dt; b.x += b.vx * dt; b.y += b.vy * dt;
      }
    },
    popFx: function (x, y, text, plate) {
      if (plate) this.worldFx = this.worldFx.filter(function (w) { return !w.plate; });
      this.worldFx.push({ x: x, y: y, text: text, life: 1.4, vy: plate ? -4 : -16, plate: plate });
      if (this.worldFx.length > 40) this.worldFx.shift();
    },
    drawEntity: function (ctx, e) {
      const sp = e.sp; if (!sp) return;
      // F7: draw at the parallax-transformed position; e.x stays a world coordinate
      const a = this.anchorOf(e);
      const sx = a.sx, gy = a.gy;
      let dir = (e.dir || 1) >= 0 ? 1 : -1;
      const pal = shadePal((e.data.pal as Palette) || sp.pal || { body: '#888', dark: '#555', eye: '#161616' });
      let squash = 1, rot = 0, extraY = 0, jab = 0;
      const walking = e.state === 'walk' || e.state === 'flee' || e.state === 'exit';
      if (e.state === 'react' || e.state === 'sleep') {
        switch (e.fx) {
          case 'curl': case 'ball': case 'shell': squash = 1 - Math.min(0.5, e.t * 0.6); break;
          case 'roar': case 'happy': case 'friend': case 'pet': squash = 1 + Math.sin(e.t * 10) * 0.06; break;
          case 'rotate': rot = Math.min(1, e.t / 1.2) * Math.PI * 2; break;
          case 'loop': rot = e.t * 8; break;
          case 'thump': extraY = -Math.abs(Math.sin(e.t * 16)) * 3; break;
          case 'peck': jab = Math.abs(Math.sin(e.t * 22)) * 2; break;
          default: break;
        }
      }
      ctx.save();
      // new arrivals on the pond fade in; divers fade out
      if (e.state === 'dive') ctx.globalAlpha = Math.max(0, 1 - e.t / 0.7);
      else if (e.data.fadeIn && e.t < 0.6) ctx.globalAlpha = e.t / 0.6;
      if (rot) { ctx.translate(sx, a.cy); ctx.rotate(rot); ctx.translate(-sx, -a.cy); }
      const wy = waterSurfaceY(state);
      let size: { w: number; h: number } | undefined;
      if (sp.kind === 'bird') {
        const perched = !!e.perch, afloat = e.layer === 'pond';
        const bob = afloat && !state.reduce ? Math.sin(this.t * 2 + e.phase) * 0.7 : 0;
        size = drawBird(ctx, sx + jab * dir, a.cy + extraY + bob, e.scale, dir, pal, e.phase, !perched && !afloat,
          sp.style, perched ? 'perch' : afloat ? 'swim' : undefined);
        if (afloat) { ctx.fillStyle = 'rgba(255,255,255,.35)'; ctx.fillRect(Math.round(sx - 9 * e.scale), Math.round(wy), Math.round(18 * e.scale), 1); }
      } else if (sp.kind === 'flutter') {
        const landed = e.state === 'react' && e.fx === 'land';
        size = drawFlutter(ctx, sx, landed ? gy - 4 * e.scale : a.cy, e.scale, landed ? e.t * 0.5 : e.phase + e.t, pal, sp.style, dir);
      } else if (sp.kind === 'fish') { // F3: arcing fish, nose follows the arc
        ctx.translate(sx, e.jy || gy);
        ctx.rotate((e.vy || 0) * 0.012);
        ctx.fillStyle = pal.body; ctx.fillRect(-6 * e.scale, -3 * e.scale, 12 * e.scale, 6 * e.scale);
        ctx.fillStyle = pal.accent || pal.body; ctx.fillRect(-10 * e.scale, -3 * e.scale, 4 * e.scale, 6 * e.scale);
        ctx.fillStyle = pal.belly || pal.body; ctx.fillRect(-6 * e.scale, 0, 12 * e.scale, 2 * e.scale);
        ctx.fillStyle = '#161616'; ctx.fillRect(3 * e.scale, -1 * e.scale, 1 * e.scale, 1 * e.scale);
        size = { w: 20, h: 14 };
      } else if (sp.kind === 'glow') {
        const glow = e.fx === 'glow' ? 1 : (0.4 + 0.5 * Math.abs(Math.sin(world.t * 3 + e.x * 0.05)));
        ctx.globalAlpha = glow;
        ctx.fillStyle = sp.pal.body;
        ctx.shadowBlur = 10; ctx.shadowColor = sp.pal.body;
        if (e.data.isSwarm) {
          // a loose cloud of lights, not one big square
          for (let i = 0; i < 14; i++) {
            const ox = Math.sin(world.t * (0.7 + i * 0.13) + i * 2.1) * 26, oy = Math.cos(world.t * (0.9 + i * 0.11) + i * 1.3) * 12;
            ctx.fillRect(Math.round(sx + ox), Math.round(a.cy + oy), 3, 3);
          }
          size = { w: 60, h: 30 };
        } else {
          ctx.fillRect(Math.round(sx - 1.5), Math.round(a.cy - 1.5), 3, 3);
          size = { w: 10, h: 10 };
        }
        ctx.shadowBlur = 0; ctx.globalAlpha = 1;
      } else if (sp.style === 'frog') {
        const hop = e.data.hop as { k: number } | undefined;
        // the pad it is sitting on, unless it was dropped somewhere without one
        size = drawFrog(ctx, sx, gy + extraY, e.scale * 1.1, dir, pal, hop ? Math.sin(Math.PI * hop.k) : 0);
      } else {
        if (a.swim) {
          // only what is above the waterline shows
          ctx.beginPath(); ctx.rect(sx - 80, wy - 200, 160, 201); ctx.clip();
        }
        size = drawQuad(ctx, sx, gy + extraY, e.scale, dir, pal, e.phase, walking, sp.shape, squash, e.state === 'flee');
        if (a.swim) {
          ctx.fillStyle = 'rgba(255,255,255,.35)';
          ctx.fillRect(Math.round(sx - size.w * 0.55), Math.round(wy), Math.round(size.w * 1.1), 1);
          size = { w: size.w, h: size.h * 0.5 };
        }
      }
      ctx.restore();
      if (size) { e.hb.w = Math.max(18, size.w); e.hb.h = Math.max(16, size.h); }
      if (e.data && e.data.ducklings && sp.key === 'duck' && e.state !== 'dive') {
        const ducklings = e.data.ducklings as Array<{ ox: number; oy: number }>;
        const afloat = e.layer === 'pond';
        const chick: Palette = { body: sh('#F4D35E'), accent: sh('#D9B23C'), dark: sh('#B8912A'), beak: sh('#E2761B'), head: sh('#F4D35E'), eye: '#161616' };
        dir = (e.dir || 1) >= 0 ? 1 : -1;
        ducklings.forEach(function (_d, i) {
          // they trail behind their mother, tidier once she has lined them up
          const gap = e.data.line ? 9 : 10 + (i % 2) * 2;
          const tx = sx - (i + 1) * gap * dir - 6 * dir;
          const ty = a.cy + 1.5 * e.scale + (e.data.line || state.reduce ? 0 : Math.sin(world.t * 2.4 + i * 1.7) * 0.8);
          drawBird(ctx, tx, afloat ? ty : a.cy + (i + 1) * 3, e.scale * 0.55, dir, chick, e.phase + i * 0.6, !afloat, 'duck', afloat ? 'swim' : undefined);
        });
      }
    },
    drawProp: function (ctx, p) {
      // F7: draw at the parallax-transformed position; p.x stays a world coordinate
      const a = propAnchor(p);
      const sx = a.sx, gy = a.gy;
      if (p.key === 'pond') { // F9: water fills the carved valley — blocky columns, no ellipse
        const stepW = 12, lvl = state.WATER_LEVEL + paraY(state, 2), offX = paraX(state, 2) - state.OVER;
        const basin = state.BASIN;
        if (basin && state.R3) {
          for (let i = basin.c - basin.half; i <= basin.c + basin.half; i++) {
            if (i < 0 || i >= state.R3.length) continue;
            const ty = state.R3[i] + paraY(state, 2);
            if (ty <= lvl) continue;                              // land, not water
            const x = Math.round(i * stepW + offX), top = Math.round(lvl), depth = Math.round(ty - lvl);
            ctx.globalAlpha = 0.84;
            ctx.fillStyle = sh('#3A7CB0');
            ctx.fillRect(x, top, stepW + 1, depth);
            if (depth > 12) { ctx.fillStyle = sh('#2A5F92'); ctx.fillRect(x, top + 12, stepW + 1, depth - 12); }
            ctx.globalAlpha = state.reduce ? 0.6 : 0.5 + 0.3 * Math.sin(world.t * 2 + i * 0.6);
            ctx.fillStyle = sh('#96D7F5');
            ctx.fillRect(x, top, stepW + 1, 2);
            ctx.globalAlpha = 1;
          }
        }
        // ripples: expanding horizontal bars along the surface
        if (!state.reduce && (p.ripple as number) > 0) {
          ctx.fillStyle = 'rgba(255,255,255,' + ((p.ripple as number) * 0.45) + ')';
          for (let r = 0; r < 3; r++) {
            const yy = lvl + 3 + r * 3, half = 16 + r * 10 + (1 - (p.ripple as number)) * 46;
            ctx.fillRect(Math.round(sx - half), Math.round(yy), Math.round(half * 2), 2);
          }
        }
        // lily pads float on the surface
        lilyXs().forEach(function (lx, l) { drawLilyPad(ctx, lx - 6 + paraX(state, 2), lvl, l, sh); });
        return;
      }
      if (p.layer === 'sky') {
        if (p.key === 'star') {
          // a small four-point star, brighter by rank
          const tw = state.reduce ? 1 : 0.75 + 0.25 * Math.sin(world.t * 2.2 + (p.rank as number));
          ctx.fillStyle = '#FFE47A';
          ctx.globalAlpha = (0.5 + (p.rank as number) * 0.1) * tw;
          const cy = (p.y as number) - 4;
          ctx.fillRect(Math.round(sx - 1), Math.round(cy - 4), 2, 8);
          ctx.fillRect(Math.round(sx - 4), Math.round(cy - 1), 8, 2);
          ctx.globalAlpha = 1;
        }
        // sunmoon / clouds / rainbow ends are painted by the sky pass; these
        // props are their invisible hit targets
        return;
      }
      if (p.layer === 'pond') return; // the lily pads are drawn with the water
      const s = layerScale(p.layer);
      const wig = (!state.reduce && (p.shakeT as number)) ? Math.sin((p.shakeT as number) * 40) * 3 * (p.shakeT as number) : 0;
      const host = p.host as Prop | undefined;
      const baseX = host ? host.x : p.x;
      drawPropSprite(ctx, p, sx, gy, s, wig, {
        t: world.t,
        reduce: state.reduce,
        snow: state.WEATHER[state.weather] === 'snow',
        sh: sh,
        groundAt: function (dx) { return groundYWorld(state, baseX + dx * s, p.layer) + paraY(state, p.layer); },
      });
    },
    drawItem: function (ctx, it) {
      const ip = itemPos(it), bob = state.reduce || it.h ? 0 : Math.sin(it.t * 3) * 1.5;
      drawItemSprite(ctx, it.id, ip.x, ip.y - 2 * ip.k + bob, 1.5 * ip.k, sh);
    },
    drawHoverGlow: function (ctx, hit) {
      // Corner brackets on exactly the box that answers the click, plus the
      // thing's name: most of what is out here is small, and a name is the
      // quickest way to say what you are pointing at.
      const b = this.boxOf(hit);
      const x = Math.round(b.x), y = Math.round(b.y), w = Math.round(b.w), h = Math.round(b.h);
      const L = Math.max(3, Math.min(7, Math.floor(w / 3), Math.floor(h / 3)));
      ctx.save();
      ctx.fillStyle = 'rgba(255,255,255,.92)';
      ctx.fillRect(x, y, L, 2); ctx.fillRect(x, y, 2, L);
      ctx.fillRect(x + w - L, y, L, 2); ctx.fillRect(x + w - 2, y, 2, L);
      ctx.fillRect(x, y + h - 2, L, 2); ctx.fillRect(x, y + h - L, 2, L);
      ctx.fillRect(x + w - L, y + h - 2, L, 2); ctx.fillRect(x + w - 2, y + h - L, 2, L);
      const name = hit.kind === 'item' ? (ITEMS[(hit.obj as WorldItem).id] ? ITEMS[(hit.obj as WorldItem).id].name : '')
        : hit.kind === 'entity' ? (hit.obj as Entity).sp.name : (hit.obj as Prop).name;
      if (name) {
        ctx.font = TAG_FONT;
        ctx.textAlign = 'center';
        const m = typeof ctx.measureText === 'function' ? ctx.measureText(name) : null;
        const tw = Math.ceil((m && m.width) || name.length * 6);
        const cx = Math.round(x + w / 2), ty = Math.max(14, y - 5);
        ctx.fillStyle = 'rgba(8,14,26,.8)';
        ctx.fillRect(cx - Math.ceil(tw / 2) - 4, ty - 10, tw + 8, 13);
        ctx.fillStyle = '#fff';
        ctx.fillText(name, cx, ty);
      }
      ctx.restore();
    },
    beginDraw: function () { sh = shader(this.dl); },
    drawLayer: function (ctx, L) {
      const self = this;
      this.props.forEach(function (p) { if (p.layer === L && (!p.visibleIf || p.visibleIf())) self.drawProp(ctx, p); });
      this.entities.forEach(function (e) { if (e.layer === L) self.drawEntity(ctx, e); });
      // items lie on their own layer too, so a nearer hill can hide them
      this.items.forEach(function (it) { if ((it.layer === undefined ? 2 : it.layer) === L) self.drawItem(ctx, it); });
    },
    drawOverlay: function (ctx) {
      // rain patch under a clicked cloud
      this.props.forEach(function (p) {
        if (!state.reduce && p.key === 'clouds' && (p.rainT as number) > 0) {
          ctx.strokeStyle = 'rgba(190,220,255,.5)';
          ctx.lineWidth = 1.4;
          ctx.beginPath();
          for (let i = 0; i < 14; i++) {
            const rx = (p.x as number) + paraX(state, 'sky') - 90 + i * 14;
            const ry = (p.y as number) + 4 + ((world.t * 160 + i * 23) % 80);
            ctx.moveTo(rx, ry);
            ctx.lineTo(rx - 2, ry + 10);
          }
          ctx.stroke();
        }
      });
      if (this.bits.length) {
        this.bits.forEach(function (b) {
          ctx.globalAlpha = Math.max(0, Math.min(1, b.life / b.max * 1.6));
          ctx.fillStyle = sh(b.color);
          ctx.fillRect(Math.round(b.x), Math.round(b.y), b.size, b.size);
        });
        ctx.globalAlpha = 1;
      }
      if (this.worldFx.length) {
        ctx.textAlign = 'center';
        this.worldFx.forEach(function (wf) {
          ctx.globalAlpha = Math.max(0, Math.min(1, wf.life * 2));
          if (wf.plate) {
            ctx.font = TAG_FONT;
            const m = typeof ctx.measureText === 'function' ? ctx.measureText(wf.text) : null;
            const tw = Math.ceil((m && m.width) || wf.text.length * 6);
            const cx = Math.round(Math.max(tw / 2 + 8, Math.min(state.W - tw / 2 - 8, wf.x))), y = Math.round(wf.y);
            ctx.fillStyle = '#E9D2A0';
            ctx.fillRect(cx - Math.ceil(tw / 2) - 6, y - 12, tw + 12, 17);
            ctx.fillStyle = '#6B4A2E';
            ctx.fillRect(cx - Math.ceil(tw / 2) - 6, y + 3, tw + 12, 2);
            ctx.fillRect(cx - 2, y + 5, 4, 3);
            ctx.fillStyle = '#3A2A12';
            ctx.fillText(wf.text, cx, y);
          } else {
            ctx.font = TAG_FONT;
            ctx.fillStyle = '#fff';
            ctx.fillText(wf.text, wf.x, wf.y);
          }
        });
        ctx.globalAlpha = 1;
      }
      if (this.hover) this.drawHoverGlow(ctx, this.hover);
      if (this.carry) {
        // Canvas-space pointer, not `mx * W`: that multiplied a VIEWPORT
        // fraction by the CANVAS width, so the carried item drifted away from
        // the cursor as soon as the two stopped being the same size (zoom).
        const bob = state.reduce ? 0 : Math.sin(world.t * 4) * 2;
        drawItemSprite(ctx, this.carry, state.pcx, state.pcy - 14 + bob, 2, function (c) { return c; });
      }
    },
    draw: function (ctx) {
      // Everything in one go, back to front. The frame loop interleaves these
      // with the terrain instead (see skyFrame), so nearer hills hide what is
      // behind them.
      this.beginDraw();
      this.drawLayer(ctx, 0); this.drawLayer(ctx, 1); this.drawLayer(ctx, 2);
      this.drawLayer(ctx, 'pond'); this.drawLayer(ctx, 'sky');
      this.drawOverlay(ctx);
    },
    interact: function (hit) {
      this.lastInteractAt = this.t;
      if (hit.kind === 'item') {
        const it = hit.obj as WorldItem;
        this.items.splice(this.items.indexOf(it), 1);
        this.pickUp(it.id, it.x, it.y);
        return;
      }
      if (hit.kind === 'entity') {
        const e = hit.obj as Entity, sp = e.sp;
        if (sp && sp.onClick) sp.onClick.call(sp, e);
        return;
      }
      if (hit.kind === 'prop') {
        const p = hit.obj as Prop;
        // Everything you can poke moves when you poke it, whether or not it has
        // anything else to say: a click that only chirped looked like a bug.
        if (p.layer !== 'sky' && p.layer !== 'pond' && p.key !== 'pond') p.shakeT = Math.max((p.shakeT as number) || 0, 0.35);
        if (p.reaction) p.reaction(p);
        return;
      }
    },
    drop: function (x, _y) {
      if (!this.carry) return;
      const carried = this.carry;
      // F7: the pointer is in screen space — store the WORLD x so the item lands
      // under the cursor and egg logic compares world x to world x
      const wx = x - paraX(state, 2);
      this.items.push({ id: carried, x: wx, y: 0, t: 0 });
      if (carried === 'truffle' && this.nearPond(wx)) { this.flags.truffleThrown = true; this._findEgg?.('truffle-pond'); }
      else if (carried === 'acorn' && this.nearPond(wx)) { this.flags.acornInPond = true; dropGroundItem(state, 'coin', wx); this._findEgg?.('acorn-pond'); }
      else if (carried === 'acorn') {
        const buries = (this.flags.acornBuries = (this.flags.acornBuries as Record<string, boolean>) || {});
        buries[Math.round(wx / 60)] = true;
        if (Object.keys(buries).length >= 3) this.flags.acornHuntArmed = true;
      }
      this.dropCarry();
    },
  };

  world.emit = opts.emit;

  // The firefly-swarm species (only spawned by spawnSwarm).
  const SWARM_SP: Species = {
    key: 'swarm', name: 'Firefly swarm', kind: 'glow', pal: P('#CFFF7A', '#fff'),
    w: 0, t: 'A', layer: 2, speed: 0,
    onClick: function (e) {
      if (e.data.spelled) return;
      e.data.spelled = true;
      reactState(e, 'glow', 3);
      playVoice(state, { freqs: [880, 988, 1109, 1245, 1397, 1568, 1760], gap: 90 });
      const a = world.anchorOf(e);
      world.popFx(a.sx, a.cy - 24, 'A G O R A');
      world._findEgg?.('moonlit-rave');
    },
  };

  // Bind the game API the species/props close over, then build them.
  const findEgg = makeFindEgg(state, opts.emit);
  const api: GameApi = {
    world: () => world,
    reactState,
    fleeState,
    boltState,
    exitToward,
    dropGroundItem: (itemId, x, from) => {
      // An item lands on the ground of the layer it came from, and falls there
      // from wherever it was let go: a pine on the middle hills sheds its cones
      // at its own feet, not onto the foreground in front of it.
      let layer = 2, height = 0, at = x;
      if (from && (from as Entity).sp) {
        const e = from as Entity, a = world.anchorOf(e);
        layer = typeof e.layer === 'number' ? e.layer : 2;
        height = Math.max(0, groundYWorld(state, x, layer) + paraY(state, layer) - a.cy);
      } else if (from) {
        const pr = from as Prop;
        layer = typeof pr.layer === 'number' ? pr.layer : 2;
        const ls = layerScale(layer);
        if (pr.key === 'oak' || pr.key === 'pine') {
          height = (pr.hb ? pr.hb.h : 40) * ls * 0.6;
          at = x + (Math.random() < 0.5 ? -1 : 1) * (8 + Math.random() * 10) * ls;
        } else height = (((pr.lift as number) || 0) + 8) * ls;
      }
      dropGroundItem(state, itemId, at, layer, height);
    },
    playVoice: (v) => playVoice(state, v),
    findEgg,
    shakeProp,
    blip: opts.blip,
  };
  buildSpecies(api);
  // The layout reads the ground (flat spots, the water's edge), so the terrain
  // has to exist before the props are placed on it.
  if (state.W > 0 && state.H > 0) regenerateTerrain(state);
  world.props = buildProps(state, api);
  world._findEgg = findEgg;
  // keep the achievement checker reachable after resume
  void checkAchievements;
  return world;
}
