/**
 * The 30-species wildlife catalogue + the procedural pixel critter renderers.
 *
 * The state helpers (reactState, fleeState, boltState, exitToward,
 * dropGroundItem, playVoice, findEgg, shakeProp) come from the bound GameApi.
 */

import type { GameApi } from './api';
import type { Entity, Palette, ShapeDef, Species } from './types';

function darken(hex: string, amt: number): string {
  hex = hex.replace('#', '');
  const r = parseInt(hex.substr(0, 2), 16), g = parseInt(hex.substr(2, 2), 16), b = parseInt(hex.substr(4, 2), 16);
  const c = (v: number): number => Math.max(0, Math.min(255, v - amt));
  return 'rgb(' + c(r) + ',' + c(g) + ',' + c(b) + ')';
}

export function P(body: string, accent?: string, belly?: string): Palette {
  return {
    body: body, accent: accent || body, dark: darken(body, 55), light: darken(body, -26), mid: darken(body, 26),
    belly: belly, eye: '#161616',
  };
}

const DEFSHAPE: ShapeDef = {
  bw: 16, bh: 10, hw: 8, hh: 8, lw: 3, lh: 6, tw: 8, th: 6, ew: 3, eh: 5, ears: true, tail: true, legs: true,
  // --- silhouette features (F4): every quadruped shared one box outline before this ---
  earShape: 'point', // point | round | long | none
  tailShape: 'stub', // stub | bushy | long | flat | none
  gait: 'walk',
  snout: 0,         // muzzle projection, px
  neck: 0,          // how far the head sits above the body
  hump: 0,          // shoulder hump height
  antlers: false, tusks: false, spikes: false, shell: false, mane: false, mask: false,
};

export function shape(o: ShapeDef): ShapeDef {
  const s: ShapeDef = {};
  for (const k in DEFSHAPE) (s as Record<string, unknown>)[k] = (DEFSHAPE as Record<string, unknown>)[k];
  for (const k2 in o) (s as Record<string, unknown>)[k2] = (o as Record<string, unknown>)[k2];
  return s;
}

/** Fraction of a hop spent in the air; the rest is the crouch between hops. */
export const HOP_AIR = 0.68;

/** Where in its hop cycle (0..1) an animal is for a given gait phase. */
export function hopCycle(phase: number): number {
  const u = (phase / (Math.PI * 2)) % 1;
  return u < 0 ? u + 1 : u;
}

/**
 * How fast the gait phase should advance, in radians per second, for an
 * animal covering `speed` px/s. One full turn of the phase is one stride (or
 * one hop), so the rate is the speed divided by the ground a stride covers.
 * A fixed rate for every animal made a rabbit drift along in slow arcs and a
 * fox's feet slide under it.
 */
export function gaitRate(shapeDef: ShapeDef | undefined, s: number, speed: number): number {
  if (!speed) return 0;
  const sd = shapeDef || {};
  if (sd.gait === 'hop') return (Math.PI * 2 * speed) / (Math.max(26, speed * 0.45) * s);
  const reach = Math.min((sd.lh || 6) * 0.75, 4.5) * s;
  return Math.min(24, (Math.PI * 2 * speed) / (4 * reach));
}

/**
 * A four-legged animal, built from blocks on the same grid as the props.
 *
 * The body is lit from above (a pale strip along the back, a darker underside),
 * the far pair of legs is darker than the near pair, and the face has a nose
 * and an eye, so an animal reads as a small figure and not a brown rectangle.
 * Hips stay fixed to the body's underside and the feet lift (F2); each species
 * keeps its own silhouette through `shapeDef` (F4).
 */
