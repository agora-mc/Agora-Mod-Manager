/**
 * The fixed props: where they stand and what they do when poked.
 *
 * The layout is seeded, so a session's world is stable, but it is no longer a
 * list of fixed fractions. Each prop asks for a stretch of the map and the
 * placer finds it a spot there that is dry, reasonably level and not already
 * taken. That is what keeps a signpost out of a rock and a snow pile out of
 * the pond. The terrain must exist before this runs (see `createWorld`).
 *
 * Reactions: every prop does something visible on every click. The ones that
 * hide a discovery still do; the rest rustle, puff, or send something
 * scurrying.
 */

import type { GameApi } from './api';
import type { EngineState } from './state';
import { SPECIES_BY_KEY } from './species';
import { BOULDER_ROLL, HIVE_DX, HIVE_LIFT } from './sprites';
import { stepFor, waterSpan } from './terrain';
import type { Prop } from './types';

export function buildProps(state: EngineState, api: GameApi): Prop[] {
  const { world: W, dropGroundItem, findEgg, shakeProp, blip } = api;
  const wrand = state.wrand;
  const hasTerrain = !!state.R3;
  const span = hasTerrain ? waterSpan(state) : { x0: state.W * 0.22, x1: state.W * 0.38 };

  /* ── placement ───────────────────────────────────────────────────── */

  interface Taken { x: number; r: number; layer: number; }
  const taken: Taken[] = [];

  const ridgeOf = (layer: number): number[] | null => (layer === 0 ? state.R1 : layer === 1 ? state.R2 : state.R3);

  /** How many height changes lie under a footprint (0 = level ground). */
  function steps(x: number, r: number, layer: number): number {
    const arr = ridgeOf(layer);
    if (!arr) return 0;
    const st = stepFor(layer);
    const a = Math.max(0, Math.floor((x - r + state.OVER) / st));
    const b = Math.min(arr.length - 1, Math.floor((x + r - 0.01 + state.OVER) / st));
    let n = 0;
    for (let i = a + 1; i <= b; i++) if (arr[i] !== arr[i - 1]) n++;
    return n;
  }

  function wet(x: number, r: number): boolean {
    return x + r > span.x0 - 8 && x - r < span.x1 + 20;
  }

  /**
   * Find a spot for something `r` wide (half-width) between two fractions of
   * the map. Tries seeded candidates, scores each for water, neighbours and
   * slope, and takes the first clean one — or the least bad if the stretch is
   * crowded, looking a little further afield on the later tries.
   */
  function place(lo: number, hi: number, r: number, layer = 2): number {
    const st = stepFor(layer) / 2;
    let best = (lo + hi) / 2 * state.W, bestScore = -Infinity;
    for (let i = 0; i < 70; i++) {
      const widen = i < 14 ? 0 : 0.04 + (i - 14) * 0.007;
      const f = (lo - widen) + wrand() * (hi - lo + widen * 2);
      let x = Math.max(0.03, Math.min(0.97, f)) * state.W;
      // line the footprint up with the terrain's columns
      x = Math.round((x + state.OVER) / st) * st - state.OVER;
      let score = -i * 0.4;
      if (layer === 2 && wet(x, r)) score -= 10000;
      for (const q of taken) {
        const gap0 = Math.abs(q.x - x) - (q.r + r);
        // things on the hills behind may stand behind something in front, but
        // would rather not: a pine growing out of a signpost reads as one object
        if (q.layer !== layer) { if (gap0 < 0) score -= 30; continue; }
        const gap = gap0;
        if (gap < 4) score -= 2000 + (4 - gap) * 20;
      }
      score -= steps(x, r, layer) * 45;
      if (score > bestScore) { bestScore = score; best = x; }
      if (score > -6) break;
    }
    if (bestScore < -1000) {
      // The stretch it asked for is full. Rather than overlap a neighbour,
      // sweep the whole map for the free spot nearest to where it wanted to be.
      const want = (lo + hi) / 2 * state.W;
      let near = Infinity;
      for (let x = Math.round((0.03 * state.W + state.OVER) / st) * st - state.OVER; x < 0.97 * state.W; x += st) {
        if (layer === 2 && wet(x, r)) continue;
        if (taken.some((q) => q.layer === layer && Math.abs(q.x - x) - (q.r + r) < 2)) continue;
        const d = Math.abs(x - want) + steps(x, r, layer) * 30;
        if (d < near) { near = d; best = x; }
      }
    }
    taken.push({ x: best, r, layer });
    return best;
  }

  const arr: Prop[] = [];
  function add(o: Prop): Prop {
    o.hb = o.hb || { w: (o.w as number || 30), h: (o.h as number || 36) };
    arr.push(o);
    return o;
  }

  /** Middle of a prop as drawn, and its top and foot, for particles and text. */
  function at(p: Prop): { x: number; top: number; foot: number } {
    const b = W().boxOf({ kind: 'prop', obj: p });
    return { x: b.x + b.w / 2, top: b.y + 6, foot: b.y + b.h - 6 };
  }

  /* Positions first, biggest and most particular things first, so the props
     that are hardest to seat get the pick of the ground. */
  const pondX = (span.x0 + span.x1) / 2;
  const cattailX = span.x0 - 6;
  taken.push({ x: cattailX, r: 8, layer: 2 });
  const logW = 54;
  const logX = place(0.66, 0.78, logW / 2);
  const ringX = place(0.50, 0.60, 27);
  const boulderX = place(0.82, 0.90, 15);
  taken.push({ x: boulderX + BOULDER_ROLL, r: 15, layer: 2 });   // room to roll into
  const campfireX = place(0.58, 0.66, 12);
  const oakXs = [place(0.05, 0.17, 9), place(0.42, 0.50, 9), place(0.80, 0.94, 9)];
  taken.push({ x: oakXs[1] + HIVE_DX, r: 6, layer: 2 });
  const scarecrowX = place(0.72, 0.80, 14);
  const signX = place(0.44, 0.54, 17);
  const stumpX = place(0.40, 0.48, 11);
  const snowX = place(0.08, 0.90, 30);
  const bushXs = [place(0.12, 0.22, 13), place(0.66, 0.78, 13)];
  const rockXs = [place(0.04, 0.14, 11), place(0.48, 0.58, 11), place(0.60, 0.70, 11), place(0.90, 0.97, 11)];
  const flowerXs = [place(0.10, 0.20, 15), place(0.44, 0.58, 15), place(0.84, 0.96, 15)];
  const anthillX = place(0.54, 0.64, 10);
  const pineXs = [place(0.08, 0.18, 12, 1), place(0.55, 0.62, 12, 1), place(0.68, 0.76, 12, 1)];
  const caveX = place(0.90, 0.95, 22, 1);

  /* ── the props, in drawing order (trees behind, small things in front) ── */

  oakXs.forEach((x) => {
    add({
      key: 'oak', name: 'Oak tree', layer: 2, x: x, w: 34, h: 64, hole: false, shakeT: 0,
      reaction: function (p: Prop) {
        shakeProp(p); if (Math.random() < 0.1) { W().spawn(SPECIES_BY_KEY.squirrel, { x: p.x, fromLeft: Math.random() < 0.5 }); }
        dropGroundItem('acorn', p.x, p);
        const a = at(p);
        W().puff(a.x, a.top + 24, '#5BA84A', 7, { spread: 50, up: 10, g: 60, life: 1.3 });
      },
    });
  });
  pineXs.forEach((x) => {
    add({
      key: 'pine', name: 'Pine tree', layer: 1, x: x, w: 26, h: 70, shakeT: 0,
      reaction: function (p: Prop) {
        const snow = state.WEATHER[state.weather] === 'snow';
        shakeProp(p); dropGroundItem(snow ? 'snowball' : 'pinecone', p.x, p);
        const a = at(p);
        W().puff(a.x, a.top + 16, snow ? '#F4F8FB' : '#3F7A52', 6, { spread: 36, up: 8, g: 70, life: 1.1 });
      },
    });
  });
  add({
    key: 'pond', name: 'Pond', layer: 2, x: pondX, rw: 130, h: 26, ripple: 0,
    reaction: function (p: Prop) {
      p.ripple = 1; blip(300, 0.3, 'sine', 0.06);
      // F10: an empty-handed click lets you scoop up water
      if (!W().carry) W().pickUp('water', p.x, 0);
      // F3: the fish is caught mid-air (see fish.onClick) — discovery rewards catching
      W().spawnFish(p);
      const n = ((W().flags.pondClicks as number) || 0) + 1;
      W().flags.pondClicks = n;
    },
    hb: { w: 260, h: 40 },
  });
  flowerXs.forEach((fx) => {
    add({
      key: 'flowers', name: 'Flower patch', layer: 2, x: fx, w: 30, h: 15, bloom: false, picked: false,
      reaction: function (p: Prop) {
        const opening = !p.bloom;
        p.bloom = true; blip(700, 0.12); findEgg('flower-bloom');
        const a = at(p);
        W().puff(a.x, a.top + 2, opening ? '#F2A6C6' : '#F4C542', opening ? 8 : 3, { spread: 34, up: 30, g: 50, life: 1.1 });
        // F10: once bloomed, a flower can be picked and given to the deer
        if (p.bloom && !p.picked) { p.picked = true; dropGroundItem('flower', (p.x as number) + 14, p); }
        for (let i = 0; i < 2; i++) W().spawn(SPECIES_BY_KEY.bee, { x: (p.x as number) + (i - 1) * 30 });
        const wf = (W().flags.wateredFlowers = (W().flags.wateredFlowers as Record<string, boolean>) || {});
        if (W().carry === 'water') {
          wf[p.x as unknown as string] = true; W().dropCarry();
          if (Object.keys(wf).length >= 3) findEgg('water-flowers');
        }
      },
    });
  });
  rockXs.forEach((rx) => {
    add({
      key: 'rock', name: 'Rock', layer: 2, x: rx, w: 20, h: 14, tipped: false, beetleT: 0,
      reaction: function (p: Prop) {
        // the first push turns it over; after that a beetle bolts each time
        p.tipped = true; p.beetleT = 2.4; blip(240, 0.15, 'square'); findEgg('rock-beetle');
        // the box follows the rock to where it now lies
        p.hb = { w: 34, h: 14 }; p.boxDx = 6;
        const a = at(p);
        W().puff(a.x, a.foot, '#8A6A44', 5, { spread: 30, up: 22, g: 160, life: 0.6 });
      },
    });
  });
  add({
    key: 'log', name: 'Fallen log', layer: 2, x: logX, w: logW, h: 12, shrooms: false,
    reaction: function (p: Prop) {
      const first = !p.shrooms;
      p.shrooms = true; blip(500, 0.1); findEgg('log-shroom');
      const a = at(p);
      W().puff(a.x, a.top + 2, first ? '#E9E4D4' : '#C99B66', first ? 8 : 4, { spread: 50, up: 26, g: 80, life: 0.9 });
    },
  });
  bushXs.forEach((bx) => {
    add({
      key: 'bush', name: 'Berry bush', layer: 2, x: bx, w: 24, h: 16, berries: true,
      reaction: function (p: Prop) {
        const a = at(p);
        if (p.berries) {
          dropGroundItem('berry', p.x, p);
          p.berries = false;
          setTimeout(function () { p.berries = true; }, 15000);
          blip(560, 0.1);
          W().puff(a.x, a.top + 6, '#5B3FE0', 4, { spread: 30, up: 20, g: 200, life: 0.6 });
        } else {
          // picked clean for now: it just rustles
          blip(420, 0.06);
          W().puff(a.x, a.top + 6, '#44803C', 4, { spread: 30, up: 14, g: 80, life: 0.8 });
        }
      },
    });
  });
  for (let ri = 0; ri < 7; ri++) {
    add({
      key: 'mushroom', ringIdx: ri, name: 'Mushroom', layer: 2, x: ringX + (ri - 3) * 8, w: 8, h: 10,
      reaction: function (p: Prop) {
        blip(620 + ri * 20, 0.08); W().ringHit(ri);
        const a = at(p);
        W().puff(a.x, a.top, '#F2E9DA', 4, { spread: 16, up: 26, g: -10, life: 1 });
      },
    });
  }
  add({
    key: 'hive', name: 'Beehive', layer: 2, x: oakXs[1] + HIVE_DX, host: arr[1], lift: HIVE_LIFT, w: 12, h: 16, clicks: 0, honey: false,
    reaction: function (p: Prop) {
      p.clicks = (p.clicks as number) + 1; blip(340, 0.1, 'sawtooth');
      for (let i = 0; i < 2; i++) W().spawn(SPECIES_BY_KEY.bee, { x: p.x });
      if ((p.clicks as number) >= 3) { p.honey = true; dropGroundItem('honey', p.x, p); }
    },
  });
  add({
    key: 'campfire', name: 'Campfire', layer: 2, x: campfireX, w: 22, h: 16, clicks: 0, lit: false,
    reaction: function (p: Prop) {
      p.clicks = (p.clicks as number) + 1; blip(260, 0.14, 'square');
      const a = at(p);
      W().puff(a.x, a.foot - 6, '#FFB05A', p.lit ? 6 : 3, { spread: 26, up: 46, g: 30, life: 0.7 });
      if ((p.clicks as number) >= 3 && !p.lit) {
        p.lit = true; blip(180, 0.4, 'sawtooth', 0.1);
        if (W().isNight()) W().startCampfireTales(p);
      }
    },
  });
  add({
    key: 'stump', name: 'Tree stump', layer: 2, x: stumpX, w: 20, h: 12, sprout: 0,
    reaction: function (p: Prop) {
      // a seedling grows a little each time; in flower, it sends a butterfly up
      const stage = ((p.sprout as number) || 0) + 1;
      blip(300 + stage * 60, 0.12);
      const a = at(p);
      if (stage > 3) {
        p.sprout = 0;
        const b = W().spawn(SPECIES_BY_KEY.butterfly, { x: p.x, fromLeft: Math.random() < 0.5 });
        b.data.fadeIn = true;
        W().puff(a.x + 5, a.top - 4, '#F2A6C6', 6, { spread: 30, up: 30, g: 40, life: 1 });
      } else {
        p.sprout = stage;
        W().puff(a.x + 5, a.top, '#6DBB58', 3, { spread: 16, up: 20, g: 60, life: 0.7 });
      }
    },
  });
  add({
    key: 'cave', name: 'Cave mouth', layer: 1, x: caveX, w: 48, h: 42, blink: 0, glow: false,
    reaction: function (p: Prop) {
      p.blink = 1; blip(200, 0.3, 'sine');
      if (W().carry === 'firefly') { p.glow = true; W().dropCarry(); findEgg('firefly-cave'); }
      if (W().flags.haveKey) { p.opened = true; findEgg('the-long-con'); }
    },
  });
  add({
    key: 'scarecrow', name: 'Scarecrow', layer: 2, x: scarecrowX, w: 28, h: 39, spin: 0, feather: false,
    reaction: function (p: Prop) {
      p.spin = 1; blip(360, 0.15);
      const a = at(p);
      W().puff(a.x, a.top + 16, '#E9C88A', 5, { spread: 44, up: 16, g: 120, life: 0.8 });
      if (W().carry === 'feather') { p.feather = true; W().dropCarry(); findEgg('feather-scarecrow'); }
    },
  });
  add({
    key: 'lily', name: 'Lily pads', layer: 'pond', x: pondX, w: 20, h: 6, hb: { w: 24, h: 10 },
    reaction: function () {
      const pond = W().pondProp();
      if (pond) pond.ripple = 0.6;
      blip(520, 0.06, 'sine', 0.04);
      if (W().carry === 'snail') { W().dropCarry(); findEgg('snail-lily'); }
    },
  });
  add({
    key: 'cattails', name: 'Cattails', layer: 2, x: cattailX, w: 14, h: 26,
    reaction: function (p: Prop) {
      blip(900, 0.08);
      const a = at(p);
      W().puff(a.x, a.top + 4, '#F2E9DA', 6, { spread: 24, up: 20, g: -14, life: 1.4 });
    },
  });
  add({
    key: 'boulder', name: 'Boulder', layer: 2, x: boulderX, w: 30, h: 22, clicks: 0, rolled: false, rollT: 1,
    reaction: function (p: Prop) {
      p.clicks = (p.clicks as number) + 1; blip(220, 0.18, 'square');
      const a = at(p);
      W().puff(a.x, a.foot, '#9A9AA2', 4, { spread: 40, up: 26, g: 200, life: 0.5 });
      if ((p.clicks as number) >= 5 && !p.rolled) {
        // it finally gives, and rolls off the burrow it was plugging
        p.rolled = true; p.rollT = 0; p.name = 'Burrow';
        W().flags.boulderMoved = true; W().flags.boulderX = p.x; findEgg('boulder-hole');
        W().puff(a.x, a.foot, '#8A6A44', 10, { spread: 70, up: 40, g: 220, life: 0.8 });
      }
    },
  });
  const SIGNS = ['Nether: 3km', "Home: you're soaking in it", 'The End: keep walking', 'Village: ask a llama', "Ravine: don't"];
  add({
    key: 'signpost', name: 'Signpost', layer: 2, x: signX, w: 30, h: 28, idx: -1,
    reaction: function (p: Prop) {
      // the sign had five things to say and no way of saying them
      p.idx = ((p.idx as number) + 1) % SIGNS.length; blip(500, 0.08);
      const a = at(p);
      W().popFx(a.x, a.top - 12, SIGNS[p.idx as number], true);
    },
  });
  add({
    key: 'anthill', name: 'Anthill', layer: 2, x: anthillX, w: 20, h: 9, antT: 0,
    reaction: function (p: Prop) {
      blip(1200, 0.05); p.antT = 5;
      const a = at(p);
      W().puff(a.x, a.top + 2, '#A8845A', 4, { spread: 18, up: 24, g: 180, life: 0.5 });
    },
  });
  add({
    key: 'sunmoon', name: 'Sun / Moon', layer: 'sky', x: 0, y: 0, w: 34, h: 34, phase: 0,
    reaction: function (p: Prop) {
      if (Math.sin(state.tod * Math.PI) > 0.4) { state.flash = Math.max(state.flash, 0.6); blip(880, 0.2); }
      else { p.phase = ((p.phase as number) + 1) % 4; blip(500, 0.2); }
    },
  });
  add({
    key: 'clouds', name: 'Cloud', layer: 'sky', x: state.W * 0.62, y: state.H * 0.16, w: 224, h: 30, rainT: 0,
    reaction: function (p: Prop) { p.rainT = 5; blip(340, 0.2, 'sine', 0.05); },
  });
  // snowman chain: three piles, only interactive in snow weather
  const snowXs = [snowX - 19, snowX + 2, snowX + 18];
  ([['big', 22, 0], ['medium', 15, 1], ['small', 9, 2]] as Array<[string, number, number]>).forEach((s, idx) => {
    add({
      key: 'snowpile', name: 'Snow pile', layer: 2, x: snowXs[idx], w: s[1], h: s[1] * 0.7, rank: s[2],
      visibleIf: function () { return state.WEATHER[state.weather] === 'snow'; },
      reaction: function (p: Prop) {
        const need = (W().flags.snowmanStep as number) || 0;
        const a = at(p);
        W().puff(a.x, a.top + 4, '#F4F8FB', 5, { spread: 26, up: 30, g: 160, life: 0.6 });
        if ((p.rank as number) === need) {
          W().flags.snowmanStep = need + 1; blip(500 + need * 100, 0.15);
          if ((W().flags.snowmanStep as number) >= 3 && !W().flags.snowmanBuilt) {
            W().flags.snowmanBuilt = true;
            W().props.push({
              key: 'snowman', name: 'Snowman', layer: 2, x: snowXs[1], w: 16, h: 26, waved: false, hb: { w: 24, h: 34 },
              visibleIf: function () { return state.WEATHER[state.weather] === 'snow'; },
              reaction: function (sp2: Prop) { sp2.waved = true; blip(660, 0.2); findEgg('snowman'); },
            });
          }
        } else { W().flags.snowmanStep = 0; blip(180, 0.2, 'square'); }
      },
    });
  });
  // constellation chain: five fixed stars, only interactive at night
  const starXs = [0.15, 0.30, 0.45, 0.60, 0.75].map((f) => f * state.W);
  const starRanks = [3, 5, 1, 4, 2];
  starXs.forEach((sx, i) => {
    add({
      key: 'star', name: 'Star', layer: 'sky', x: sx, y: state.H * (0.08 + 0.05 * i), w: 10, h: 10, rank: starRanks[i],
      visibleIf: function () { return W().isNight(); },
      reaction: function (p: Prop) {
        const order = [5, 4, 3, 2, 1];
        const need = order[(W().flags.constStep as number) || 0];
        if (p.rank === need) {
          W().flags.constStep = ((W().flags.constStep as number) || 0) + 1; blip(700 + (p.rank as number) * 80, 0.12);
          if ((W().flags.constStep as number) >= 5) { findEgg('constellation'); W().popFx(state.W * 0.5, state.H * 0.15, '* * * * *'); }
        } else { W().flags.constStep = 0; blip(160, 0.2, 'square'); }
      },
    });
  });

  return arr;
}

export function shakeProp(p: Prop): void {
  (p as Prop & { shakeT: number }).shakeT = 0.5;
}
