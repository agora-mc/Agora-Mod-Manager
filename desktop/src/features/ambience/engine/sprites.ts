/**
 * Pixel-art sprites for the fixed props.
 *
 * Every prop is drawn from filled rectangles on the world's own grid: no
 * paths, no rotation, no images. A prop is described bottom-up from the spot
 * it stands on, so `B(x, y, w, h, colour)` reads as "a block `w` wide and `h`
 * tall whose bottom edge is `y` above the ground, `x` from the centre".
 *
 * Colours pass through the daylight shader, so the scene dims together at
 * night instead of leaving bright props standing on dark hills. Things that
 * give off light (fire, eyes in the cave) skip it.
 */

import type { Prop } from './types';

type Ctx = CanvasRenderingContext2D;

export interface SpriteEnv {
  /** World time in seconds (drives idle sway, flames, ants). */
  t: number;
  /** Reduced motion: draw the resting pose. */
  reduce: boolean;
  snow: boolean;
  /** Shade a colour for the current daylight. */
  sh: (c: string) => string;
  /** Drawn ground y at a world-x offset from the prop's own x. */
  groundAt: (dx: number) => number;
}

/* ── daylight shading ───────────────────────────────────────────────── */

const NIGHT = [14, 22, 48];
const LEVELS = 12;
const shadeCache = new Map<string, string[]>();