export function drawQuad(
  ctx: CanvasRenderingContext2D, x: number, gy: number, s: number, dir: number,
  pal: Palette, phase: number, walking: boolean, shapeDef?: ShapeDef, squash?: number, running?: boolean,
): { w: number; h: number } {
  shapeDef = shapeDef || {}; squash = squash || 1;
  const bw = (shapeDef.bw || 16) * s, bh = (shapeDef.bh || 10) * s * squash;
  const hw = (shapeDef.hw || 8) * s, hh = (shapeDef.hh || 8) * s;
  const lw = Math.max(1, (shapeDef.lw || 3) * s), lh = (shapeDef.lh || 6) * s;
  // --- gait ---------------------------------------------------------
  // Walking: the body rises and falls twice a stride and each leg swings from
  // the hip, lifting its foot on the way forward. Hopping (rabbits, squirrels):
  // the whole animal leaves the ground, front legs reaching and back legs
  // trailing, then gathers itself to land.
  const hopping = walking && shapeDef.gait === 'hop';
  const hopU = hopping ? hopCycle(phase) : 0;
  const air = hopping && hopU < HOP_AIR ? Math.sin(Math.PI * hopU / HOP_AIR) : 0;
  // between hops it gathers itself: a short crouch on the ground
  const crouch = hopping && hopU >= HOP_AIR ? Math.sin(Math.PI * (hopU - HOP_AIR) / (1 - HOP_AIR)) * Math.min(1.5, lh * 0.3) * s : 0;
  const bob = walking && !hopping ? Math.abs(Math.sin(phase)) * Math.min(1.2, lh * 0.12) * s : 0;
  const rise = air * (running ? 7 : 6) * s;
  const by = gy - lh - bh - bob - rise + crouch;
  const bx0 = x - bw / 2;
  const R = Math.round;
  const light = pal.light || pal.body, mid = pal.mid || pal.dark;
  const F = (fx: number, fy: number, fw: number, fh: number, c?: string) => {
    if (c) ctx.fillStyle = c;
    ctx.fillRect(R(fx), R(fy), Math.max(1, R(fx + fw) - R(fx)), Math.max(1, R(fy + fh) - R(fy)));
  };
  /** A block placed from the animal's back end: `d` runs towards the head. */
  const along = (d: number, w: number): number => (dir > 0 ? bx0 + d : bx0 + bw - d - w);
  if (shapeDef.snail) {
    // A snail: a long flat foot, a head with two eye stalks, and a spiral
    // shell riding on its back. It used to borrow the turtle's dome.
    const ripple = walking ? Math.sin(phase) * 0.6 * s : 0;
    F(along(-2 * s, bw + 5 * s + ripple), gy - 2.5 * s, bw + 5 * s + ripple, 2.5 * s, pal.body);
    F(along(-2 * s, bw + 5 * s + ripple), gy - 1 * s, bw + 5 * s + ripple, 1 * s, mid);
    F(along(bw + 0.5 * s + ripple, 2.5 * s), gy - 6 * s, 2.5 * s, 4 * s, pal.body);
    F(along(bw + 0.5 * s + ripple, 1 * s), gy - 9 * s, 1 * s, 3 * s, pal.body);
    F(along(bw + 2.5 * s + ripple, 1 * s), gy - 8.5 * s, 1 * s, 2.5 * s, pal.body);
    F(along(bw + 0.5 * s + ripple, 1 * s), gy - 9.5 * s, 1 * s, 1 * s, pal.eye || '#161616');
    F(along(bw + 2.5 * s + ripple, 1 * s), gy - 9 * s, 1 * s, 1 * s, pal.eye || '#161616');
    const shell = pal.accent || pal.dark;
    F(along(0.5 * s, 8 * s), gy - 8.5 * s, 8 * s, 6 * s, shell);
    F(along(1.5 * s, 6 * s), gy - 10 * s, 6 * s, 1.5 * s, shell);
    F(along(1.5 * s, 6 * s), gy - 2.5 * s - 0.5 * s, 6 * s, 0.5 * s, shell);
    // the spiral
    F(along(2 * s, 5 * s), gy - 8 * s, 5 * s, 1 * s, light);
    F(along(6 * s, 1 * s), gy - 8 * s, 1 * s, 4 * s, light);
    F(along(3 * s, 4 * s), gy - 5 * s, 4 * s, 1 * s, light);
    F(along(3 * s, 1 * s), gy - 7 * s + 0.5 * s, 1 * s, 2.5 * s, light);
    F(along(4.5 * s, 1 * s), gy - 6.5 * s, 1 * s, 1 * s, light);
    return { w: bw + 6 * s, h: 11 * s };
  }
  // --- legs: hips fixed to the body's underside (F2); thigh and shin, so a step bends at the knee ---
  if (shapeDef.legs !== false) {
    const hipY = by + bh;
    const reach = Math.min(lh * 0.75, 4.5 * s) * (running ? 1.4 : 1);
    const foot = Math.max(1, s);
    const leg = (d: number, a: number, col: string, shod: boolean): void => {
      let swing: number, lift: number, len: number;
      if (hopping) {
        // front pair reaches ahead, back pair trails; all tuck up in the air
        swing = (d > bw * 0.5 ? 1 : -1) * air * reach;
        len = lh * (1 - air * 0.35) - crouch;
        lift = 0;
      } else {
        swing = walking ? Math.sin(phase + a) * reach : 0;
        lift = walking ? Math.max(0, Math.cos(phase + a)) * lh * 0.3 : 0;
        len = gy - hipY;
      }
      // The whole leg swings: it is drawn as a stack of short slices running
      // from the hip to wherever the foot is, so it leans forward and back as
      // one limb. A leg that is off the ground also bends at the knee. Only
      // the lower half used to move, and only half-way, which read as the
      // feet shuffling under a body that was not walking.
      const hx0 = along(d, lw);
      const reachLen = Math.max(1, len - lift);
      const n = Math.max(2, Math.min(8, Math.round(reachLen / Math.max(1, 1.5 * s))));
      const step = reachLen / n;
      for (let i = 0; i < n; i++) {
        const t = (i + 0.5) / n;
        const bend = Math.sin(Math.PI * t) * lift * 0.6;
        F(hx0 + (swing * t + bend) * dir, hipY + step * i, lw, step + 0.5, col);
      }
      if (shod && reachLen > foot * 2) F(hx0 + swing * dir, hipY + reachLen - foot, lw + (lw < 2 * s ? 0 : foot * 0.5), foot, pal.dark);
    };
    // the far pair first, in shadow; diagonal legs move together
    leg(bw * 0.26, Math.PI, pal.dark, false);
    leg(bw * 0.62, 0, pal.dark, false);
    leg(bw * 0.10, 0, mid, true);
    leg(bw * 0.80, Math.PI, mid, true);
  }
  // --- tail: shape-specific, behind the body ---
  const tailK = shapeDef.tailShape || 'stub';
  if (shapeDef.tail !== false && tailK !== 'none') {
    const tw = (shapeDef.tw || 8) * s, th = (shapeDef.th || 6) * s;
    const back = dir > 0 ? bx0 - tw * 0.7 : bx0 + bw - tw * 0.3; // rear edge, whichever way it faces
    ctx.fillStyle = pal.accent || pal.body;
    if (tailK === 'bushy') {                                  // fox / squirrel: a plume that rises from the rump
      // Each block is placed from the body's rear edge and overlaps the one
      // before it, so the plume is one piece however long the tail is. It
      // used to step backwards by a fixed amount, which left a squirrel's
      // tall tail floating behind it.
      const rear = (d: number, w: number): number => (dir > 0 ? bx0 - d - w : bx0 + bw + d);
      const wag = walking ? Math.sin(phase * 0.5) * 0.8 * s : 0;
      const ty = by + bh * 0.2;
      F(rear(-1.5 * s, tw * 0.55), ty, tw * 0.55, th * 0.5);
      F(rear(tw * 0.2 + wag * 0.5, tw * 0.6), ty - th * 0.4, tw * 0.6, th * 0.6);
      F(rear(tw * 0.4 + wag, tw * 0.55), ty - th * 0.8, tw * 0.55, th * 0.55);
      // a pale tip on the plume
      if (pal.belly) F(rear(tw * 0.5 + wag, tw * 0.35), ty - th * 0.8, tw * 0.35, th * 0.25, pal.belly);
      else F(rear(tw * 0.5 + wag, tw * 0.35), ty - th * 0.8, tw * 0.35, Math.max(1, s), light);
    } else if (tailK === 'cat') {                             // cat: carried straight up, with a crook at the tip
      const rear = (d: number, w: number): number => (dir > 0 ? bx0 - d - w : bx0 + bw + d);
      const sway = walking ? Math.sin(phase * 0.5) * 0.8 * s : 0;
      const ty = by + bh * 0.15;
      F(rear(-1 * s, 2.2 * s), ty, 2.2 * s, 2 * s, pal.body);
      F(rear(0.2 * s, 1.6 * s), ty - th * 0.5, 1.6 * s, th * 0.5 + 1, pal.body);
      F(rear(0.2 * s + sway * 0.5, 1.6 * s), ty - th, 1.6 * s, th * 0.5 + 1, pal.body);
      F(rear(1.4 * s + sway, 2 * s), ty - th, 2 * s, 1.6 * s, pal.accent || pal.dark);
    } else if (tailK === 'long') {                            // wolf: low straight sweep
      F(back + (dir > 0 ? -tw * 0.35 : tw * 0.35), by + bh * 0.34, tw * 1.15, th * 0.36);
    } else if (tailK === 'flat') {                            // beaver-ish paddle
      F(back, by + bh * 0.42, tw * 1.05, th * 0.85);
    } else {                                                  // stub
      F(back + (dir > 0 ? tw * 0.25 : -tw * 0.25), by + bh * 0.12, tw * 0.5, th * 0.55);
    }
  }
  // --- shoulder hump (bear, boar): stacked steps above the front of the back ---
  if (shapeDef.hump) {
    const hpx = dir > 0 ? bx0 + bw * 0.46 : bx0 + bw * 0.1, hstep = shapeDef.hump * s / 3;
    ctx.fillStyle = pal.body;
    for (let hI = 0; hI < 3; hI++) F(hpx + hI * 1.2 * s, by - hstep * (3 - hI), bw * 0.42 - hI * 1.6 * s, hstep * (3 - hI) + 1);
    F(hpx + 2.4 * s, by - hstep * 3, bw * 0.42 - 4 * s, Math.max(1, s), light);
  }
  // --- body: lit along the back, shaded underneath ---
  F(bx0, by, bw, bh, pal.body);
  if (pal.belly) F(bx0, by + bh * 0.55, bw, bh * 0.42, pal.belly);
  else F(bx0, by + bh * 0.72, bw, bh * 0.28, mid);
  if (!shapeDef.shell && !shapeDef.spikes) F(bx0 + bw * 0.08, by, bw * 0.84, Math.max(1, bh * 0.2), light);
  if (shapeDef.stripes) {                                    // tabby bars across the back
    for (let b = 0; b < 3; b++) F(along(bw * (0.3 + b * 0.2), 1.3 * s), by, 1.3 * s, bh * 0.5, pal.accent || pal.dark);
  }
  // the haunch: a darker block where the back leg meets the body
  if (shapeDef.legs !== false && bw > 12 * s) F(along(bw * 0.04, bw * 0.2), by + bh * 0.3, bw * 0.2, bh * 0.42, mid);
  // --- spikes along the back (hedgehog) ---
  if (shapeDef.spikes) {
    ctx.fillStyle = pal.dark;
    for (let sI = 0; sI < 9; sI++) F(bx0 + bw * (0.04 + sI * 0.108), by - 5.2 * s - (sI % 2) * 1.4 * s, 1.5 * s, 6.4 * s + (sI % 2) * 1.4 * s);
    ctx.fillStyle = mid;
    for (let sI = 1; sI < 9; sI += 2) F(bx0 + bw * (0.04 + sI * 0.108), by - 1 * s, 1.5 * s, 3 * s);
  }
  // --- shell dome (turtle), drawn over the body ---
  if (shapeDef.shell) {
    ctx.fillStyle = pal.accent || pal.dark;
    F(bx0 - bw * 0.10, by - bh * 0.70, bw * 1.20, bh * 0.95);
    F(bx0 + bw * 0.10, by - bh * 1.10, bw * 0.80, bh * 0.5);
    F(bx0 + bw * 0.28, by - bh * 1.35, bw * 0.44, bh * 0.35);
    F(bx0 + bw * 0.28, by - bh * 1.35, bw * 0.44, Math.max(1, s), light);
    ctx.fillStyle = pal.dark;
    for (let kI = 0; kI < 3; kI++) F(bx0 + bw * (0.22 + kI * 0.24), by - bh * 0.45, 1.4 * s, bh * 0.5);
    F(bx0 - bw * 0.10, by + bh * 0.15, bw * 1.20, Math.max(1, s));
  }
  // --- head, raised by neck ---
  const neck = (shapeDef.neck || 0) * s;
  const hx = bx0 + (dir > 0 ? bw - hw * 0.35 : -hw * 0.65);
  // the head nods a little against the body's rise and fall
  const hy = by - hh * 0.45 - neck + (walking && !hopping ? Math.sin(phase * 2) * 0.6 * s : 0);
  if (neck) {                                               // visible throat column linking body to head
    F(hx + (dir > 0 ? -hw * 0.05 : hw * 0.42), hy + hh * 0.55, hw * 0.6, neck + hh * 0.5, pal.body);
    if (pal.belly) F(hx + (dir > 0 ? hw * 0.3 : hw * 0.42), hy + hh * 0.9, hw * 0.25, neck + hh * 0.15, pal.belly);
  }
  if (shapeDef.mane) {                                      // wolf ruff between head and shoulders
    F(hx + (dir > 0 ? -hw * 0.42 : hw * 0.5), hy + hh * 0.1, hw * 0.62, hh * 1.05, pal.dark);
  }
  F(hx, hy, hw, hh, pal.body);
  F(hx + hw * 0.1, hy, hw * 0.8, Math.max(1, hh * 0.18), light);
  if (pal.belly && !shapeDef.mask) F(dir > 0 ? hx + hw * 0.45 : hx, hy + hh * 0.66, hw * 0.55, hh * 0.34, pal.belly);
  if (shapeDef.mask && pal.belly) {                         // raccoon/badger face band
    F(hx, hy + hh * 0.28, hw, hh * 0.3, pal.dark);
    F(dir > 0 ? hx + hw * 0.5 : hx, hy + hh * 0.62, hw * 0.5, hh * 0.38, pal.belly);
  }
  // --- snout, with a nose at its tip ---
  const noseC = '#241C18';
  if (shapeDef.snout && shapeDef.droop) {
    // a moose: a long nose that overhangs the jaw, and a bell under the chin
    const sn = shapeDef.snout * s;
    const sx0 = dir > 0 ? hx + hw * 0.7 : hx - sn * 0.85 + hw * 0.12;
    F(sx0, hy + hh * 0.3, sn + hw * 0.18, hh * 0.85, pal.body);
    F(sx0, hy + hh * 0.3, sn + hw * 0.18, Math.max(1, hh * 0.16), light);
    F(dir > 0 ? sx0 + sn * 0.45 : sx0, hy + hh * 0.85, sn * 0.55 + hw * 0.18, hh * 0.3, mid);
    F(dir > 0 ? sx0 + sn + hw * 0.18 - 1.6 * s : sx0, hy + hh * 0.62, 1.6 * s, Math.max(1, s), noseC);
    F(dir > 0 ? hx + hw * 0.3 : hx + hw * 0.7 - 1.8 * s, hy + hh, 1.8 * s, 3 * s, pal.dark);
  } else if (shapeDef.snout) {
    const sn = shapeDef.snout * s;
    const sx0 = dir > 0 ? hx + hw * 0.82 : hx - sn * 0.9;
    F(sx0, hy + hh * 0.42, sn, hh * 0.42, pal.belly || mid);
    F(dir > 0 ? sx0 + sn - Math.max(1, s) : sx0, hy + hh * 0.42, Math.max(1, s), Math.max(1, s), noseC);
  } else {
    F(dir > 0 ? hx + hw - Math.max(1, s) : hx, hy + hh * 0.55, Math.max(1, s), Math.max(1, s), noseC);
  }
  if (shapeDef.whiskers) {
    const wx = dir > 0 ? hx + hw : hx - 2 * s;
    F(wx, hy + hh * 0.62, 2 * s, Math.max(1, s * 0.6), '#F2F4F6');
    F(wx, hy + hh * 0.82, 1.5 * s, Math.max(1, s * 0.6), '#F2F4F6');
  }
  // --- tusks (boar) ---
  if (shapeDef.tusks) {
    ctx.fillStyle = '#F2EBD8';
    const tx0 = dir > 0 ? hx + hw * 0.95 : hx - hw * 0.2;
    F(tx0, hy + hh * 0.5, 1.6 * s, 3.4 * s);
    F(tx0 + (dir > 0 ? -1.8 * s : 1.8 * s), hy + hh * 0.34, 1.4 * s, 2.4 * s);
  }
  /** A block on the head, placed from the back of the skull: `d` runs towards the nose. */
  const ahead = (d: number, w: number): number => (dir > 0 ? hx + d : hx + hw - d - w);
  // --- ears, with a paler inside. Drawn before antlers, which stand in front of them. ---
  const earK = shapeDef.earShape || (shapeDef.ears === false ? 'none' : 'point');
  if (shapeDef.ears !== false && earK !== 'none') {
    const ew = (shapeDef.ew || 3) * s, eh = (shapeDef.eh || 5) * s;
    const inner = pal.belly || light;
    if (earK === 'long') {                                    // rabbit / hare
      F(hx + hw * 0.14, hy - eh * 1.9, ew * 0.85, eh * 2.0, pal.dark);
      F(hx + hw * 0.55, hy - eh * 1.7, ew * 0.85, eh * 1.85, pal.dark);
      if (ew * 0.85 >= 2) F(hx + hw * 0.55 + ew * 0.25, hy - eh * 1.4, Math.max(1, ew * 0.3), eh * 1.2, inner);
    } else if (earK === 'side') {                             // deer / moose: a leaf-shaped ear held out behind the head
      F(ahead(-ew * 0.9, ew * 1.3), hy + hh * 0.02, ew * 1.3, eh * 0.55, pal.body);
      F(ahead(-ew * 1.5, ew * 1.1), hy - eh * 0.35, ew * 1.1, eh * 0.55, pal.body);
      F(ahead(-ew * 1.5, ew * 1.1), hy - eh * 0.35, ew * 1.1, Math.max(1, s * 0.8), pal.dark);
      F(ahead(-ew * 0.7, ew * 0.7), hy + hh * 0.02 + eh * 0.12, ew * 0.7, eh * 0.3, mid);
    } else if (earK === 'round') {                            // bear / mouse / raccoon
      F(hx + hw * 0.02, hy - eh * 0.6, ew * 1.5, eh * 0.85, pal.dark);
      F(hx + hw * 0.6, hy - eh * 0.6, ew * 1.5, eh * 0.85, pal.dark);
    } else {                                                  // pointed
      F(hx + hw * 0.08, hy - eh * 0.85, ew, eh, pal.dark);
      F(hx + hw * 0.58, hy - eh * 0.85, ew, eh, pal.dark);
      if (ew >= 2.5) F(hx + hw * 0.58 + ew * 0.3, hy - eh * 0.5, Math.max(1, ew * 0.35), eh * 0.5, inner);
    }
  }
  // --- antlers ---
  if (shapeDef.antlers) {
    const horn = pal.horn || light;
    const a0 = hw * 0.3;
    if (shapeDef.antlerShape === 'moose') {
      // Broad flat paddles held out level, with short points along the top.
      // The far one shows behind the near one, a little higher and in shadow.
      F(ahead(a0 - 3 * s, 10 * s), hy - 7 * s, 10 * s, 2 * s, pal.dark);
      for (let i = 0; i < 3; i++) F(ahead(a0 - 2 * s + i * 3.4 * s, 1.3 * s), hy - 8.5 * s, 1.3 * s, 1.6 * s, pal.dark);
      F(ahead(a0, 1.8 * s), hy - 3 * s, 1.8 * s, 3 * s + 1, horn);
      F(ahead(a0 - 5.5 * s, 13 * s), hy - 5.5 * s, 13 * s, 2.6 * s, horn);
      F(ahead(a0 - 5.5 * s, 13 * s), hy - 3.6 * s, 13 * s, 0.7 * s, mid);
      F(ahead(a0 - 6.5 * s, 2 * s), hy - 7 * s, 2 * s, 2.6 * s, horn);
      F(ahead(a0 + 6.5 * s, 2.2 * s), hy - 6.8 * s, 2.2 * s, 2.2 * s, horn);
      for (let i = 0; i < 4; i++) F(ahead(a0 - 3.6 * s + i * 2.9 * s, 1.4 * s), hy - 7.4 * s, 1.4 * s, 2 * s, horn);
    } else {
      // A deer's rack: a beam that rises from the brow and sweeps forward
      // over the head, with tines standing up along it.
      const rack = (ox: number, oy: number, col: string): void => {
        F(ahead(a0 + ox, 1.3 * s), hy - 3.2 * s + oy, 1.3 * s, 3.2 * s + 1, col);
        F(ahead(a0 - 1.6 * s + ox, 7.6 * s), hy - 4.4 * s + oy, 7.6 * s, 1.3 * s, col);
        F(ahead(a0 - 1.6 * s + ox, 1.2 * s), hy - 7.4 * s + oy, 1.2 * s, 3.2 * s, col);
        F(ahead(a0 + 1.4 * s + ox, 1.2 * s), hy - 8.6 * s + oy, 1.2 * s, 4.4 * s, col);
        F(ahead(a0 + 4.2 * s + ox, 1.2 * s), hy - 7.2 * s + oy, 1.2 * s, 3 * s, col);
        F(ahead(a0 + 5.8 * s + ox, 1.2 * s), hy - 5.8 * s + oy, 1.2 * s, 1.6 * s, col);
      };
      rack(1.8 * s, -0.8 * s, pal.dark);
      rack(0, 0, horn);
    }
  }
  // --- eye ---
  F(hx + (dir > 0 ? hw * 0.58 : hw * 0.42 - Math.max(1, s)), hy + hh * 0.28, Math.max(1, s), Math.max(1, s * 1.4), pal.eye || '#161616');
  return { w: bw + hw, h: bh + hh + lh + neck + (shapeDef.antlers ? hh : 0) };
}

