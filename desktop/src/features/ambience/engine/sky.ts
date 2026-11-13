/**
 * The living background frame — sky gradient, day/night, stars, sun/moon,
 * clouds, parallax ridges, grass strip, weather, and the world's own update
 * and draw.
 *
 * The engine owns the requestAnimationFrame loop; `skyFrame` renders exactly
 * one frame. `document.hidden` pauses everything: the update loop, the music
 * context, and rAF all stop.
 */

import type { EngineState } from './state';
import { mixc, rgba } from './state';
import { basinColumn, carveBasin, groundYWorld, paraX, ridge, ridgeBase, waterSpan } from './terrain';
import type { WorldState } from './types';

/**
 * F9: water sits in a carved valley. The level and span derive from the basin
 * once the terrain is generated. The pond and lily pads are anchored to the
 * ACTUAL water after the terrain exists (the ridge is wavy, so the submerged
 * span isn't a fixed screen x); on resize the terrain regenerates, so this
 * re-anchoring keeps the hitbox, nearPond and the fish spawn aligned.
 */
function reanchorPond(state: EngineState): void {
  const world = state.world as WorldState | null;
  if (!world || !world.props) return;
  const ws = waterSpan(state);
  if (ws && ws.x1 > ws.x0) {
    const pondP = world.pondProp();
    if (pondP) pondP.x = (ws.x0 + ws.x1) / 2;
    world.props.filter(function (p) { return p.key === 'lily'; }).forEach(function (lp) { lp.x = (ws.x0 + ws.x1) / 2; });
  }
}

/**
 * F-rainbow-layer: the rainbow is drawn in the SKY pass (after the clouds,
 * before the parallax ridges) so it sits just in front of the sky and BEHIND
 * the terrain. The rainbow-end props are invisible hit targets (their `y` is
 * anchored to the terrain so the arc's flat ends read as rising from behind
 * the hills).
 */
function drawRainbow(state: EngineState, ctx: CanvasRenderingContext2D): void {
  const world = state.world as WorldState | null;
  if (!world || !world.props) return;
  const left = world.props.find(function (p) { return p.key === 'rainbow-end' && p.side === 'l'; });
  if (!left) return;
  const cols = ['#E6533E', '#F4C542', '#5E9E4A', '#3EC7C0', '#63B4FF', '#C58BFF'];
  const cxx = ((left.x as number) + (left.pairX as number)) / 2 + paraX(state, 'sky');
  const rad = ((left.pairX as number) - (left.x as number)) / 2;
  const gy = left.y !== undefined ? left.y : state.H * 0.55;
  for (let ri = 0; ri < cols.length; ri++) {
    ctx.strokeStyle = cols[ri];
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.arc(cxx, gy, rad - ri * 4 + 24, Math.PI, 0);
    ctx.stroke();
  }
}

/**
 * Rebuild the ridges, the basin and the water for the current canvas size.
 *
 * Resizing must move the world as little as possible. `ridge` is deterministic
 * per column, so a wider canvas only APPENDS hills — the existing ones keep
 * their shape as long as their baseline does, which is what `ridgeBase` (fixed
 * distance above the bottom edge) buys. And the basin is carved around a world
 * x pinned on the first pass, not around a fraction of the ridge array, whose
 * length tracks the width: that fraction is why the pond used to slide sideways
 * every time the window changed shape.
 */
export function regenerateTerrain(state: EngineState): void {
  if (state.W === state.lastW && state.H === state.lastH) return;
  state.lastW = state.W;
  state.lastH = state.H;
  if (state.refH === 0) state.refH = state.H;
  const [s1, s2, s3] = state.ridgeSeeds;
  state.R1 = ridge(s1, 90, ridgeBase(state.H, state.refH, 0.62), 16, state.W, state.OVER);
  state.R2 = ridge(s2, 70, ridgeBase(state.H, state.refH, 0.74), 14, state.W, state.OVER);
  state.R3 = ridge(s3, 44, ridgeBase(state.H, state.refH, 0.86), 12, state.W, state.OVER);
  const col = basinColumn(state.R3, state.basinAnchorX, 0.30, 12, state.OVER);
  if (state.basinAnchorX === null) state.basinAnchorX = col * 12 - state.OVER;
  state.BASIN = carveBasin(state.R3, 12, col, 11, 46);
  // water fills the bowl up to its lowest rim — stays in the dip, never floods
  const b = state.BASIN;
  state.WATER_LEVEL = Math.max(state.R3[b.c - b.half], state.R3[b.c + b.half]);
  reanchorPond(state);
  reanchorRainbow(state);
}

