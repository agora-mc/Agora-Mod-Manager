/**
 * Shared types for the ambience engine.
 *
 * Everything here is type-only (no runtime code) so that `world.ts`,
 * `species.ts`, `props.ts`, `eggs.ts` and `api.ts` can import each other's
 * types without creating runtime import cycles.
 */

import type { VoiceDef } from './api';

export interface Palette {
  body: string;
  accent?: string;
  dark: string;
  /** Derived tones for shading: the lit top and the half-shadow. */
  light?: string;
  mid?: string;
  belly?: string;
  eye?: string;
  /** Antler colour. */
  horn?: string;
  /** Birds: beak and head colours, when they differ from the body. */
  beak?: string;
  head?: string;
}

export interface ShapeDef {
  bw?: number; bh?: number; hw?: number; hh?: number;
  lw?: number; lh?: number; tw?: number; th?: number;
  ew?: number; eh?: number;
  ears?: boolean; tail?: boolean; legs?: boolean;
  earShape?: 'point' | 'round' | 'long' | 'side' | 'none';
  tailShape?: 'stub' | 'bushy' | 'long' | 'flat' | 'cat' | 'none';
  snout?: number; neck?: number; hump?: number;
  /** How it moves: a walk (default) or a bounding hop. */
  gait?: 'walk' | 'hop';
  antlers?: boolean; tusks?: boolean; spikes?: boolean; shell?: boolean;
  mane?: boolean; mask?: boolean;
  /** Antler form: a deer's tined beams (default) or a moose's flat paddles. */
  antlerShape?: 'deer' | 'moose';
  /** A long muzzle that hangs below the jaw, with a bell under the chin (moose). */
  droop?: boolean;
  /** Tabby bars across the back, whiskers at the muzzle. */
  stripes?: boolean; whiskers?: boolean;
  /** Not a four-legged animal at all: a snail, drawn as a foot, a spiral shell and eye stalks. */
  snail?: boolean;
}

export interface Species {
  key: string;
  name: string;
  /** Spawn weight (0 = gated, spawned only when its gate is open). */
  w: number;
  /** D = day, N = night, A = any. */
  t: 'D' | 'N' | 'A';
  layer: number | 'sky' | 'pond';
  layerRange?: number[];
  speed: number;
  size?: number;
  kind: 'bird' | 'quad' | 'flutter' | 'glow' | 'fish';
  group?: boolean;
  perch?: boolean;
  swim?: boolean;
  /** Sprite variant within a `kind` (owl, bat, goose, bee, dragonfly, frog…). */
  style?: string;
  shape?: ShapeDef;
  pal: Palette;
  /** Alternative palettes; one is picked per animal when it spawns. */
  variants?: Palette[];
  voice?: VoiceDef;
  /** F11: gated species (w:0 + gate) spawn directly when the gate opens. */
  gate?: () => boolean;
  accepts?: string[];
  onClick?: (e: Entity) => void;
  behave?: (e: Entity, dt: number) => void;
}

export interface Entity {
  sp: Species;
  layer: number | 'sky' | 'pond';
  dir: number;
  vx: number;
  scale: number;
  state: string;
  t: number;
  stateT: number;
  phase: number;
  fx: string | null;
  data: Record<string, unknown>;
  hb: { w: number; h: number };
  removeAt: number | null;
  /** WORLD x (F7): transformed to screen only at draw/hit time. */
  x: number;
  /** WORLD y — only meaningful for sky (and fish arcs via jy). */
  y: number;
  uid?: string;
  /** F8a: pond & perch residents get a lifespan so they don't crowd out. */
  residentUntil?: number;
  sleepStart?: number;
  sleepFor?: number;
  /** How long a react state lasts (set by reactState). */
  reactDur?: number;
  jy?: number;
  vy?: number;
  waterY?: number;
  pondCX?: number;
  pondR?: number;
  groupMembers?: Entity[];
  groupOffset?: number;
  /** A perched bird's seat: a spot on a prop, in that prop's own units. */
  perch?: PerchSpot;
}

/** Somewhere a bird can sit: `dx`/`dy` are from the prop's base, `dy` upward. */
export interface PerchSpot {
  id: string;
  prop: Prop;
  dx: number;
  dy: number;
  /** Which species may use it. */
  for: string[];
}

/** One world-space pixel particle (leaves, spray, spores, dust). */
export interface Bit {
  x: number;
  y: number;
  vx: number;
  vy: number;
  g: number;
  life: number;
  max: number;
  size: number;
  color: string;
}

/** An axis-aligned box in drawn (parallax-applied) world units. */
export interface Box { x: number; y: number; w: number; h: number; }