/**
 * A bird, drawn side-on around its body centre. `style` picks the silhouette
 * (owl, bat, goose, duck; anything else is the plain songbird shape) and
 * `pose` says what it is doing when it is not flying: sitting on a perch or
 * floating on the pond. A bird that is not flying keeps its wings folded.
 */
export function drawBird(
  ctx: CanvasRenderingContext2D, x: number, y: number, s: number, dir: number,
  pal: Palette, phase: number, flying: boolean, style?: string, pose?: 'perch' | 'swim',
): { w: number; h: number } {
  const R = (dx: number, dy: number, w: number, h: number, c: string): void => {
    const l = dir > 0 ? x + dx * s : x - (dx + w) * s;
    const t = y + dy * s;
    ctx.fillStyle = c;
    ctx.fillRect(Math.round(l), Math.round(t), Math.max(1, Math.round(l + w * s) - Math.round(l)), Math.max(1, Math.round(t + h * s) - Math.round(t)));
  };
  const accent = pal.accent || pal.body, head = pal.head || pal.body, beak = pal.beak || '#F5A64B';
  const flap = flying ? Math.sin(phase * (style === 'bat' ? 3.4 : 2.2)) : 0;
  if (style === 'owl') {
    if (flying) { R(-10, -3 - flap * 3, 6, 3, accent); R(4, -3 - flap * 3, 6, 3, accent); }
    R(-4, -5, 8, 10, pal.body);
    R(-2, -1, 5, 5, pal.belly || accent);
    R(-2, 0, 1, 1, pal.dark); R(1, 2, 1, 1, pal.dark); R(-1, 3, 1, 1, pal.dark);
    R(-4, -7, 2, 2, pal.dark); R(2, -7, 2, 2, pal.dark);
    R(-3, -4, 2, 2, '#F4C542'); R(1, -4, 2, 2, '#F4C542');
    R(-2, -3, 1, 1, '#161616'); R(2, -3, 1, 1, '#161616');
    R(0, -2, 1, 2, '#E2761B');
    if (!flying) { R(-4, -1, 1, 5, pal.dark); R(3, -1, 1, 5, pal.dark); R(-2, 5, 1, 1, '#E2761B'); R(1, 5, 1, 1, '#E2761B'); }
    return { w: 14 * s, h: 14 * s };
  }
  if (style === 'bat') {
    R(-10, -2 - flap * 3, 8, 3, accent); R(2, -2 - flap * 3, 8, 3, accent);
    R(-10, 1 - flap * 3, 2, 2, accent); R(-6, 1 - flap * 3, 2, 1, accent); R(4, 1 - flap * 3, 2, 1, accent); R(8, 1 - flap * 3, 2, 2, accent);
    R(-2, -2, 4, 5, pal.body);
    R(-2, -4, 1, 2, pal.body); R(1, -4, 1, 2, pal.body);
    R(-1, -1, 1, 1, '#E6533E'); R(1, -1, 1, 1, '#E6533E');
    return { w: 20 * s, h: 10 * s };
  }
  const long = style === 'goose';
  // tail, body, belly
  R(long ? -10 : -8, -1, 4, 2, accent);
  R(long ? -7 : -5, -3, long ? 13 : 10, 6, pal.body);
  if (pal.belly) R(long ? -5 : -3, 1, long ? 10 : 7, 2, pal.belly);
  // neck and head
  if (long) {
    R(5, -4, 7, 2, head); R(11, -6, 4, 4, head); R(12, -4, 2, 1, '#F2F4F6');
    R(15, -4, 2, 1, beak);
  } else if (style === 'duck') {
    R(3, -2, 3, 1, '#F2F4F6');
    R(3, -7, 5, 5, head); R(8, -5, 3, 2, beak); R(6, -6, 1, 1, '#161616');
    R(-8, -4, 3, 3, accent);
  } else {
    R(2, -6, 5, 5, head); R(7, -4, 3, 2, beak); R(5, -5, 1, 1, '#161616');
  }
  // wing
  if (flying) {
    const span = 2 + Math.abs(flap) * 5;
    if (flap > 0) R(-3, -3 - span, long ? 8 : 6, span, accent);
    else R(-3, 1, long ? 8 : 6, span, accent);
  } else {
    R(-4, -2, 7, 3, accent); R(-4, 0, 7, 1, pal.dark);
    if (style === 'duck') R(-1, -1, 2, 1, '#3F6FA0');
  }
  if (pose === 'perch') { R(-1, 3, 1, 2, pal.dark); R(2, 3, 1, 2, pal.dark); }
  return { w: (long ? 26 : 19) * s, h: 14 * s };
}