/**
 * Re-span the rainbow across the new viewport.
 *
 * Its ends were positioned once, in absolute pixels, at spawn time — so after a
 * resize the arc kept the old window's width and drifted away from the hills it
 * is supposed to rise from.
 */
export function reanchorRainbow(state: EngineState): void {
  const world = state.world as WorldState | null;
  if (!world || !world.props) return;
  for (const p of world.props) {
    if (p.key !== 'rainbow-end') continue;
    const fx = p.fx as number | undefined;
    const pairFx = p.pairFx as number | undefined;
    if (fx === undefined || pairFx === undefined) continue;
    p.x = state.W * fx;
    p.pairX = state.W * pairFx;
  }
  // Both ends sit on the lower of the two ground heights, as at spawn.
  const l = world.props.find((p) => p.key === 'rainbow-end' && p.side === 'l');
  const r = world.props.find((p) => p.key === 'rainbow-end' && p.side === 'r');
  if (l && r) {
    const gy = Math.max(
      groundYWorld(state, l.x as number, 2),
      groundYWorld(state, r.x as number, 2),
    );
    l.y = gy;
    r.y = gy;
  }
}

/** Render exactly one background frame at time `ts`. */
export function skyFrame(state: EngineState, ctx: CanvasRenderingContext2D, ts: number): void {
  // regenerateTerrain guards on the size itself (and records it after
  // generating); do NOT pre-set lastW/lastH here or the guard always
  // early-returns and the terrain (ridges, basin, water) never renders
  // (F-terrain).
  if (state.W !== state.lastW || state.H !== state.lastH) {
    regenerateTerrain(state);
  }
  const day = state.tod; // 0..1  (0 = deep night, .5 = noon)
  const dl = Math.max(0, Math.sin(day * Math.PI));
  const top = mixc([8, 14, 34], [74, 150, 220], dl), bot = mixc([26, 34, 60], [176, 218, 238], dl);
  const g = ctx.createLinearGradient(0, 0, 0, state.H);
  g.addColorStop(0, rgba(top, 1));
  g.addColorStop(1, rgba(bot, 1));
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, state.W, state.H);
  if (state.flash > 0) {
    ctx.fillStyle = 'rgba(210,230,255,' + (state.flash * 0.5) + ')';
    ctx.fillRect(0, 0, state.W, state.H);
    state.flash -= 0.06;
  }

  if (dl < 0.42) {
    const sa = (1 - dl / 0.42);
    state.stars.forEach(function (s) {
      ctx.globalAlpha = sa * (0.35 + 0.65 * Math.abs(Math.sin(ts / 700 + s.p)));
      ctx.fillStyle = '#fff';
      ctx.fillRect(s.x * state.W + (state.mx - 0.5) * -8, s.y * state.H + (state.my - 0.5) * -6, s.r, s.r);
    });
    ctx.globalAlpha = 1;
  }

  // sun / moon: a blocky disc with a soft glow. The sun has four stubby rays;
  // the moon has craters and whatever phase the player has clicked it to.
  const ang = day * Math.PI, sx = Math.round(state.W * (1 - day)), sy = Math.round(state.H * 0.72 - Math.sin(ang) * state.H * 0.6);
  const isSun = dl > 0.4;
  ctx.save();
  ctx.shadowBlur = 44;
  ctx.shadowColor = isSun ? '#FFD84D' : '#DDE7FF';
  ctx.fillStyle = isSun ? '#FFE47A' : '#E8EEFF';
  ctx.fillRect(sx - 14, sy - 16, 28, 32);
  ctx.fillRect(sx - 16, sy - 14, 32, 28);
  ctx.restore();
  if (isSun) {
    ctx.fillStyle = '#FFF3B0';
    ctx.fillRect(sx - 10, sy - 12, 14, 6); ctx.fillRect(sx - 12, sy - 10, 6, 10);
    ctx.fillStyle = '#F7C63A';
    ctx.fillRect(sx - 14, sy + 12, 28, 4); ctx.fillRect(sx + 12, sy - 6, 4, 20);
    const pulse = state.reduce ? 0 : Math.round(Math.sin(ts / 600) * 2);
    ctx.fillStyle = '#FFE47A';
    ctx.fillRect(sx - 3, sy - 26 - pulse, 6, 6); ctx.fillRect(sx - 3, sy + 20 + pulse, 6, 6);
    ctx.fillRect(sx - 26 - pulse, sy - 3, 6, 6); ctx.fillRect(sx + 20 + pulse, sy - 3, 6, 6);
  } else {
    ctx.fillStyle = '#C9D3EE';
    ctx.fillRect(sx - 9, sy - 8, 6, 6); ctx.fillRect(sx + 3, sy + 2, 8, 6); ctx.fillRect(sx + 4, sy - 11, 4, 4); ctx.fillRect(sx - 6, sy + 6, 4, 4);
    ctx.fillStyle = '#FFFFFF';
    ctx.fillRect(sx - 12, sy - 12, 8, 3);
    const liveWorld = state.world as WorldState | null;
    const moon = liveWorld && liveWorld.props ? liveWorld.props.find(function (p) { return p.key === 'sunmoon'; }) : undefined;
    const phase = moon ? ((moon.phase as number) || 0) : 0;
    if (phase > 0) {
      // the unlit part of the disc takes the colour of the sky behind it
      ctx.fillStyle = rgba(mixc(top, bot, sy / state.H), 0.92);
      ctx.fillRect(sx + 16 - phase * 8, sy - 16, phase * 8, 32);
    }
  }

  // clouds (parallax): three stacked slabs with a shaded underside
  for (let c = 0; c < 9; c++) {
    const cw = 110 + ((c * 57) % 150);
    const cx = Math.round(((ts / 38 + c * 260) % (state.W + 420)) - 210 + (state.mx - 0.5) * -30);
    const cy = Math.round(60 + ((c * 97) % 180) + (state.my - 0.5) * -18);
    ctx.globalAlpha = 0.16 + 0.3 * dl;
    ctx.fillStyle = '#fff';
    ctx.fillRect(cx, cy, cw, 17);
    ctx.fillRect(cx + 26, cy - 13, Math.round(cw * 0.62), 15);
    ctx.fillRect(cx + Math.round(cw * 0.34), cy - 22, Math.round(cw * 0.3), 10);
    ctx.globalAlpha = (0.16 + 0.3 * dl) * 0.5;
    ctx.fillStyle = '#9FB4CC';
    ctx.fillRect(cx + 6, cy + 13, cw - 12, 4);
  }
  ctx.globalAlpha = 1;

  // rainbow — drawn here, in the SKY pass, so it sits just in front of the
  // sky and BEHIND the terrain ridges (F-rainbow-layer). The rainbow-end
  // props remain invisible hit targets for the egg.
  drawRainbow(state, ctx);

  // The living world advances once, then is drawn a layer at a time: each
  // ridge is painted and whatever lives on it goes on top, so a nearer hill
  // hides the trees and animals standing behind it. Everything used to be
  // drawn after all three ridges, which put a pine on the middle hills in
  // front of the foreground it was supposed to be behind.
  const dtw = Math.min((ts - state.lastTs) / 1000, 0.05) || 0;
  state.lastTs = ts;
  const world = state.world as WorldState | null;
  const live = !document.hidden && world ? world : null;
  if (live) {
    live.dl = dl;
    live.update(dtw);
    live.beginDraw();
  }

  // deterministic per-column noise, so the texture is stable from frame to frame
  const noise = function (i: number, salt: number): number {
    const v = Math.sin(i * 127.1 + salt * 311.7 + state.ridgeSeeds[0]) * 43758.5453;
    return v - Math.floor(v);
  };
  const H = state.H, OVER = state.OVER;

  // far ridge: bare rock with snow on the highest peaks
  if (state.R1) {
    const pts = state.R1, off = (state.mx - 0.5) * -26 - OVER, yo = (state.my - 0.5) * -26 * 0.4;
    let peak = Infinity;
    for (let i = 0; i < pts.length; i++) peak = Math.min(peak, pts[i]);
    const body = rgba(mixc([16, 26, 46], [86, 124, 150], dl), 1);
    const lit = rgba(mixc([22, 34, 58], [108, 146, 170], dl), 1);
    const snow = rgba(mixc([96, 110, 140], [232, 240, 248], dl), 1);
    for (let i = 0; i < pts.length; i++) {
      const x = Math.round(i * 16 + off), y = Math.round(pts[i] + yo);
      ctx.fillStyle = body; ctx.fillRect(x, y, 17, H - y + OVER);
      ctx.fillStyle = pts[i] <= peak + 16 ? snow : lit;
      ctx.fillRect(x, y, 17, pts[i] <= peak + 16 ? 6 : 3);
      if (i > 0 && pts[i - 1] > pts[i]) { ctx.fillStyle = lit; ctx.fillRect(x, y, 3, Math.min(40, pts[i - 1] - pts[i])); }
    }
  }
  if (live) live.drawLayer(ctx, 0);

  // middle ridge: wooded hills, with a ragged line of small conifers along the top
  if (state.R2) {
    const pts = state.R2, off = (state.mx - 0.5) * -16 - OVER, yo = (state.my - 0.5) * -16 * 0.4;
    const body = rgba(mixc([14, 30, 38], [62, 110, 96], dl), 1);
    const lit = rgba(mixc([18, 38, 44], [78, 130, 108], dl), 1);
    const wood = rgba(mixc([10, 24, 32], [46, 90, 80], dl), 1);
    for (let i = 0; i < pts.length; i++) {
      const x = Math.round(i * 14 + off), y = Math.round(pts[i] + yo);
      if (noise(i, 2) > 0.62) {
        ctx.fillStyle = wood;
        const tx = x + 2 + Math.round(noise(i, 3) * 5);
        ctx.fillRect(tx, y - 4, 7, 5); ctx.fillRect(tx + 1, y - 8, 5, 4); ctx.fillRect(tx + 2, y - 11, 3, 3);
      }
      ctx.fillStyle = body; ctx.fillRect(x, y, 15, H - y + OVER);
      ctx.fillStyle = lit; ctx.fillRect(x, y, 15, 5);
      if (noise(i, 4) > 0.5) { ctx.fillStyle = wood; ctx.fillRect(x + 3 + Math.round(noise(i, 5) * 6), y + 12 + Math.round(noise(i, 6) * 40), 4, 4); }
    }
  }
  if (live) live.drawLayer(ctx, 1);

  // near ridge: soil with a turf cap, lit from the left, with tufts and stones
  if (state.R3) {
    const pts = state.R3, off = (state.mx - 0.5) * -8 - OVER, yo = (state.my - 0.5) * -3.2;
    const soil = rgba(mixc([12, 34, 30], [52, 120, 64], dl), 1);
    const soilDark = rgba(mixc([9, 27, 25], [43, 103, 57], dl), 1);
    const soilLight = rgba(mixc([15, 41, 34], [62, 134, 72], dl), 1);
    const turf = rgba(mixc([22, 58, 34], [110, 190, 74], dl), 1);
    const turfLight = rgba(mixc([30, 74, 42], [142, 214, 98], dl), 1);
    const turfDark = rgba(mixc([17, 46, 30], [84, 158, 62], dl), 1);
    for (let i = 0; i < pts.length; i++) {
      const x = Math.round(i * 12 + off), y = Math.round(pts[i] + yo);
      ctx.fillStyle = soil; ctx.fillRect(x, y, 13, H - y + OVER);
      // a few clods and pale patches in the soil
      const n1 = noise(i, 7), n2 = noise(i, 8), n3 = noise(i, 9);
      ctx.fillStyle = soilDark; ctx.fillRect(x + Math.round(n1 * 8), y + 18 + Math.round(n2 * 50), 4, 3);
      if (n3 > 0.4) ctx.fillRect(x + Math.round(n2 * 8), y + 74 + Math.round(n1 * 60), 3, 3);
      if (n2 > 0.55) { ctx.fillStyle = soilLight; ctx.fillRect(x + Math.round(n3 * 8), y + 30 + Math.round(n1 * 70), 3, 2); }
      // turf: a cap with a ragged lower edge
      ctx.fillStyle = turf; ctx.fillRect(x, y, 13, 12);
      ctx.fillRect(x + 1 + Math.round(n1 * 3), y + 12, 3, 2 + Math.round(n2 * 2));
      ctx.fillRect(x + 7 + Math.round(n3 * 3), y + 12, 2, 1 + Math.round(n1 * 3));
      ctx.fillStyle = turfLight; ctx.fillRect(x, y, 13, 2);
      ctx.fillStyle = turfDark; ctx.fillRect(x, y + 10, 13, 2);
      // the exposed faces of a step: light on the left, shade on the right
      if (i > 0 && pts[i - 1] > pts[i]) { ctx.fillStyle = turfLight; ctx.fillRect(x, y, 2, 12); }
      if (i < pts.length - 1 && pts[i + 1] > pts[i]) {
        ctx.fillStyle = turfDark; ctx.fillRect(x + 10, y + 2, 3, 10);
        ctx.fillStyle = soilDark; ctx.fillRect(x + 10, y + 12, 3, Math.max(0, pts[i + 1] - pts[i] - 12));
      }
      // tufts of grass on the open flats (never under the pond)
      if (n1 > 0.58 && !(state.BASIN && Math.abs(i - state.BASIN.c) <= state.BASIN.half && pts[i] > state.WATER_LEVEL)) {
        ctx.fillStyle = turfLight;
        const tx = x + 2 + Math.round(n2 * 6);
        ctx.fillRect(tx, y - 3, 1, 3); ctx.fillRect(tx + 2, y - 2, 1, 2);
        if (n3 > 0.7) ctx.fillRect(tx - 2, y - 2, 1, 2);
      }
    }
  }
  if (live) {
    live.drawLayer(ctx, 2);
    live.drawLayer(ctx, 'pond');
    live.drawLayer(ctx, 'sky');
    live.drawOverlay(ctx);
  }

  // weather
  if (state.WEATHER[state.weather] === 'rain') {
    ctx.strokeStyle = 'rgba(190,220,255,.42)';
    ctx.lineWidth = 1.6;
    ctx.beginPath();
    state.drops.forEach(function (d) {
      d.y += 0.016 * d.s;
      if (d.y > 1) d.y -= 1;
      const X = d.x * state.W + (state.mx - 0.5) * -14, Y = d.y * state.H;
      ctx.moveTo(X, Y);
      ctx.lineTo(X - 3, Y + 15 * d.s);
    });
    ctx.stroke();
    if (Math.random() < 0.004) state.flash = 1;
  } else if (state.WEATHER[state.weather] === 'snow') {
    state.flakes.forEach(function (f) {
      f.y += 0.0016 * f.s;
      f.x += Math.sin(ts / 1400 + f.p) * 0.0006;
      if (f.y > 1) f.y -= 1;
      ctx.globalAlpha = 0.55 + 0.4 * f.s;
      ctx.fillStyle = '#fff';
      ctx.fillRect(f.x * state.W + (state.mx - 0.5) * -10, f.y * state.H, 3 * f.s + 1, 3 * f.s + 1);
    });
    ctx.globalAlpha = 1;
  }
  if (dl < 0.35) {
    state.fireflies.forEach(function (f) {
      const a = (1 - dl / 0.35) * (0.35 + 0.65 * Math.abs(Math.sin(ts / 560 + f.p)));
      ctx.globalAlpha = a;
      ctx.fillStyle = '#CFFF7A';
      ctx.shadowBlur = 10;
      ctx.shadowColor = '#CFFF7A';
      ctx.fillRect((f.x + Math.sin(ts / 2600 + f.p) * 0.02) * state.W, (f.y + Math.cos(ts / 3100 + f.p) * 0.012) * state.H, 3, 3);
    });
    ctx.globalAlpha = 1;
    ctx.shadowBlur = 0;
  }
}