function parseColor(c: string): [number, number, number] | null {
  if (c.charCodeAt(0) === 35 && c.length === 7) {
    return [parseInt(c.slice(1, 3), 16), parseInt(c.slice(3, 5), 16), parseInt(c.slice(5, 7), 16)];
  }
  const m = /^rgb\((\d+),\s*(\d+),\s*(\d+)\)$/.exec(c);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

const identity = (c: string): string => c;

/**
 * A colour shader for a daylight level (`dl`: 0 = deep night, 1 = noon).
 * Full colour by day; towards night everything leans into the sky's blue.
 * Anything it cannot parse (an rgba string) is returned untouched.
 */
export function shader(dl: number): (c: string) => string {
  const level = Math.round((1 - Math.min(1, Math.max(0, dl) / 0.6)) * LEVELS);
  if (level <= 0) return identity;
  const k = (level / LEVELS) * 0.52;
  return (c: string): string => {
    let row = shadeCache.get(c);
    if (!row) { row = []; shadeCache.set(c, row); }
    let out = row[level];
    if (out === undefined) {
      const rgb = parseColor(c);
      out = rgb
        ? 'rgb(' + Math.round(rgb[0] + (NIGHT[0] - rgb[0]) * k) + ',' + Math.round(rgb[1] + (NIGHT[1] - rgb[1]) * k) + ',' + Math.round(rgb[2] + (NIGHT[2] - rgb[2]) * k) + ')'
        : c;
      row[level] = out;
    }
    return out;
  };
}

/* ── geometry shared with the perch spots and the layout ────────────── */

/** Where the hive hangs, measured from its oak's base. */
export const HIVE_DX = 12;
export const HIVE_LIFT = 8;
/** How far a rolled boulder comes to rest from the hole it covered. */
export const BOULDER_ROLL = 28;
/** Lily pads sit at these fractions of the water's width. */
export const LILY_FRACTIONS = [0.16, 0.38, 0.62, 0.84];

/* ── the sprites ────────────────────────────────────────────────────── */

/**
 * Draw one prop. `sx`/`gy` are its drawn foot position, `s` its layer scale
 * and `wig` the sideways wobble of a prop that was just clicked.
 */
export function drawPropSprite(ctx: Ctx, p: Prop, sx: number, gy: number, s: number, wig: number, env: SpriteEnv): void {
  const sh = env.sh;
  const cx = sx + wig;
  /** Block: bottom edge `y` above the ground, `x` from the centre. */
  const B = (x: number, y: number, w: number, h: number, c: string, raw?: boolean): void => {
    const l = Math.round(cx + x * s), r = Math.round(cx + (x + w) * s);
    const b = Math.round(gy - y * s), t = Math.round(gy - (y + h) * s);
    ctx.fillStyle = raw ? c : sh(c);
    ctx.fillRect(l, t, Math.max(1, r - l), Math.max(1, b - t));
  };
  const sway = (speed: number, phase: number, amp: number): number =>
    env.reduce ? 0 : Math.sin(env.t * speed + phase) * amp;
  const W = (p.w as number) || 20;

  switch (p.key) {
    case 'oak': {
      // trunk, roots, the shaded side
      B(-4, 0, 8, 28, '#6B4A2E');
      B(2, 0, 2, 28, '#55381F');
      B(-6, 0, 2, 3, '#6B4A2E'); B(4, 0, 2, 3, '#55381F');
      B(-3, 8, 1, 5, '#55381F'); B(-1, 18, 1, 4, '#55381F');
      if (p.hole) { B(-2, 12, 4, 6, '#1A1410'); B(-1, 18, 2, 1, '#1A1410'); }
      // canopy: a stepped blob, dark underneath and lit from the top left
      B(-13, 24, 26, 6, '#2F6A2C');
      B(-17, 30, 34, 20, '#3F8A38');
      B(-14, 50, 28, 8, '#3F8A38');
      B(-9, 58, 18, 6, '#3F8A38');
      B(4, 30, 11, 6, '#2F6A2C'); B(-17, 30, 6, 4, '#2F6A2C'); B(8, 44, 6, 4, '#2F6A2C');
      B(-13, 44, 10, 6, '#5BA84A'); B(-8, 54, 8, 4, '#5BA84A'); B(1, 37, 7, 4, '#5BA84A'); B(-4, 60, 6, 2, '#6DBB58');
      if (env.snow) { B(-9, 62, 18, 2, '#F4F8FB'); B(-14, 56, 5, 2, '#F4F8FB'); B(9, 56, 5, 2, '#F4F8FB'); B(-17, 48, 3, 2, '#F4F8FB'); }
      break;
    }
    case 'pine': {
      B(-2, 0, 5, 14, '#5A3C24'); B(1, 0, 2, 14, '#452C19');
      const rows: Array<[number, number]> = [[12, 26], [18, 20], [24, 22], [30, 16], [36, 18], [42, 12], [48, 12], [54, 8], [60, 4]];
      rows.forEach(([y, w], i) => {
        B(-w / 2, y, w, 6, i % 2 ? '#2C5E3E' : '#28563A');
        B(-w / 2, y + 4, Math.min(6, w / 2), 2, '#3F7A52');
        if (env.snow && i % 2 === 0) B(-w / 2, y + 5, w, 1, '#F4F8FB');
      });
      B(-1, 66, 2, 4, '#2C5E3E');
      if (env.snow) B(-1, 69, 2, 1, '#F4F8FB');
      break;
    }
    case 'flowers': {
      const cols = ['#E6659E', '#F4C542', '#8FD1E8', '#F2F4F6', '#C58BFF'];
      const hs = [7, 9, 8, 10, 7];
      for (let i = 0; i < 5; i++) {
        const x = (i - 2) * 6, h = hs[i];
        const lean = sway(1.5, i * 1.7, 0.7);
        // each stem roots in the ground under it, so a patch on a slope
        // steps down the slope instead of hanging in the air
        const g = -(env.groundAt(x) - gy) / s;
        B(x, g, 1, h, '#3B7A34');
        B(x + (i % 2 ? 1 : -2), g + 2 + (i % 3), 2, 1, '#4E9A44');
        if (p.bloom) {
          B(x - 2 + lean, g + h + 1, 5, 2, cols[i]);
          B(x - 1 + lean, g + h, 3, 4, cols[i]);
          B(x + lean, g + h + 1, 1, 2, i === 1 ? '#B5651D' : '#F4C542');
        } else {
          B(x - 1 + lean, g + h, 3, 3, '#4E9A44');
          B(x + lean, g + h + 2, 1, 1, cols[i]);
        }
      }
      break;
    }
    case 'rock': {
      if (!p.tipped) {
        B(-10, 0, 20, 8, '#77777F'); B(-8, 8, 16, 4, '#77777F'); B(-5, 12, 10, 2, '#8B8B93');
        B(-10, 0, 20, 2, '#5C5C64'); B(4, 2, 6, 5, '#66666E');
        B(-7, 9, 6, 2, '#A5A5AD'); B(-8, 5, 3, 2, '#8B8B93');
        B(-10, 6, 4, 2, '#4E7A3A'); B(-9, 8, 2, 1, '#5E9E4A');
      } else {
        // the damp patch it was sitting on, and the rock turned over beside it
        B(-9, 0, 17, 2, '#2B2118'); B(-6, 2, 4, 1, '#3A2D20');
        const ox = 13, dy = (env.groundAt(ox) - gy) / s;
        B(ox - 8, -dy, 16, 3, '#6A5A48'); B(ox - 10, 3 - dy, 20, 7, '#77777F'); B(ox - 7, 10 - dy, 14, 3, '#77777F');
        B(ox - 10, 3 - dy, 20, 2, '#5C5C64'); B(ox - 6, 8 - dy, 6, 2, '#A5A5AD'); B(ox + 3, 5 - dy, 5, 3, '#66666E');
        // a beetle makes a run for it
        const bt = (p.beetleT as number) || 0;
        if (bt > 0 && !env.reduce) {
          const bx = -4 - (2.4 - bt) * 16;
          const by = (env.groundAt(bx) - gy) / s;
          B(bx, -by, 4, 2, '#1A1410'); B(bx - 1, -by, 1, 1, '#1A1410');
          B(bx + (Math.floor(env.t * 14) % 2), -by - 1 + 1, 1, 1, '#3A2D20');
        }
      }
      break;
    }
    case 'log': {
      const half = W / 2;
      B(-half + 3, 0, W - 6, 10, '#7A5433');
      B(-half + 3, 8, W - 6, 2, '#94683F');
      B(-half + 3, 0, W - 6, 2, '#5A3C24');
      for (let x = -half + 8; x < half - 10; x += 9) B(x, 3 + ((x & 1) ? 2 : 0), 5, 1, '#5A3C24');
      B(-half, 1, 4, 8, '#C99B66'); B(-half + 1, 3, 2, 4, '#A67843'); B(-half, 1, 4, 1, '#A67843');
      B(half - 4, 1, 4, 8, '#5A3C24');
      B(half * 0.3, 10, 3, 3, '#7A5433'); B(half * 0.3, 12, 3, 1, '#94683F');
      B(-half * 0.5, 4, 3, 3, '#5A3C24');
      if (p.shrooms) {
        for (let m = 0; m < 4; m++) {
          const x = -half + 9 + m * (W - 18) / 3.4;
          B(x, 10, 1, 2 + (m % 2), '#E9E4D4');
          B(x - 2, 12 + (m % 2), 5, 2, '#D0392B'); B(x - 1, 14 + (m % 2), 3, 1, '#D0392B');
          B(x - 1, 13 + (m % 2), 1, 1, '#F2F4F6');
        }
      }
      break;
    }
    case 'bush': {
      B(-12, 0, 24, 9, '#2E5E2A'); B(-9, 9, 18, 5, '#2E5E2A'); B(-5, 14, 10, 2, '#2E5E2A');
      B(-12, 0, 24, 2, '#234A20'); B(3, 2, 7, 4, '#234A20');
      B(-8, 10, 6, 2, '#44803C'); B(-10, 5, 4, 2, '#44803C'); B(-2, 14, 5, 1, '#44803C');
      if (p.berries) {
        const spots: Array<[number, number]> = [[-8, 5], [-3, 9], [2, 4], [6, 9], [-1, 2]];
        spots.forEach(([x, y]) => { B(x, y, 2, 2, '#5B3FE0'); B(x, y + 1, 1, 1, '#9C8BFF'); });
      }
      break;
    }
    case 'mushroom': {
      const big = ((p.ringIdx as number) || 0) % 2 === 0;
      const hop = !env.reduce && (p.shakeT as number) > 0 ? Math.abs(Math.sin((p.shakeT as number) * 22)) * 3 : 0;
      const k = big ? 1 : 0.8;
      B(-1 * k, hop, 3 * k, 4 * k, '#E9E4D4');
      B(-4 * k, 4 * k + hop, 9 * k, 3 * k, '#D0392B'); B(-3 * k, 7 * k + hop, 7 * k, 2 * k, '#D0392B'); B(-1 * k, 9 * k + hop, 3 * k, 1, '#E0584A');
      B(-4 * k, 4 * k + hop, 9 * k, 1, '#A32A20');
      B(-2 * k, 6 * k + hop, 1, 1, '#F2F4F6'); B(2 * k, 5 * k + hop, 1, 1, '#F2F4F6'); B(0, 8 * k + hop, 1, 1, '#F2F4F6');
      break;
    }
    case 'hive': {
      // hangs under the oak's canopy; `gy` is the oak's own ground
      B(0, 22, 1, 3, '#5A3C24');
      B(-4, 8, 8, 3, '#D9A441'); B(-5, 11, 10, 4, '#E8B85A'); B(-4, 15, 8, 3, '#D9A441'); B(-3, 18, 6, 2, '#C28E2E'); B(-2, 20, 4, 2, '#C28E2E');
      B(-5, 11, 10, 1, '#B07F25'); B(-4, 15, 8, 1, '#B07F25'); B(-3, 18, 6, 1, '#A8751F');
      B(-1, 9, 2, 2, '#3A2A12');
      B(-4, 13, 2, 1, '#F4D58A');
      if (p.honey) { B(1, 4, 1, 4, '#E8A317'); B(0, 3, 2, 2, '#E8A317'); }
      break;
    }
    case 'campfire': {
      B(-10, 0, 4, 3, '#77777F'); B(-6, 0, 3, 2, '#8B8B93'); B(3, 0, 3, 2, '#8B8B93'); B(6, 0, 4, 3, '#77777F'); B(-2, 0, 4, 2, '#66666E');
      B(-7, 2, 14, 2, '#6B4A2E'); B(-5, 4, 10, 2, '#5A3C24'); B(-7, 2, 2, 2, '#C99B66'); B(5, 2, 2, 2, '#C99B66');
      if (p.lit) {
        const f = env.reduce ? 0 : Math.sin(env.t * 18) * 1.5;
        const g = env.reduce ? 0 : Math.sin(env.t * 11 + 1) * 1;
        ctx.globalAlpha = 0.12; B(-18, 0, 36, 24, '#FFB05A', true); ctx.globalAlpha = 1;
        B(-4, 6, 8, 5 + f, '#E2571B', true); B(-3, 11 + f, 6, 3, '#E2571B', true); B(-1, 14 + f, 2, 2 + g, '#E2571B', true);
        B(-3, 6, 6, 4 + f, '#FF8A3E', true); B(-2, 10 + f, 4, 2 + g, '#FF8A3E', true);
        B(-1, 6, 3, 3 + f / 2, '#FFD84D', true);
        if (!env.reduce) {
          for (let i = 0; i < 3; i++) {
            const y = 16 + ((env.t * 22 + i * 9) % 18);
            B(-3 + i * 3 + Math.sin(env.t * 3 + i) * 2, y, 1, 1, '#FFD84D', true);
          }
        }
      } else {
        B(-2, 6, 4, 1, '#2B2118'); B(-1, 7, 2, 1, '#3A2D20');
      }
      break;
    }
    case 'stump': {
      B(-8, 0, 16, 7, '#6B4A2E'); B(3, 0, 5, 7, '#55381F');
      B(-10, 0, 2, 3, '#6B4A2E'); B(8, 0, 2, 2, '#55381F');
      B(-5, 2, 1, 4, '#55381F'); B(-1, 1, 1, 3, '#55381F');
      B(-8, 7, 16, 3, '#C99B66'); B(-6, 8, 12, 1, '#A67843'); B(-2, 8, 4, 1, '#8A6234'); B(-8, 7, 16, 1, '#A67843');
      const stage = (p.sprout as number) || 0;
      if (stage > 0) {
        B(5, 10, 1, 2 * stage, '#4E9A44');
        if (stage >= 2) B(6, 8 + 2 * stage, 2, 1, '#6DBB58');
        if (stage >= 3) { B(3, 9 + 2 * stage, 2, 1, '#6DBB58'); B(4, 10 + 2 * stage, 3, 2, '#F2A6C6'); B(5, 11 + 2 * stage, 1, 1, '#F4C542'); }
      }
      break;
    }
    case 'cave': {
      const H = (p.h as number) || 40;
      void H;
      B(-24, 0, 48, 26, '#4A4A52'); B(-20, 26, 40, 10, '#4A4A52'); B(-13, 36, 26, 6, '#55555D');
      B(-24, 0, 48, 3, '#3A3A42'); B(12, 6, 10, 12, '#3F3F47');
      B(-18, 30, 8, 3, '#6A6A72'); B(-22, 16, 5, 4, '#6A6A72'); B(6, 37, 6, 2, '#6A6A72');
      B(-22, 22, 6, 2, '#4E7A3A'); B(14, 26, 6, 2, '#4E7A3A');
      B(-12, 0, 24, 18, '#0E0B09'); B(-9, 18, 18, 6, '#0E0B09'); B(-5, 24, 10, 3, '#0E0B09');
      if (p.opened) { B(-4, 1, 8, 5, '#B8860B', true); B(-3, 5, 6, 2, '#FFD84D', true); B(-1, 7, 2, 1, '#FFF1A8', true); }
      if ((!env.reduce && (p.blink as number) > 0) || p.glow) {
        const c = p.glow ? '#CFFF7A' : '#FFFFFF';
        B(-5, 12, 2, 2, c, true); B(3, 12, 2, 2, c, true);
      }
      break;
    }
    case 'scarecrow': {
      const sw = (!env.reduce && (p.spin as number) > 0) ? Math.sin((p.spin as number) * 30) * 3 : 0;
      B(-1, 0, 3, 34, '#8A6A3A'); B(1, 0, 1, 34, '#6B4F28');
      B(-12 + sw, 22, 24, 2, '#8A6A3A');
      B(-14 + sw, 20, 3, 5, '#E9C88A'); B(11 + sw, 20, 3, 5, '#E9C88A');
      B(-5 + sw / 2, 14, 10, 10, '#B0503A'); B(-5 + sw / 2, 14, 10, 2, '#8A3B2A');
      B(-3 + sw / 2, 17, 3, 3, '#D9A441'); B(2 + sw / 2, 20, 2, 2, '#3F6FA0');
      B(-5 + sw / 2, 12, 2, 2, '#E9C88A'); B(-1 + sw / 2, 11, 2, 3, '#E9C88A'); B(3 + sw / 2, 12, 2, 2, '#E9C88A');
      B(-4 + sw, 26, 8, 7, '#E9C88A'); B(-4 + sw, 26, 8, 1, '#C9A565');
      B(-2 + sw, 29, 1, 2, '#3A2A12'); B(1 + sw, 29, 1, 2, '#3A2A12'); B(-2 + sw, 27, 4, 1, '#8A3B2A');
      B(-6 + sw, 33, 12, 2, '#5A3C24'); B(-3 + sw, 35, 6, 4, '#5A3C24'); B(-3 + sw, 35, 6, 1, '#B0503A');
      if (p.feather) { B(2 + sw, 38, 2, 6, '#F2F4F6'); B(3 + sw, 40, 1, 3, '#B9C4CC'); }
      break;
    }
    case 'cattails': {
      const xs = [-4, 0, 4], hs = [18, 24, 20];
      for (let i = 0; i < 3; i++) {
        const lean = sway(1.2, i * 2.1, 1);
        B(xs[i], 0, 1, hs[i], '#5E8A3A');
        B(xs[i] + 2, 0, 1, hs[i] * 0.5, '#4E7A3A');
        B(xs[i] - 1 + lean, hs[i] - 7, 3, 6, '#6B4A2E'); B(xs[i] - 1 + lean, hs[i] - 2, 3, 1, '#8A6234');
        B(xs[i] + lean, hs[i], 1, 2, '#C9B27A');
      }
      break;
    }
    case 'boulder': {
      const roll = Math.min(1, (p.rollT as number) ?? (p.rolled ? 1 : 0));
      if (p.rolled) {
        // the burrow it was hiding
        B(-10, -5, 20, 5, '#120D09', true); B(-7, -8, 14, 3, '#120D09', true);
        B(-13, 0, 4, 2, '#6B5234'); B(9, 0, 4, 2, '#6B5234'); B(-11, 0, 22, 1, '#3A2A18');
      }
      const ease = roll * roll * (3 - 2 * roll);
      const ox = p.rolled ? BOULDER_ROLL * ease : 0;
      const dy = p.rolled ? ((env.groundAt(ox) - gy) / s) * ease : 0;
      const bob = p.rolled && roll < 1 ? Math.abs(Math.sin(roll * Math.PI * 2)) * 2 : 0;
      const y0 = -dy + bob;
      B(ox - 15, y0, 30, 12, '#63636B'); B(ox - 13, y0 + 12, 26, 6, '#63636B'); B(ox - 9, y0 + 18, 18, 4, '#6F6F77');
      B(ox - 15, y0, 30, 3, '#4A4A52'); B(ox + 6, y0 + 3, 9, 8, '#55555D');
      B(ox - 10, y0 + 16, 8, 2, '#8B8B93'); B(ox - 12, y0 + 9, 4, 4, '#8B8B93'); B(ox - 4, y0 + 20, 6, 1, '#9A9AA2');
      B(ox - 15, y0 + 5, 3, 2, '#4E7A3A');
      // each push opens another crack, so the clicks visibly add up
      const clicks = p.rolled ? 0 : Math.min(4, (p.clicks as number) || 0);
      if (clicks >= 1) { B(-2, 14, 1, 6, '#2B2B30'); B(-1, 12, 1, 3, '#2B2B30'); }
      if (clicks >= 2) { B(-6, 9, 5, 1, '#2B2B30'); B(-7, 6, 1, 3, '#2B2B30'); }
      if (clicks >= 3) { B(0, 8, 1, 4, '#2B2B30'); B(1, 5, 4, 1, '#2B2B30'); }
      if (clicks >= 4) { B(5, 12, 1, 5, '#2B2B30'); B(-11, 3, 4, 1, '#2B2B30'); B(4, 2, 1, 3, '#2B2B30'); }
      break;
    }
    case 'signpost': {
      B(-1, 0, 3, 26, '#6B4A2E'); B(1, 0, 1, 26, '#55381F');
      B(-14, 16, 26, 10, '#B08A4A'); B(12, 18, 3, 6, '#B08A4A'); B(15, 20, 2, 2, '#B08A4A');
      B(-14, 16, 26, 1, '#8A6A3A'); B(-14, 25, 26, 1, '#C9A565');
      B(-11, 22, 14, 1, '#5A3C24'); B(-11, 19, 9, 1, '#5A3C24'); B(5, 19, 4, 1, '#5A3C24');
      B(-13, 24, 1, 1, '#3A2A12'); B(10, 17, 1, 1, '#3A2A12');
      break;
    }
    case 'anthill': {
      B(-9, 0, 18, 3, '#8A6A44'); B(-6, 3, 12, 3, '#8A6A44'); B(-3, 6, 6, 2, '#96744C');
      B(-9, 0, 18, 1, '#6B5234'); B(2, 1, 5, 3, '#775A3A');
      B(-6, 1, 2, 1, '#A8845A'); B(-3, 4, 2, 1, '#A8845A'); B(4, 4, 1, 1, '#A8845A');
      B(-1, 7, 2, 2, '#2B2118');
      // ants: two loiter by the door, a column files out when it is poked
      const ant = (dx: number): void => {
        const dy = (env.groundAt(dx) - gy) / s;
        B(dx, -dy, 2, 1, '#1A1410');
      };
      if (!env.reduce) {
        ant(-12 + Math.sin(env.t * 0.9) * 2); ant(11 + Math.sin(env.t * 0.7 + 2) * 2);
        const at = (p.antT as number) || 0;
        if (at > 0) {
          const out = 5 - at;
          for (let i = 0; i < 7; i++) {
            const d = out * 13 - i * 6;
            if (d > 0 && d < 60) ant((i % 2 ? -1 : 1) * (9 + d));
          }
        }
      }
      break;
    }
    case 'snowpile': {
      const H = (p.h as number) || 12;
      B(-W / 2, 0, W, H * 0.5, '#F4F8FB'); B(-W / 3, H * 0.5, W * 0.66, H * 0.3, '#F4F8FB'); B(-W / 6, H * 0.8, W / 3, H * 0.2, '#FFFFFF');
      B(-W / 2, 0, W, 2, '#D5E2EC'); B(W * 0.1, 2, W * 0.3, 2, '#E3ECF3');
      break;
    }
    case 'snowman': {
      B(-9, 0, 18, 12, '#F4F8FB'); B(-7, 12, 14, 2, '#F4F8FB'); B(-9, 0, 18, 2, '#D5E2EC');
      B(-6, 13, 12, 8, '#F4F8FB'); B(-4, 20, 9, 7, '#FFFFFF'); B(3, 3, 5, 6, '#E3ECF3');
      B(-2, 24, 1, 1, '#1A1410'); B(1, 24, 1, 1, '#1A1410'); B(0, 22, 4, 1, '#E2761B');
      B(0, 15, 1, 1, '#1A1410'); B(0, 18, 1, 1, '#1A1410');
      B(-11, 17, 5, 1, '#6B4A2E');
      const wave = p.waved && !env.reduce ? Math.sin(env.t * 8) * 2 : 0;
      if (p.waved) { B(6, 18, 3, 1, '#6B4A2E'); B(8, 19 + wave, 1, 4, '#6B4A2E'); }
      else B(6, 17, 5, 1, '#6B4A2E');
      B(-4, 27, 9, 1, '#26242A'); B(-2, 28, 5, 4, '#26242A'); B(-2, 28, 5, 1, '#B0503A');
      break;
    }
    default: break;
  }
}

/** The pond's lily pads (drawn over the water by the world). */
export function drawLilyPad(ctx: Ctx, x: number, lvl: number, i: number, sh: (c: string) => string): void {
  const l = Math.round(x), t = Math.round(lvl);
  ctx.fillStyle = sh('#4E8A3E'); ctx.fillRect(l, t - 2, 13, 3);
  ctx.fillStyle = sh('#6DBB58'); ctx.fillRect(l + 1, t - 2, 6, 1);
  ctx.fillStyle = sh('#2E5426'); ctx.fillRect(l + 1, t + 1, 11, 1);
  ctx.fillStyle = sh('#2E6EA5'); ctx.fillRect(l + 9, t - 2, 2, 1);
  if (i === 1) {
    ctx.fillStyle = sh('#F2A6C6'); ctx.fillRect(l + 3, t - 4, 4, 2);
    ctx.fillStyle = sh('#F4C542'); ctx.fillRect(l + 4, t - 5, 2, 1);
  }
}

/**
 * A carryable item, as a small pixel sprite. `x` is its centre and `y` what it
 * rests on; `k` is the size of one sprite pixel in world units. These used to
 * be emoji, which were the one thing in the scene not made of blocks.
 */
export function drawItemSprite(ctx: Ctx, id: string, x: number, y: number, k: number, sh: (c: string) => string): void {
  const B = (bx: number, by: number, w: number, h: number, c: string, raw?: boolean): void => {
    const l = Math.round(x + bx * k), r = Math.round(x + (bx + w) * k);
    const b = Math.round(y - by * k), t = Math.round(y - (by + h) * k);
    ctx.fillStyle = raw ? c : sh(c);
    ctx.fillRect(l, t, Math.max(1, r - l), Math.max(1, b - t));
  };
  switch (id) {
    case 'acorn':
      B(-2, 0, 4, 2, '#C98A3C'); B(-3, 2, 6, 4, '#C98A3C'); B(-2, 3, 1, 2, '#E3B36A'); B(1, 1, 2, 4, '#A66E2A');
      B(-3, 6, 6, 2, '#6B4A2E'); B(-2, 8, 4, 1, '#6B4A2E'); B(0, 9, 1, 1, '#55381F'); B(-3, 6, 6, 1, '#55381F');
      break;
    case 'pinecone':
      B(-2, 0, 4, 2, '#6B4A2E'); B(-3, 2, 6, 4, '#7A5433'); B(-2, 6, 4, 2, '#7A5433'); B(-1, 8, 2, 1, '#6B4A2E');
      B(-2, 2, 1, 1, '#55381F'); B(0, 3, 1, 1, '#55381F'); B(2, 2, 1, 1, '#55381F'); B(-1, 5, 1, 1, '#55381F'); B(1, 6, 1, 1, '#55381F');
      B(-3, 4, 1, 1, '#94683F'); B(-1, 7, 1, 1, '#94683F');
      break;
    case 'fish':
      B(-6, 1, 2, 6, '#3D6FA8'); B(-5, 3, 1, 2, '#6FA8DC');
      B(-4, 2, 8, 4, '#6FA8DC'); B(-3, 1, 6, 1, '#6FA8DC'); B(-3, 6, 5, 1, '#3D6FA8');
      B(-4, 2, 8, 1, '#DCE9F5'); B(2, 4, 1, 1, '#161616'); B(-1, 3, 2, 1, '#3D6FA8');
      break;
    case 'berry':
      B(-4, 0, 4, 4, '#5B3FE0'); B(0, 0, 4, 4, '#4A2ED8'); B(-2, 3, 4, 4, '#6A50F0');
      B(-3, 2, 1, 1, '#9C8BFF'); B(-1, 5, 1, 1, '#B5A8FF'); B(1, 2, 1, 1, '#9C8BFF');
      B(0, 7, 1, 2, '#3B7A34'); B(1, 8, 2, 1, '#4E9A44');
      break;
    case 'honey':
      B(-3, 0, 6, 6, '#E8A317'); B(-3, 0, 6, 1, '#B8860B'); B(-2, 2, 1, 3, '#F4D58A'); B(1, 1, 2, 3, '#C98A14');
      B(-3, 6, 6, 1, '#F2F4F6'); B(-4, 7, 8, 2, '#C9B27A'); B(-4, 7, 8, 1, '#A8905A');
      break;
    case 'feather':
      B(0, 0, 1, 3, '#B9C4CC'); B(-1, 3, 3, 5, '#F2F4F6'); B(-2, 4, 1, 3, '#F2F4F6'); B(0, 8, 2, 1, '#F2F4F6');
      B(0, 3, 1, 5, '#B9C4CC'); B(1, 5, 1, 2, '#D5E2EC');
      break;
    case 'truffle':
      B(-3, 0, 7, 4, '#4A3526'); B(-2, 4, 5, 2, '#4A3526'); B(-4, 1, 1, 2, '#4A3526');
      B(-2, 1, 1, 1, '#6B4F3A'); B(1, 3, 1, 1, '#6B4F3A'); B(2, 1, 1, 1, '#2E2018'); B(-1, 4, 1, 1, '#2E2018'); B(0, 2, 1, 1, '#6B4F3A');
      break;
    case 'flower':
      B(0, 0, 1, 5, '#3B7A34'); B(1, 2, 2, 1, '#4E9A44');
      B(-2, 6, 5, 2, '#F2A6C6'); B(-1, 5, 3, 4, '#F2A6C6'); B(-2, 6, 1, 2, '#E6659E'); B(2, 6, 1, 2, '#E6659E');
      B(0, 6, 1, 2, '#F4C542');
      break;
    case 'firefly':
      ctx.save(); ctx.shadowBlur = 8; ctx.shadowColor = '#CFFF7A';
      B(-1, 2, 3, 3, '#CFFF7A', true); ctx.restore();
      B(-1, 5, 3, 2, '#3A3A40'); B(-3, 5, 2, 1, '#DFFFFA', true); B(2, 5, 2, 1, '#DFFFFA', true);
      break;
    case 'snowball':
      B(-3, 1, 6, 5, '#F4F8FB'); B(-2, 0, 4, 7, '#F4F8FB'); B(-2, 4, 2, 2, '#FFFFFF'); B(0, 0, 3, 2, '#D5E2EC'); B(2, 2, 1, 2, '#D5E2EC');
      break;
    case 'coin':
      B(-3, 1, 6, 6, '#F4C542'); B(-2, 0, 4, 8, '#F4C542'); B(-2, 0, 4, 1, '#B8860B'); B(2, 1, 1, 5, '#D9A441');
      B(-1, 2, 2, 4, '#B8860B'); B(-2, 5, 1, 2, '#FFF1A8');
      break;
    case 'key':
      B(-5, 2, 4, 5, '#F4C542'); B(-4, 3, 2, 3, '#8A6A1E'); B(-5, 6, 4, 1, '#FFF1A8');
      B(-1, 4, 6, 2, '#F4C542'); B(2, 2, 1, 2, '#D9A441'); B(4, 2, 1, 2, '#D9A441'); B(-1, 4, 6, 1, '#D9A441');
      break;
    case 'shell':
      B(-3, 0, 6, 2, '#E9C8B0'); B(-4, 2, 8, 3, '#F2DCCB'); B(-3, 5, 6, 2, '#F2DCCB'); B(-1, 7, 2, 1, '#E9C8B0');
      B(-2, 1, 1, 5, '#C9A088'); B(0, 1, 1, 6, '#C9A088'); B(2, 1, 1, 5, '#C9A088');
      break;
    case 'water':
      B(-2, 0, 4, 1, '#5FA8E0'); B(-3, 1, 6, 3, '#8FD3FF'); B(-2, 4, 4, 2, '#8FD3FF'); B(-1, 6, 2, 2, '#8FD3FF'); B(0, 8, 1, 1, '#BFE6FF');
      B(-2, 2, 1, 2, '#E6F6FF'); B(1, 1, 2, 2, '#5FA8E0');
      break;
    default:
      B(-3, 0, 6, 6, '#C9B27A'); B(-1, 2, 2, 2, '#6B4A2E');
      break;
  }
}