/**
 * A flying insect. `style` is 'bee', 'dragonfly', or (default) a butterfly.
 * `y` is the hover height; the insect bobs around it on its own.
 */
export function drawFlutter(
  ctx: CanvasRenderingContext2D, x: number, y: number, s: number, phase: number, pal: Palette,
  style?: string, dir = 1,
): { w: number; h: number } {
  const u = s * 1.25;
  const bob = Math.sin(phase * 1.5) * 3 * s;
  const cy = y + bob;
  const F = (fx: number, fy: number, fw: number, fh: number, c: string): void => {
    ctx.fillStyle = c;
    ctx.fillRect(Math.round(fx), Math.round(fy), Math.max(1, Math.round(fw)), Math.max(1, Math.round(fh)));
  };
  if (style === 'bee') {
    const flick = Math.floor(phase * 5) % 2;
    F(x - 3 * u, cy - 2 * u, 6 * u, 4 * u, pal.body);
    F(x - 1.5 * u, cy - 2 * u, 1 * u, 4 * u, '#2A2A2A'); F(x + 0.5 * u, cy - 2 * u, 1 * u, 4 * u, '#2A2A2A');
    F(dir > 0 ? x + 3 * u : x - 5 * u, cy - 1.5 * u, 2 * u, 3 * u, '#2A2A2A');
    F(dir > 0 ? x - 4 * u : x + 3 * u, cy - 0.5 * u, 1 * u, 1 * u, '#2A2A2A');
    ctx.globalAlpha *= 0.75;
    F(x - 2.5 * u, cy - (4 + flick) * u, 3 * u, 2 * u, '#F2F8FF'); F(x + 0.5 * u, cy - (4.5 - flick) * u, 3 * u, 2 * u, '#F2F8FF');
    ctx.globalAlpha /= 0.75;
    return { w: 9 * u, h: 8 * u };
  }
  if (style === 'dragonfly') {
    const wf = 0.4 + 0.6 * Math.abs(Math.sin(phase * 6));
    for (let i = 0; i < 5; i++) F(dir > 0 ? x - 7 * u + i * 2 * u : x + 5 * u - i * 2 * u, cy, 2 * u, 1.5 * u, i % 2 ? pal.body : pal.dark);
    F(dir > 0 ? x + 3 * u : x - 5 * u, cy - 0.5 * u, 2 * u, 2.5 * u, pal.body);
    F(dir > 0 ? x + 5 * u : x - 7 * u, cy - 1 * u, 2 * u, 3 * u, pal.dark);
    ctx.globalAlpha *= 0.7;
    F(x - 1 * u, cy - 4.5 * u * wf, 2 * u, 4.5 * u * wf, '#DFFFFA'); F(x + 1.5 * u, cy - 4 * u * wf, 2 * u, 4 * u * wf, '#DFFFFA');
    F(x - 1 * u, cy + 1.5 * u, 2 * u, 3 * u * wf, '#DFFFFA'); F(x + 1.5 * u, cy + 1.5 * u, 2 * u, 2.5 * u * wf, '#DFFFFA');
    ctx.globalAlpha /= 0.7;
    return { w: 16 * u, h: 10 * u };
  }
  // butterfly: two pairs of wings that open and close around a dark body
  const wf = 0.3 + 0.7 * Math.abs(Math.sin(phase * 2.5));
  const up = 5 * u * wf, lo = 4 * u * wf;
  F(x - 0.5 * u - up, cy - 4 * u, up, 4 * u, pal.body); F(x + 0.5 * u, cy - 4 * u, up, 4 * u, pal.body);
  F(x - 0.5 * u - lo, cy, lo, 3 * u, pal.accent || pal.body); F(x + 0.5 * u, cy, lo, 3 * u, pal.accent || pal.body);
  if (wf > 0.55) {
    F(x - 0.5 * u - up * 0.7, cy - 3 * u, u, u, '#FFF6E0'); F(x + 0.5 * u + up * 0.7 - u, cy - 3 * u, u, u, '#FFF6E0');
    F(x - 0.5 * u - up, cy - 4 * u, up, u * 0.8, pal.dark); F(x + 0.5 * u, cy - 4 * u, up, u * 0.8, pal.dark);
  }
  F(x - 0.5 * u, cy - 3.5 * u, u, 6.5 * u, '#2A2A2A');
  F(x - 1.5 * u, cy - 5.5 * u, u * 0.8, 2 * u, '#2A2A2A'); F(x + 0.7 * u, cy - 5.5 * u, u * 0.8, 2 * u, '#2A2A2A');
  return { w: 12 * u, h: 10 * u };
}