export interface Prop {
  key: string;
  name: string;
  layer: number | 'sky' | 'pond';
  /** WORLD x (F7). */
  x: number;
  y?: number;
  /** Some props (the pond) have no blocky w/h — they draw from hb/rw. */
  w?: number;
  h?: number;
  hb?: { w: number; h: number };
  visibleIf?: () => boolean;
  reaction?: (p: Prop) => void;
  /** Per-prop state lives in extra fields (hole, shakeT, bloom, …). */
  [k: string]: unknown;
}

export interface WorldItem {
  id: string;
  /** WORLD x (F7). */
  x: number;
  y: number;
  t: number;
  /** Which depth layer's ground it lies on (default: the foreground). */
  layer?: number;
  /** Height above that ground while it is still falling, and its fall speed. */
  h?: number;
  vh?: number;
}

export interface WorldFx {
  x: number;
  y: number;
  text: string;
  life: number;
  vy: number;
  /** Draw on a backing plate (signpost text), not as a bare glyph. */
  plate?: boolean;
}

export interface HitResult {
  kind: 'item' | 'entity' | 'prop';
  obj: WorldItem | Entity | Prop;
}

export interface SpawnOptions {
  x?: number;
  fromLeft?: boolean;
  /** Extra entity data (e.g. the migration chain). */
  data?: Record<string, unknown>;
  /** Start in the air at this drawn position, already flying (a startled flock). */
  airborne?: { x: number; y: number };
}

/** A carryable item picked up by the player (for the React carry tag). */
export interface LastPickup {
  itemId: string;
  name: string;
  firstTime: boolean;
  at: number;
}

export interface WorldState {
  t: number;
  entities: Entity[];
  props: Prop[];
  items: WorldItem[];
  carry: string | null;
  flags: Record<string, unknown>;
  found: Record<string, boolean>;
  hover: HitResult | null;
  hoverIsProp: boolean;
  dl: number;
  spawnTimer: number;
  sinceLastSpawn: number;
  worldFx: WorldFx[];
  bits: Bit[];
  firstLoad?: number;
  lastInteractAt?: number;
  _pond?: Prop;
  /** Last picked-up carryable (for the React carry tag). */
  lastPickup?: LastPickup;
  /** Event emitter wired by the engine (toasts, journal, sounds). */
  emit?: (ev: import('./state').AmbienceEvent) => void;
  /** Discovery lookup bound after the world is built (prototype's global `findEgg`). */
  _findEgg?: (id: string) => void;

  layerScale(layer: number | 'sky' | 'pond'): number;
  isNight(): boolean;
  isDay(): boolean;
  eligible(sp: Species): boolean;
  spawn(sp: Species, opts?: SpawnOptions): Entity;
  pondProp(): Prop | undefined;
  despawn(e: Entity): void;
  hit(px: number, py: number): HitResult | null;
  nearestProp(x: number, key: string): Prop | null;
  nearPond(x: number): boolean;
  pet(species: string): void;
  dropCarry(): void;
  pickUp(itemId: string, x: number, y: number): void;
  spawnFish(pond: Prop): Entity;
  splash(x: number, y: number): void;
  spawnRainbow(): void;
  checkRainbow(): void;
  /** Take the arc down and reset its lifetime (fade-out, or the egg claiming it). */
  clearRainbow(): void;
  spawnSwarm(): void;
  campfireClick(e: Entity, key: string): void;
  startCampfireTales(fire: Prop): void;
  ringHit(idx: number): void;
  update(dt: number): void;
  popFx(x: number, y: number, text: string, plate?: boolean): void;
  /** Scatter pixel particles at a drawn position. */
  puff(x: number, y: number, color: string, n?: number, opts?: { spread?: number; up?: number; g?: number; life?: number; size?: number }): void;
  /** Lift a bird off whatever it is sitting or floating on and send it flying. */
  takeOff(e: Entity): void;
  /** Where an entity is drawn: its foot/centre line and whether it is swimming. */
  anchorOf(e: Entity): { sx: number; gy: number; cy: number; swim: boolean };
  /** The box that both the hover highlight and the click test use. */
  boxOf(hit: HitResult): Box;
  /** Free perches a species could take right now. */
  freePerches(key: string): PerchSpot[];
  drawEntity(ctx: CanvasRenderingContext2D, e: Entity): void;
  drawProp(ctx: CanvasRenderingContext2D, p: Prop): void;
  drawItem(ctx: CanvasRenderingContext2D, it: WorldItem): void;
  drawHoverGlow(ctx: CanvasRenderingContext2D, hit: HitResult): void;
  draw(ctx: CanvasRenderingContext2D): void;
  /** Fix the daylight shading for the frame about to be drawn. */
  beginDraw(): void;
  /** Draw one depth layer: its props, then its animals. */
  drawLayer(ctx: CanvasRenderingContext2D, layer: number | 'sky' | 'pond'): void;
  /** Draw what sits over every layer: items, particles, text, the highlight. */
  drawOverlay(ctx: CanvasRenderingContext2D): void;
  interact(hit: HitResult): void;
  drop(x: number, y: number): void;
}