/** A frog sitting on its haunches. `gy` is what it sits on. */
export function drawFrog(
  ctx: CanvasRenderingContext2D, x: number, gy: number, s: number, dir: number, pal: Palette, stretch: number,
): { w: number; h: number } {
  const R = (dx: number, dy: number, w: number, h: number, c: string): void => {
    const l = dir > 0 ? x + dx * s : x - (dx + w) * s;
    const t = gy - (dy + h) * s;
    ctx.fillStyle = c;
    ctx.fillRect(Math.round(l), Math.round(t), Math.max(1, Math.round(l + w * s) - Math.round(l)), Math.max(1, Math.round(t + h * s) - Math.round(t)));
  };
  // back legs fold under it when sitting and trail behind in a jump
  if (stretch > 0.2) { R(-11, 1, 6, 2, pal.dark); R(-13, 0, 3, 1, pal.dark); }
  else { R(-7, 0, 4, 3, pal.dark); R(-8, 0, 3, 1, pal.dark); }
  R(-5, 0, 10, 5, pal.body);
  R(-3, 0, 7, 2, pal.belly || pal.body);
  R(1, 3, 6, 4, pal.body);
  R(3, 7, 2, 2, pal.body); R(4, 7, 1, 1, '#161616');
  R(5, 3, 2, 1, pal.dark);
  R(3, 0, 2, 1, pal.dark);
  R(-3, 4, 2, 1, pal.accent || pal.dark); R(0, 5, 1, 1, pal.accent || pal.dark);
  return { w: 16 * s, h: 10 * s };
}

export const SPECIES_BY_KEY: Record<string, Species> = {};

/** The 30-species catalogue, filled by buildSpecies. */
export const SPECIES: Species[] = [];

/**
 * Build the 30-species catalogue with the game API bound so the onClick
 * handlers keep their prototype bodies. Replaces the registry each call so a
 * fresh world (a new engine, or a test rebuild) gets handlers bound to it.
 */
export function buildSpecies(api: GameApi): Species[] {
  const W = api.world;
  const { reactState, boltState, exitToward, dropGroundItem, playVoice, findEgg } = api;

  const ALL: Species[] = [
    {
      key: 'songbird', name: 'Songbird', w: 10, t: 'D', layer: 'sky', speed: 34, kind: 'bird', pal: { ...P('#E6533E', '#B83A2A', '#F5A64B'), beak: '#F4C542' },
      voice: { freqs: [880, 1046, 1318], gap: 60 }, onClick: function (e) {
        // a startled hop upward, then back to cruising
        reactState(e, 'chirp', 0.6); e.vy = -40; e.data.climbTo = e.y - 14; playVoice(this.voice); findEgg('bird-song');
      },
    },
    {
      key: 'goose', name: 'Canada goose', w: 5, t: 'D', layer: 'sky', speed: 52, kind: 'bird', style: 'goose', group: true,
      pal: { ...P('#8A7A63', '#5B5145', '#D8D0C0'), head: '#1C1C1C', beak: '#2A2A2A' },
      voice: { freqs: [220, 196], gap: 90 }, onClick: function (e) {
        const g = (e.groupMembers as Entity[] | undefined) || [e];
        g.forEach(function (m, i) {
          setTimeout(function () { reactState(m, 'honk', 0.5); api.blip(220 - i * 6, 0.16, 'sawtooth', 0.09); }, i * 140);
        });
        findEgg('goose-honk');
        if (e === g[0] && !W().found['migration']) {
          const f = W().flags;
          f.migrationStep = ((f.migrationStep as number) || 0) + 1;
          const step = f.migrationStep as number;
          // They hold formation for a moment, then carry on: a flock that hung in
          // the air for a minute and a half read as stuck.
          if (step === 1) { g.forEach(function (m) { m.state = 'idle'; m.stateT = 5; }); }
          else if (step === 2) { g.forEach(function (m) { m.state = 'idle'; m.stateT = 5; m.y += (2 - g.indexOf(m) % 2); }); }
          else if (step >= 3) {
            findEgg('migration');
            g.forEach(function (m) { m.state = 'walk'; m.vx = (Math.random() < 0.5 ? -1 : 1) * 60; });
          }
        }
      },
    },
    {
      key: 'seagull', name: 'Seagull', w: 6, t: 'D', layer: 'sky', speed: 30, kind: 'bird', pal: { ...P('#E9EDF0', '#B9C4CC'), beak: '#F4C542' },
      voice: { freqs: [1400, 1200], gap: 80 }, onClick: function (e) { reactState(e, 'screech', 0.6); playVoice(this.voice);
        dropGroundItem('feather', e.x, e); findEgg('gull-scream'); },
    },
    {
      key: 'deer', name: 'Deer', w: 8, t: 'D', layer: 2, layerRange: [1, 2], speed: 22, kind: 'quad',
      shape: shape({ bw: 18, bh: 10, hw: 8, hh: 7, lh: 10, lw: 2, neck: 6, antlers: true, earShape: 'side', ew: 3, eh: 5, snout: 4, tailShape: 'stub', tw: 5, th: 5 }),
      pal: { ...P('#A6693B', '#EAD9BE', '#EAD9BE'), horn: '#D8C7A0' },
      voice: { freqs: [520, 700] }, onClick: function (e) {
        if (e.data.atCampfire) { W().campfireClick(e, 'deer'); return; }
        if (W().carry === 'flower' && !e.data.tame) { e.data.tame = true; W().dropCarry(); reactState(e, 'pet', 1.2); playVoice({ freqs: [660, 880, 1100], gap: 90 }); findEgg('flower-deer'); return; }
        if (e.data.tame) { reactState(e, 'pet', 1); W().pet('deer'); return; }
        reactState(e, 'stare', 2); setTimeout(function () { if (e.state === 'react') boltState(e); }, 1900); playVoice(this.voice); findEgg('deer-stare');
      },
    },
    {
      key: 'bear', name: 'Bear', w: 4, t: 'D', layer: 2, layerRange: [0, 1], speed: 14, size: 1.5, kind: 'quad',
      shape: shape({ bw: 26, bh: 16, hw: 12, hh: 11, lh: 8, lw: 5, ears: true, tail: false, earShape: 'round', hump: 4, snout: 3 }),
      pal: P('#5B4433', '#3C2E22'),
      voice: { freqs: [110, 140], t: 'sawtooth', gap: 120 }, onClick: function (e) {
        const carried = W().carry;
        if (carried && (this.accepts as string[]).indexOf(carried) >= 0) {
          const fed = (e.data.fed = (e.data.fed as Record<string, boolean>) || {});
          fed[carried] = true; W().dropCarry();
          reactState(e, 'happy', 1.3); playVoice({ freqs: [440, 554, 659, 880], gap: 80 });
          if (carried === 'fish') findEgg('bear-fish');
          if (carried === 'honey') findEgg('honey-bear');
          const have = e.data.fed as Record<string, boolean>;
          if (have.fish && have.berry && have.honey) findEgg('bear-feast');
          return;
        }
        reactState(e, 'roar', 1.1); playVoice(this.voice); findEgg('bear-roar');
      }, accepts: ['fish', 'berry', 'honey'],
    },
    {
      key: 'fox', name: 'Fox', w: 8, t: 'A', layer: 2, layerRange: [1, 2], speed: 26, kind: 'quad',
      shape: shape({ bw: 16, bh: 9, tailShape: 'bushy', tw: 10, earShape: 'point', eh: 6, snout: 4 }),
      pal: P('#E2761B', '#F5A64B', '#FFF3E0'),
      voice: { freqs: [700, 900, 1200], gap: 60 }, onClick: function (e) {
        if (e.data.atCampfire) { W().campfireClick(e, 'fox'); return; }
        const carried = W().carry;
        if (carried === 'fish' && !e.data.friend) { e.data.friend = true; W().dropCarry(); reactState(e, 'friend', 1.2);
          playVoice({ freqs: [600, 800, 1000], gap: 70 }); W().flags.foxFriend = true; return; }
        if (e.data.friend && !e.data.fetched && W().flags.boulderMoved) {
          e.state = 'exit'; e.data.exitTarget = W().flags.boulderX; e.dir = (e.data.exitTarget as number) > e.x ? 1 : -1;
          e.vx = Math.abs(e.vx) * e.dir * 1.4;
          e.data.fetchQuest = true; return;
        }
        reactState(e, 'curl', 2.6); playVoice(this.voice); findEgg('fox-nap');
      },
    },
    {
      key: 'wolf', name: 'Wolf', w: 4, t: 'N', layer: 2, layerRange: [1, 2], speed: 24, kind: 'quad',
      shape: shape({ bw: 18, bh: 9, mane: true, earShape: 'point', snout: 5, tailShape: 'long', tw: 11, lh: 10, lw: 2 }),
      pal: P('#6B6B70', '#48484D'),
      voice: { freqs: [300, 240, 180], gap: 180, d: 0.4 }, onClick: function (e) {
        if (e.data.isAnswer) {
          reactState(e, 'howl', 1); playVoice({ freqs: [280, 220], gap: 150, d: 0.3 });
          if (!e.data.answered) {
            e.data.answered = true;
            const n = ((W().flags.packAnswered as number) || 0) + 1;
            W().flags.packAnswered = n;
            if (n >= 3 && !W().found['wolf-pack']) {
              findEgg('wolf-pack');
              W().entities.forEach(function (x) { if (x.sp === SPECIES_BY_KEY.wolf) boltState(x); });
            }
          }
          return;
        }
        reactState(e, 'howl', 1.4); playVoice(this.voice); findEgg('wolf-howl');
        if (!W().flags.packSpawned) {
          W().flags.packSpawned = true; W().flags.packAnswered = 0;
          for (let i = 0; i < 3; i++) {
            const w = W().spawn(SPECIES_BY_KEY.wolf, { x: e.x + (i + 1) * 90 * (e.dir || 1), fromLeft: e.dir > 0 });
            w.data.isAnswer = true;
          }
        }
      },
    },
    {
      key: 'squirrel', name: 'Squirrel', w: 9, t: 'D', layer: 2, layerRange: [1, 2], speed: 44, kind: 'quad',
      shape: shape({ bw: 11, bh: 7, hw: 6, hh: 6, lh: 4, lw: 2, tw: 7, th: 9, tailShape: 'bushy', earShape: 'point', eh: 4, gait: 'hop' }),
      pal: P('#B5652B', '#8A4A1E'),
      voice: { freqs: [1200, 1500, 1200], gap: 50 }, onClick: function (e) {
        const carried = W().carry;
        if (carried && (carried === 'acorn' || carried === 'pinecone')) {
          W().dropCarry(); reactState(e, 'bury', 1);
          playVoice({ freqs: [900, 1100], gap: 60 });
          if (carried === 'acorn') findEgg('acorn-squirrel'); else findEgg('pinecone-squirrel'); return;
        }
        reactState(e, 'nut', 1); dropGroundItem('acorn', e.x, e); playVoice(this.voice); findEgg('squirrel-nut');
      },
    },
    {
      key: 'hedgehog', name: 'Hedgehog', w: 5, t: 'A', layer: 2, speed: 12, kind: 'quad',
      shape: shape({ bw: 12, bh: 8, ears: false, tail: false, spikes: true, snout: 3, lh: 3 }),
      pal: P('#8A7A63', '#5E5142'),
      voice: { freqs: [500, 400], gap: 100 }, onClick: function (e) {
        const carried = W().carry;
        if (carried === 'berry' && !e.data.friend) {
          e.data.friend = true; W().dropCarry(); reactState(e, 'happy', 1);
          playVoice({ freqs: [600, 800], gap: 80 }); findEgg('berry-hog'); return;
        }
        reactState(e, 'ball', 1.4); playVoice(this.voice); findEgg('hog-ball');
      },
    },
    {
      key: 'rabbit', name: 'Rabbit', w: 9, t: 'A', layer: 2, layerRange: [1, 2], speed: 50, kind: 'quad',
      shape: shape({ bw: 10, bh: 9, hw: 6, hh: 6, lh: 5, lw: 2, ears: true, eh: 6, earShape: 'long', tailShape: 'stub', tw: 4, th: 4, gait: 'hop' }),
      pal: P('#C8B79A', '#9C8C70', '#F2E9DA'),
      voice: { freqs: [900, 900], gap: 120 }, onClick: function (e) {
        if (e.data.atCampfire) { W().campfireClick(e, 'rabbit'); return; }
        reactState(e, 'thump', 0.8); playVoice(this.voice);
        setTimeout(function () { if (e.state !== 'gone') boltState(e); }, 750); findEgg('rabbit-thump');
      },
    },
    {
      key: 'owl', name: 'Owl', w: 6, t: 'N', layer: 1, speed: 0, kind: 'bird', style: 'owl', perch: true, pal: P('#7C6A4E', '#584B37', '#C9B27A'),
      voice: { freqs: [300, 240], gap: 260, d: 0.3 }, onClick: function (e) { reactState(e, 'rotate', 1.4); playVoice(this.voice); findEgg('owl-spin'); },
    },
    {
      key: 'bat', name: 'Bat', w: 6, t: 'N', layer: 'sky', speed: 60, kind: 'bird', style: 'bat', pal: P('#3C3440', '#5A4E62'),
      voice: { freqs: [2200, 2600], gap: 40, d: 0.05 }, onClick: function (e) { reactState(e, 'loop', 1); playVoice(this.voice); findEgg('bat-loop'); },
    },
    {
      key: 'frog', name: 'Frog', w: 5, t: 'A', layer: 'pond', speed: 0, kind: 'quad', style: 'frog',
      shape: shape({ bw: 10, bh: 7, hw: 8, hh: 6, lh: 3, lw: 2, ears: false, tail: false, snout: 2 }),
      pal: P('#5E9E4A', '#3D7031', '#CFE7A0'),
      voice: { freqs: [180], t: 'square', d: 0.12 }, onClick: function (e) { reactState(e, 'jump', 0.7); playVoice(this.voice);
        e.data.jumpTo = e.x + (Math.random() < 0.5 ? -1 : 1) * 60; findEgg('frog-jump'); },
    },
    {
      key: 'duck', name: 'Duck', w: 5, t: 'D', layer: 'pond', speed: 10, kind: 'bird', style: 'duck', swim: true,
      pal: { ...P('#8A6A44', '#6B4F2E', '#C9B27A'), head: '#2F7A4A', beak: '#F4C542' },
      voice: { freqs: [520, 470], gap: 100 }, onClick: function (e) { reactState(e, 'quack', 0.6); playVoice(this.voice);
        const n = ((W().flags.duckClicks as number) || 0) + 1;
        W().flags.duckClicks = n;
        if (n >= 5) { e.data.line = true; findEgg('duck-line'); } },
    },
    {
      key: 'turtle', name: 'Turtle', w: 3, t: 'D', layer: 2, speed: 6, kind: 'quad',
      shape: shape({ bw: 14, bh: 9, ears: false, lh: 3, shell: true, neck: 3, tailShape: 'none' }),
      pal: P('#4E7A4A', '#345234', '#8FC77A'),
      voice: { freqs: [220], t: 'square' }, onClick: function (e) { reactState(e, 'shell', 3); playVoice(this.voice); findEgg('turtle-hide'); },
    },
    {
      key: 'butterfly', name: 'Butterfly', w: 4, t: 'D', layer: 2, speed: 20, kind: 'flutter', pal: P('#E6659E', '#F2A6C6'),
      variants: [P('#E6659E', '#F2A6C6'), P('#F4A23A', '#F7D070'), P('#6FA8FF', '#B7D3FF'), P('#F2F4F6', '#D9E6BE')],
      voice: { freqs: [1400, 1700], gap: 70 }, onClick: function (e) { reactState(e, 'land', 8); playVoice(this.voice); findEgg('butterfly-land'); },
    },
    {
      key: 'firefly', name: 'Firefly', w: 4, t: 'N', layer: 2, speed: 8, kind: 'glow', pal: P('#CFFF7A', '#CFFF7A'),
      voice: { freqs: [1800], d: 0.08 }, onClick: function (e) {
        reactState(e, 'glow', 1.2); playVoice(this.voice);
        const fc = (W().flags.fireflyClicks = (W().flags.fireflyClicks as Record<string, boolean>) || {});
        fc[e.uid as string] = true;
        const n = Object.keys(fc).length;
        if (n >= 3 && !W().found['firefly-sync']) findEgg('firefly-sync');
        if (W().isNight() && n >= 7 && !W().flags.swarmSpawned) { W().flags.swarmSpawned = true; W().spawnSwarm(); }
        // F10: an empty-handed click lets you carry one (there is no ground item)
        if (!W().carry) { e.state = 'gone'; W().pickUp('firefly', e.x, e.y); }
      },
    },
    {
      key: 'crow', name: 'Crow', w: 5, t: 'D', layer: 1, speed: 0, kind: 'bird', perch: true, pal: { ...P('#2E2C34', '#141318'), beak: '#4A4A52' },
      voice: { freqs: [300], t: 'sawtooth', d: 0.18 }, onClick: function (e) {
        reactState(e, 'caw', 0.7); playVoice(this.voice);
        // the rest of the flock bursts out from behind it and flies off
        const at = W().anchorOf(e);
        for (let i = 0; i < 3; i++) {
          const c = W().spawn(SPECIES_BY_KEY.crow, { airborne: { x: at.sx, y: at.cy }, fromLeft: Math.random() < 0.5 });
          c.vx = (c.dir || 1) * (90 + i * 20);
        }
        findEgg('crow-scatter');
      },
    },
    {
      key: 'woodpecker', name: 'Woodpecker', w: 4, t: 'D', layer: 2, speed: 0, kind: 'bird', perch: true,
      pal: { ...P('#2A2A2E', '#F2F4F6', '#F2F4F6'), head: '#D0392B', beak: '#8B8B93' },
      voice: { freqs: [1000, 1000, 1000], gap: 40, d: 0.03 }, onClick: function (e) {
        reactState(e, 'peck', 1); playVoice(this.voice);
        const oak = e.perch && e.perch.prop.key === 'oak' ? e.perch.prop : W().nearestProp(e.x, 'oak');
        if (oak) { oak.hole = true; oak.shakeT = 0.3; }
        findEgg('pecker-hole');
      },
    },
    {
      key: 'raccoon', name: 'Raccoon', w: 4, t: 'N', layer: 2, speed: 24, kind: 'quad',
      shape: shape({ bw: 13, bh: 8, lh: 3, lw: 2, tw: 9, th: 6, tailShape: 'bushy', earShape: 'round', ew: 4, eh: 4, mask: true, snout: 3 }),
      pal: P('#7C7A78', '#4E4C4A', '#D8D5CE'),
      voice: { freqs: [500, 600, 500], gap: 60 }, onClick: function (e) {
        reactState(e, 'guilty', 0.8); playVoice(this.voice);
        setTimeout(function () { if (e.state !== 'gone') boltState(e); }, 750); findEgg('raccoon-guilt');
      },
    },
    {
      key: 'moose', name: 'Moose', w: 1, t: 'D', layer: 1, layerRange: [0, 1], speed: 10, size: 1.7, kind: 'quad',
      shape: shape({
        bw: 28, bh: 17, hw: 12, hh: 11, lh: 13, lw: 3, tail: false, antlers: true, antlerShape: 'moose', neck: 5, hump: 5,
        snout: 7, droop: true, earShape: 'side', ew: 3, eh: 5,
      }),
      pal: { ...P('#4A3B2E', '#2E241B'), horn: '#CDB98C' },
      voice: { freqs: [90], t: 'sawtooth', d: 0.6 }, onClick: function (e) { reactState(e, 'bellow', 1.2); playVoice(this.voice); findEgg('moose-bellow'); },
    },
    {
      key: 'boar', name: 'Boar', w: 3, t: 'A', layer: 2, layerRange: [0, 1], speed: 20, kind: 'quad',
      shape: shape({ bw: 16, bh: 10, ears: true, earShape: 'round', ew: 2, eh: 3, tusks: true, snout: 5, hump: 3, lh: 4, tailShape: 'stub', tw: 4, th: 3 }),
      pal: P('#4A4038', '#302A24'),
      voice: { freqs: [160, 200], t: 'sawtooth' }, onClick: function (e) {
        reactState(e, 'dig', 1); playVoice(this.voice);
        dropGroundItem('truffle', e.x, e); findEgg('boar-truffle');
      },
    },
    {
      key: 'mouse', name: 'Mouse', w: 3, t: 'A', layer: 2, speed: 60, kind: 'quad',
      shape: shape({ bw: 7, bh: 5, hw: 4, hh: 4, lh: 2, lw: 1, tw: 9, th: 1, tailShape: 'long', earShape: 'round', ew: 2, eh: 3, snout: 2 }),
      pal: P('#9C9084', '#6E6459'),
      voice: { freqs: [2000, 2200], gap: 40, d: 0.05 }, onClick: function (e) {
        reactState(e, 'hide', 0.6); playVoice(this.voice);
        const rock = W().nearestProp(e.x, 'rock'); exitToward(e, rock ? rock.x : e.x + (e.dir || 1) * 80); findEgg('mouse-hide');
      },
    },
    {
      key: 'badger', name: 'Badger', w: 2, t: 'N', layer: 2, speed: 14, kind: 'quad',
      shape: shape({ bw: 14, bh: 9, lh: 4, mask: true, snout: 4, earShape: 'round', tailShape: 'stub' }),
      pal: P('#B9B4AC', '#3A3A3A'),
      voice: { freqs: [200, 150] }, onClick: function (e) { reactState(e, 'dig', 0.8); playVoice(this.voice); e.data.willVanish = true; },
    },
    {
      key: 'cat', name: 'Cat', w: 0, t: 'A', layer: 2, speed: 20, kind: 'quad',
      shape: shape({
        bw: 13, bh: 6, hw: 7, hh: 6, lh: 5, lw: 2, th: 10, tailShape: 'cat', earShape: 'point', ew: 2.4, eh: 4,
        stripes: true, whiskers: true,
      }),
      pal: { ...P('#8E8E96', '#5E5E66', '#ECE7DA'), eye: '#3F8A2A' },
      gate: function () { return (W().t - (W().lastInteractAt || 0)) > 60; },
      voice: { freqs: [600, 700, 600], gap: 110 }, onClick: function (e) {
        reactState(e, 'purr', 2); W().pet('cat'); playVoice(this.voice);
        e.data.follow = true;
      },
    },
    {
      key: 'dragonfly', name: 'Dragonfly', w: 6, t: 'D', layer: 'pond', speed: 26, kind: 'flutter', style: 'dragonfly', pal: P('#3EC7C0', '#63E0DA'),
      voice: { freqs: [1700, 2000], gap: 30, d: 0.05 }, onClick: function (e) { reactState(e, 'zip', 0.7); playVoice(this.voice); },
    },
    {
      key: 'bee', name: 'Bee', w: 4, t: 'D', layer: 2, speed: 30, kind: 'flutter', style: 'bee', pal: P('#F4C542', '#2A2A2A'),
      voice: { freqs: [300], t: 'sawtooth', d: 0.4 }, onClick: function (e) {
        reactState(e, 'buzz', 0.6); playVoice(this.voice);
        const n = ((W().flags.beeFollows as number) || 0) + 1;
        W().flags.beeFollows = n;
        if (n >= 3) findEgg('bee-hive');
      },
    },
    {
      key: 'snail', name: 'Snail', w: 1, t: 'A', layer: 2, speed: 3, kind: 'quad',
      shape: shape({ bw: 9, bh: 7, ears: false, tail: false, legs: false, snail: true }),
      pal: P('#D9C9A0', '#B5652B'),
      voice: { freqs: [400], t: 'sine', d: 0.2 }, onClick: function (e) {
        reactState(e, 'retract', 1); playVoice(this.voice);
        if (W().nearPond(e.x)) { W().dropCarry(); findEgg('snail-lily'); return; }
        // F13: away from the water, an empty-handed click lets you carry it to the pond
        if (!W().carry) { e.state = 'gone'; W().pickUp('snail', e.x, e.y); }
      },
    },
    {
      key: 'otter', name: 'Otter', w: 2, t: 'D', layer: 'pond', speed: 14, kind: 'quad',
      shape: shape({ bw: 14, bh: 8, tw: 10, th: 3, tailShape: 'long', earShape: 'round', ew: 2, eh: 2, snout: 3, lh: 3 }),
      pal: P('#5E4A38', '#3E3024'),
      voice: { freqs: [700, 850], gap: 80 }, onClick: function (e) {
        if (W().carry === 'fish') {
          W().dropCarry(); reactState(e, 'juggle', 1.4); playVoice({ freqs: [700, 900, 1100], gap: 70 }); findEgg('fish-otter'); return;
        }
        reactState(e, 'float', 1.2); playVoice(this.voice);
      },
    },
    {
      key: 'eagle', name: 'Eagle', w: 1, t: 'D', layer: 'sky', speed: 70, kind: 'bird', size: 1.3, pal: { ...P('#5A4A38', '#3A2E22'), head: '#F2F4F6', beak: '#F4C542' },
      voice: { freqs: [1600, 1300], gap: 90 }, onClick: function (e) { reactState(e, 'dive', 1); playVoice(this.voice); },
    },
    {
      key: 'fish', name: 'Fish', w: 0, t: 'A', layer: 'pond', speed: 0, kind: 'fish',
      shape: shape({ bw: 12, bh: 6, ears: false, tail: true, legs: false, tw: 6, th: 5 }),
      pal: P('#6FA8DC', '#3D6FA8', '#DCE9F5'),
      voice: { freqs: [900, 1200], gap: 50 },
      onClick: function (e) { /* caught mid-air */
        if (e.data.caught) return;
        e.data.caught = true; e.state = 'gone';
        W().pickUp('fish', e.x, e.y);
        findEgg('pond-fish');
      },
    },
  ];

  SPECIES.length = 0;
  ALL.forEach((s) => { SPECIES_BY_KEY[s.key] = s; SPECIES.push(s); });
  return ALL;
}
