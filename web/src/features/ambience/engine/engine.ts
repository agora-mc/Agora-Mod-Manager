/**
 * AmbienceEngine — owns the canvases, the frame loop, input wiring and the
 * profile. The engine is plain TypeScript with no React and no Tauri; React's
 * entire job is `new AmbienceEngine(canvas, fx, opts)` + `start()`/`stop()`.
 *
 * Lifecycle/leak contract (V5-PORT-PLAN trap 9): every listener, timer and
 * animation frame created here is released in `stop()`. React strict-mode
 * double-mounting and the 50x mount/unmount test exercise this.
 *
 * Profile split (plan §2 / §10):
 *  - `calm`: terrain, sky, weather, few animals. No eggs, no buddy, no
 *    click particles. Music optional.
 *  - `full`: everything — eggs, buddy, particles, music.
 *  - `off`: no canvas, no rAF (AmbienceCanvas does not mount).
 */

import { buddyStep, createBuddy, drawBuddy, buddySnap, buddyTarget, buddyVisible, type BuddyState } from './buddy';
import { advanceClock, createClock, setWeather as clockSetWeather, type ClockState } from './clock';
import { viewToCanvas, worldViewport } from './terrain';
import { checkAchievements, journalData, loadJournalState, saveJournalState, JOURNAL_KEY } from './eggs';
import { music } from './audio/music';
import type { MusicTrack } from './audio/tracks';
import { blip as sfxBlip, resumeSfx } from './audio/sfx';
import { ParticleLayer } from './particles';
import { createEngineState, type AmbienceEvent, type EngineState } from './state';
import { skyFrame } from './sky';
import type { WorldState } from './types';
import { createWorld } from './world';

export type AmbienceProfile = 'off' | 'calm' | 'full';

export interface AmbienceEngineOptions {
  profile?: AmbienceProfile;
  /** Music volume 0..1 (default 0.35, the prototype's default). */
  musicVolume?: number;
  musicOn?: boolean;
  /** Override prefers-reduced-motion (the shell's motion setting wins). */
  reducedMotion?: boolean;
  /** Initial time of day 0..1 (default 0.3). */
  tod?: number;
  /** Day-fraction per second. Default 1/900 -> a 15-minute day. */
  todSpeed?: number;
  onEvent?: (ev: AmbienceEvent) => void;
  /**
   * Bounds the world is fitted to, in client coordinates. Defaults to the
   * whole window ({0, 0, innerWidth, innerHeight}, with the existing
   * clientWidth / 1280×720 fallbacks) — omit for the desktop behaviour.
   * Supplying this fits the world to a box instead of the viewport: the
   * fixed-width world SCALES DOWN to the box rather than cropping, and every
   * pointer coordinate is offset by bounds.left/top before it is normalised,
   * painted on the fx canvas, or converted to world units.
   */
  getBounds?: () => { left: number; top: number; w: number; h: number };
  /**
   * DOM element the world's interaction is scoped to. When provided:
   * pointermove/pointerdown bind to the host, the capture-phase clicks bind
   * to the host (so clicking elsewhere on the page cannot poke the world),
   * and a ResizeObserver refits the world when the host resizes without a
   * window resize. Omit for the desktop defaults (window/document listeners,
   * window resize/load only). The observer is disconnected in stop().
   */
  host?: HTMLElement;
}

/** Emitted once the world is built (React can open the Field Journal). */
export interface AmbienceReady {
  world: WorldState;
  journal: ReturnType<typeof journalData>;
}

const INTERACTIVE_SELECTOR = [
  'button', 'a', 'input', 'select', 'textarea',
  '[role="button"]', '[role="link"]', '[role="menuitem"]', '[role="tab"]', '[role="checkbox"]',
  '[role="switch"]', '[role="option"]', '[role="radio"]', '[contenteditable]', '[data-radix-dialog-content]',
].join(',');

export class AmbienceEngine {
  readonly state: EngineState;
  private bg: HTMLCanvasElement;
  private fx: HTMLCanvasElement;
  private bgCtx: CanvasRenderingContext2D;
  private fxCtx: CanvasRenderingContext2D;
  private particles = new ParticleLayer();
  private buddy: BuddyState;
  /**
   * Zoom + pan. Applied in the canvas's own drawing transform, so a zoomed
   * view is redrawn at full resolution. It used to be a CSS transform on the
   * element, which only magnified the pixels already drawn: zoom in and the
   * whole world went soft.
   */
  private view = { zoom: 1, tx: 0, ty: 0 };
  /** Buffer pixels per client px (the display's pixel ratio, capped). */
  private dpr = 1;
  /**
   * The background canvas's untransformed top-left, in client coordinates.
   *
   * Zero only while the canvas is exactly the viewport. Zooming OUT grows it
   * past the viewport and `size()` centres it with a negative offset — which
   * every screen→canvas conversion has to subtract back off.
   */
  private origin = { left: 0, top: 0 };
  /**
   * The background canvas element's untransformed size, in client px.
   *
   * Not the same as its buffer size in world units: the world has fixed
   * borders and is scaled to the window, so `css.w / scale === state.W`.
   */
  private css = { w: 0, h: 0 };
  /** Client px per world unit — the fixed-width world's fit to this window. */
  private scale = 1;
  /** When the last pointermove arrived; a long gap means the cursor left. */
  private lastPointerMs = 0;
  /** Last pointer position, in both viewport and host px. Null until the pointer moves. */
  private pointer: { clientX: number; clientY: number; x: number; y: number } | null = null;
  /** The site never holds the world still (reduced motion skips it entirely), so there is nothing to repaint. */
  private scheduleStill(): void { /* no still frame on the web */ }
  private profile: AmbienceProfile;
  private options: AmbienceEngineOptions;
  private running = false;
  private raf = 0;
  private blinkUntil = 0;
  private listeners: Array<{ el: EventTarget; type: string; fn: EventListener; capture?: boolean }> = [];
  private timers: number[] = [];
  /** ResizeObservers on a boxed host; disconnected in stop(). */
  private resizers: ResizeObserver[] = [];
  private clock: ClockState = createClock();

  constructor(bg: HTMLCanvasElement, fx: HTMLCanvasElement, options: AmbienceEngineOptions = {}) {
    this.bg = bg;
    this.fx = fx;
    this.bgCtx = bg.getContext('2d') as CanvasRenderingContext2D;
    this.fxCtx = fx.getContext('2d') as CanvasRenderingContext2D;
    this.options = options;
    this.state = createEngineState(options.reducedMotion);
    this.state.tod = options.tod ?? 0.3;
    this.state.soundOn = false;
    this.profile = options.profile ?? 'full';
    this.particles.reduced = this.state.reduce;
    this.buddy = createBuddy(this.state.reduce, 0, 0);
  }

  get profileValue(): AmbienceProfile { return this.profile; }

  getWorld(): WorldState | null {
    return this.state.world as WorldState | null;
  }

  journal(): ReturnType<typeof journalData> {
    return journalData(this.state);
  }

  /** Set the ambient profile live. */
  setProfile(p: AmbienceProfile): void {
    this.profile = p;
    this.particles.reduced = this.state.reduce || p === 'calm';
    buddyVisible(this.buddy, p === 'full' && !this.state.reduce);
    this.bg.style.cursor = p === 'full' ? 'default' : 'default';
  }

  setSoundOn(on: boolean): void {
    this.state.soundOn = on;
  }

  /**
   * Set the weather explicitly.
   *
   * Setting `state.weather` alone did nothing visible: `advanceClock` recomputes
   * the weather from its own timer on EVERY frame, so a manual choice was
   * overwritten about 16ms later. Reposition the clock instead (clock.ts already
   * exposes exactly that), and hold it there — a person who picks "Snow" in a
   * settings panel means "make it snow", not "make it snow until the cycle
   * disagrees".
   */
  setWeather(id: string): void {
    const idx = this.state.WEATHER.indexOf(id as never);
    if (idx < 0) return;
    clockSetWeather(this.state, this.clock, idx);
    this.state.weatherLocked = true;
  }

  /** Pin the weather where it is, or hand it back to the automatic cycle. */
  setWeatherLocked(on: boolean): void {
    this.state.weatherLocked = on;
  }

  /** Pin the time of day where it is, or let the day run again. */
  setTodLocked(on: boolean): void {
    this.state.todLocked = on;
  }

  /**
   * What the world's clock actually reads right now.
   *
   * Controls that mirror the clock (a time-of-day slider, the weather pills)
   * have to be told, or they drift: the day keeps turning and the cycle keeps
   * changing the weather while the panel still shows whatever was last set by
   * hand.
   */
  clockState(): { tod: number; weather: string; todLocked: boolean; weatherLocked: boolean } {
    return {
      tod: this.state.tod,
      weather: this.state.WEATHER[this.state.weather] ?? 'clear',
      todLocked: this.state.todLocked,
      weatherLocked: this.state.weatherLocked,
    };
  }

  /** Set the background zoom (0.5–2). */
  setZoom(z: number): void {
    // 0.9 floor matches the slider: below that the cover-canvas has to generate
    // a lot of extra world that is then scaled into near-invisibility.
    const next = Math.max(0.9, Math.min(2, z));
    const changedCoverage = (next < 1) !== (this.view.zoom < 1) || next < 1;
    this.view.zoom = next;
    if (next <= 1) { this.view.tx = 0; this.view.ty = 0; }
    else this.setPan(this.view.tx, this.view.ty);   // keep the pan inside the new limits
    if (changedCoverage) this.size();   // re-cover the viewport at the new scale
    this.applyView();
  }

  /** Pan the world (only meaningful while zoomed in). */
  setPan(tx: number, ty: number): void {
    // Bound the pan so the world can never be dragged fully off-screen. The
    // pan is client px, so it bounds against the ELEMENT, not the world units
    // drawn inside it.
    const b = this.bounds();
    const w = this.css.w || b.w || 1;
    const h = this.css.h || b.h || 1;
    const maxX = Math.max(0, (w * (this.view.zoom - 1)) / 2);
    const maxY = Math.max(0, (h * (this.view.zoom - 1)) / 2);
    this.view.tx = Math.max(-maxX, Math.min(maxX, tx));
    this.view.ty = Math.max(-maxY, Math.min(maxY, ty));
    this.applyView();
    this.scheduleStill();
  }

  /** Pan by a number of client px (a controller's right stick). */
  panBy(dx: number, dy: number): void {
    if (this.view.zoom <= 1) return;
    this.setPan(this.view.tx + dx, this.view.ty + dy);
  }

  /* ── a cursor that is not the mouse ───────────────────────────────────
   * A controller (or anything else without a pointer) drives the world
   * through these: the engine keeps a cursor position, treats moving it
   * exactly like moving the mouse, and draws a small reticle there so there
   * is something to aim with. The first real pointer move hands control back.
   */

  /** True while the cursor is being driven by `moveCursorBy`, not the mouse. */
  private virtualCursor = false;

  /** Move the cursor by client px. Pushing against an edge pans a zoomed view. */
  moveCursorBy(dx: number, dy: number): void {
    const b = this.bounds();
    const from = this.pointer ?? { x: b.w / 2, y: b.h / 2 };
    let x = from.x + dx, y = from.y + dy;
    if (this.view.zoom > 1) {
      const m = 24;
      if (x < m) { this.panBy(m - x, 0); x = m; }
      if (x > b.w - m) { this.panBy(b.w - m - x, 0); x = b.w - m; }
      if (y < m) { this.panBy(0, m - y); y = m; }
      if (y > b.h - m) { this.panBy(0, b.h - m - y); y = b.h - m; }
    }
    x = Math.max(2, Math.min(b.w - 2, x));
    y = Math.max(2, Math.min(b.h - 2, y));
    this.virtualCursor = true;
    this.pointTo(x + b.left, y + b.top);
    this.scheduleStill();
  }

  /** Show or hide the reticle (hidden also drops the hover highlight). */
  setCursorVisible(on: boolean): void {
    if (on && !this.pointer) { this.moveCursorBy(0, 0); return; }
    this.virtualCursor = on;
    if (!on) {
      const world = this.state.world as WorldState | null;
      if (world) world.hover = null;
    }
    this.scheduleStill();
  }

  /** Press on whatever is under the cursor, as a click would. */
  clickAtCursor(): void {
    if (!this.pointer) this.moveCursorBy(0, 0);
    const p = this.pointer;
    if (!p) return;
    this.worldClick(p.x, p.y);
    if (!this.state.reduce && this.profile !== 'calm') this.particles.burst(p.x, p.y, '#9FF5E2', 6, 5);
    this.scheduleStill();
  }

  /** Put down whatever is being carried, at the cursor. Returns false if empty-handed. */
  dropCarried(): boolean {
    const world = this.state.world as WorldState | null;
    if (!world || !world.carry) return false;
    const p = this.pointer ?? { x: this.bounds().w / 2, y: this.bounds().h / 2 };
    const c = this.toCanvas(p.x, p.y);
    world.drop(c.x, c.y);
    this.scheduleStill();
    return true;
  }

  /** A click at a box-relative point: interact with what is there, or drop the carried item. */
  private worldClick(x: number, y: number): void {
    const world = this.state.world as WorldState | null;
    if (!world) return;
    const p = this.toCanvas(x, y);
    const hit = world.hit(p.x, p.y);
    if (hit) world.interact(hit);
    else if (world.carry) world.drop(p.x, p.y);
  }

  /** The reticle, drawn on the fx canvas (client px). */
  private drawReticle(): void {
    if (!this.virtualCursor || !this.pointer) return;
    const c = this.fxCtx, x = Math.round(this.pointer.x), y = Math.round(this.pointer.y);
    c.save();
    c.globalAlpha = 1;
    for (const [col, o] of [['rgba(8,14,26,.85)', 1], ['#FFFFFF', 0]] as Array<[string, number]>) {
      c.fillStyle = col;
      c.fillRect(x - 11 - o, y - 1 - o, 7 + o * 2, 2 + o * 2);
      c.fillRect(x + 4 - o, y - 1 - o, 7 + o * 2, 2 + o * 2);
      c.fillRect(x - 1 - o, y - 11 - o, 2 + o * 2, 7 + o * 2);
      c.fillRect(x - 1 - o, y + 4 - o, 2 + o * 2, 7 + o * 2);
    }
    c.fillStyle = '#9FF5E2';
    c.fillRect(x - 1, y - 1, 2, 2);
    c.restore();
  }

  getView(): { zoom: number; tx: number; ty: number } { return { ...this.view }; }

  private applyView(): void {
    // Nothing to style: the view is part of the drawing transform, set at the
    // top of every frame. The element itself always fills its box.
    this.bg.style.transform = '';
  }

  /**
   * Screen point → canvas point, undoing the view transform.
   *
   * The zoom is a CSS transform on the canvas, so the pixels move but the
   * canvas's own coordinate system does not. Hit tests were run against raw
   * client coordinates, which meant that as soon as you zoomed, everything was
   * clickable somewhere other than where it was drawn.
   *
   * The canvas's own offset matters just as much: at the default zoom of 0.9
   * the canvas is ~11% wider than the window and centred over it, so ignoring
   * `origin` put every hit test roughly 6% of the window's width away from the
   * cursor — the further from the middle you clicked, the further off it was.
   *
   * Inputs are expected in the box's own coordinate space (0,0 at the box's
   * top-left): `origin` is the canvas's offset WITHIN the box, so a
   * viewport-relative pointer must have bounds.left/top subtracted first. The
   * default box is the window at {0,0}, making that subtraction a no-op on
   * desktop.
   */
  private toCanvas(clientX: number, clientY: number): { x: number; y: number } {
    const b = this.bounds();
    const w = this.css.w || b.w || 1;
    const h = this.css.h || b.h || 1;
    return viewToCanvas(clientX, clientY, this.view, w, h, this.origin.left, this.origin.top, this.scale);
  }

  /**
   * Put a rainbow up, or take it down.
   *
   * The world only ever spawned one as a reward for rain→sun. It is the single
   * prettiest thing in the scene, so it also deserves to be something you can
   * simply ask for — and one you asked for is PINNED: it does not fade with the
   * shower's, it stays until you take it down.
   */
  setRainbow(on: boolean): void {
    const world = this.state.world as WorldState | null;
    if (!world) return;
    world.flags.rainbowPinned = on;
    if (on) world.spawnRainbow();
    else world.clearRainbow();
  }

  /** Play a specific piece. Implies the player is choosing, so autoplay stops. */
  setTrack(id: string): void {
    this.musicAuto = false;
    this.withTracks((all) => {
      const track = all.find((t) => t.id === id);
      if (track) music.start(track);
    });
  }

  /** Force an instrument for every track until the player picks another. */
  setInstrument(id: string): void {
    music.setInstrument(id, true);
  }

  /** Toggle the cursor companion (buddy) on/off. */
  setBuddy(on: boolean): void {
    buddyVisible(this.buddy, on && !this.state.reduce);
  }

  /** For the High Interaction UI's drag-to-rearrange (prototype's justDragged). */
  noteDrag(): void {
    this.state.justDragged = Date.now();
  }

  /**
   * Start or stop music. tracks.ts is lazy-imported so it never lands in the
   * initial chunk (plan §6).
   *
   * "Let it choose" draws from the WHOLE library through a shuffle bag (see
   * `nextTrack`). Two earlier versions both collapsed the 37-minute library
   * into a handful of pieces: the first took `[0]` of a mood and looped that
   * one track forever, and the replacement filtered to the mood of the hour,
   * which is five pieces by day (three of them Gymnopedies, which a listener
   * hears as one piece) and three at night. Neither ever reached Bumblebee,
   * Sugar Plum, Mountain King or Fate at all.
   */
  setMusicOn(on: boolean): void {
    if (!on) {
      music.onPieceEnd = null;
      music.stop();
      return;
    }
    // Idempotent: turning music "on" while it is already playing must not yank
    // the current piece and start another. Only the end of a piece (or an
    // explicit pick) changes the track.
    if (music.isPlaying()) return;
    this.withTracks((all) => this.playNextAuto(all));
  }

  /** The track library, cached after the first lazy load. */
  private tracks: typeof import('./audio/tracks') | null = null;

  /**
   * Run `fn` with the track library, autoplay's end-of-piece hook attached.
   *
   * The hook used to be installed by `setMusicOn` alone, so only the path that
   * turned music on ever wired it. Pinning a piece starts playback without
   * going through there and left `onPieceEnd` null, so handing the choice back
   * to "Let it choose" afterwards had nothing listening at the loop boundary
   * and the pinned piece repeated for the rest of the session.
   */
  private withTracks(fn: (all: MusicTrack[]) => void): void {
    const run = (m: typeof import('./audio/tracks')): void => {
      this.tracks = m;
      music.onPieceEnd = () => {
        if (!this.musicAuto) return;
        this.playNextAuto(m.MUSIC_TRACKS);
      };
      fn(m.MUSIC_TRACKS);
    };
    if (this.tracks) { run(this.tracks); return; }
    void import('./audio/tracks').then(run);
  }

  /**
   * Start autoplay's next piece, unless it is the one already playing.
   *
   * The second draw covers a piece the player pinned by hand: `setTrack` does
   * not deal from the bag, so this cycle's bag can still hold the id that is
   * playing right now, and restarting it would look like the control did
   * nothing. A bag never holds an id twice, so one more draw always clears it.
   */
  private playNextAuto(all: MusicTrack[]): void {
    let next = this.nextTrack(all);
    if (next && next.id === music.currentTrackId()) next = this.nextTrack(all) ?? next;
    if (next && next.id !== music.currentTrackId()) music.start(next);
  }

  /** Autoplay's running order: a shuffled bag, refilled once it empties. */
  private musicBag: string[] = [];

  /**
   * The next piece for autoplay.
   *
   * A bag, not a fresh `Math.random()` per piece: independent draws are happy
   * to serve the same three pieces all evening, and "avoid the one that just
   * played" only rules out an immediate repeat. Dealing from a shuffled bag
   * plays every piece in the library once before any piece plays twice, which
   * is what "let it choose" is expected to mean.
   */
  private nextTrack(all: MusicTrack[]): MusicTrack | null {
    if (all.length === 0) return null;
    if (this.musicBag.length === 0) {
      const ids = all.map((t) => t.id);
      for (let i = ids.length - 1; i > 0; i--) {
        const j = Math.floor(Math.random() * (i + 1));
        [ids[i], ids[j]] = [ids[j], ids[i]];
      }
      // A fresh bag must not open with the piece the last one closed on —
      // the only way a bag can produce a back-to-back repeat.
      const current = music.currentTrackId();
      if (ids.length > 1 && ids[0] === current) {
        const k = 1 + Math.floor(Math.random() * (ids.length - 1));
        [ids[0], ids[k]] = [ids[k], ids[0]];
      }
      this.musicBag = ids;
    }
    const id = this.musicBag.shift();
    return all.find((t) => t.id === id) ?? null;
  }

  /**
   * Whether autoplay may rotate pieces when one ends.
   *
   * There is no separate switch for this: picking a named piece in the Living
   * Background panel turns it off, and picking "Let it choose" turns it back
   * on.
   */
  musicAuto = true;
  /**
   * Flag only, deliberately: the panel calls this on open, when its selector
   * is showing "Let it choose", to bring the engine back in line with what the
   * player is being told. Cutting a piece off mid-phrase every time somebody
   * opened the panel would be worse than letting the current one finish.
   */
  setMusicAuto(on: boolean): void { this.musicAuto = on; }

  /**
   * The player chose "Let it choose" just now: hand the running order back to
   * autoplay and move to a fresh piece immediately.
   *
   * Setting the flag alone makes the control look dead — a pinned piece loops
   * on until its next boundary, minutes away for most of this library, with
   * nothing to show the pick took. Music that is off stays off; this is a
   * choice about running order, not a play button.
   */
  shuffleNow(): void {
    this.musicAuto = true;
    if (!music.isPlaying()) return;
    this.withTracks((all) => this.playNextAuto(all));
  }

  setMusicVolume(v: number): void {
    music.setVolume(v);
  }

  /** Advance / set the time of day (0..1). */
  setTod(t: number): void {
    this.state.tod = t;
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    this.size();
    if (!this.state.world) this.buildWorld();
    this.attach();
    // music may start only after a user gesture; the shell calls
    // `engine.unlockAudio()` from a pointerdown to begin playback.
    if (this.options.musicOn) {
      const kick = () => {
        this.setMusicOn(true);
        window.removeEventListener('pointerdown', kick);
      };
      window.addEventListener('pointerdown', kick);
      this.listeners.push({ el: window, type: 'pointerdown', fn: kick });
    }
    this.raf = requestAnimationFrame(this.frame);
  }

  /** Resume the shared AudioContext after a user gesture (browser autoplay). */
  unlockAudio(): void {
    resumeSfx();
    if (this.options.musicOn) this.setMusicOn(true);
  }

  stop(): void {
    if (!this.running) return;
    this.running = false;
    cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.listeners.forEach(({ el, type, fn, capture }) => el.removeEventListener(type, fn, capture));
    this.listeners = [];
    this.resizers.forEach((r) => r.disconnect());
    this.resizers = [];
    this.timers.forEach((t) => clearTimeout(t));
    this.timers = [];
    music.stop();
    this.persist();
  }

  /** Persist the journal (called on stop and after discoveries). */
  private persist(): void {
    try {
      const raw = saveJournalState(this.state);
      if (raw !== null) window.localStorage.setItem(JOURNAL_KEY, raw);
    } catch {
      // journal persistence is best-effort
    }
  }

  /**
   * The box the world is fitted to — the whole window by default, matching
   * the pre-option behaviour exactly (same fallbacks, zero offset).
   */
  private bounds(): { left: number; top: number; w: number; h: number } {
    if (this.options.getBounds) return this.options.getBounds();
    return {
      left: 0,
      top: 0,
      w: window.innerWidth || document.documentElement.clientWidth || 1280,
      h: window.innerHeight || document.documentElement.clientHeight || 720,
    };
  }

  private size(): void {
    // canvas is a replaced element: inset:0 alone leaves it at intrinsic 0x0.
    // innerWidth (or a host's rect) can still be 0 on the first tick in an
    // embedded frame; fall back and re-measure so the backdrop never stays a
    // 0x0 buffer.
    const b = this.bounds();
    const vw = b.w;
    const vh = b.h;
    // The world is a fixed-size place fitted to the box, NOT a box-sized
    // one: `v.W` follows the zoom alone, and a wider box raises `v.scale`
    // instead. Widening used to widen the world itself — more hills generated
    // past the old edge, the sun's arc and every W-relative spawn stretched
    // apart — so the map grew with the window instead of being drawn larger.
    // A smaller box therefore SCALES THE WORLD DOWN — it does not crop it.
    const v = worldViewport(vw, vh, this.view.zoom);
    // Remember where the canvas actually sits: `toCanvas` has to undo this
    // offset, and reading it back off the style string every pointermove would
    // be both slower and a chance to drift out of sync. The offset is WITHIN
    // the box, so it stays box-relative whether the box is the window or a
    // position:absolute host.
    this.origin.left = v.left;
    this.origin.top = v.top;
    this.css.w = v.cssW;
    this.css.h = v.cssH;
    this.scale = v.scale;
    // The element always fills the box exactly. `css`/`origin` describe the
    // world's untransformed footprint (larger than the box and centred over it
    // when zoomed out) and are what the drawing transform and `toCanvas` use.
    this.bg.style.width = `${vw}px`;
    this.bg.style.height = `${vh}px`;
    this.bg.style.left = '0px';
    this.bg.style.top = '0px';
    // One buffer pixel per DEVICE pixel, so the world is sharp on scaled
    // displays too, with a ceiling so a huge window cannot allocate a huge
    // canvas. The fit, the zoom and the pan all ride on the context transform.
    const ratio = (typeof window !== 'undefined' && window.devicePixelRatio) || 1;
    this.dpr = Math.max(1, Math.min(ratio, 4096 / Math.max(1, vw), 3));
    const bw = Math.max(1, Math.round(vw * this.dpr));
    const bh = Math.max(1, Math.round(vh * this.dpr));
    if (this.bg.width !== bw) this.bg.width = bw;
    if (this.bg.height !== bh) this.bg.height = bh;
    this.state.W = v.W;
    this.state.H = v.H;
    this.applyWorldScale();
    // The FX canvas must NOT get the cover treatment. Particles and the buddy
    // are positioned from box-relative coordinates, so its canvas space has to
    // stay 1:1 with the box — grown or offset like the background, every spark
    // would draw at the wrong place. The canvas sits at the box's own origin
    // (0/0 inside a position:absolute host); the box's offset from the page is
    // the pointer handler's subtraction, not this canvas's position.
    this.fx.width = vw;
    this.fx.height = vh;
    this.fx.style.width = `${vw}px`;
    this.fx.style.height = `${vh}px`;
    this.fx.style.left = '0px';
    this.fx.style.top = '0px';
  }

  /**
   * Point the background context at world units.
   *
   * Assigning `canvas.width`/`height` resets the context — transform included —
   * so this has to run after every resize. It also runs at the top of every
   * frame: it costs one call, and a drawing routine that ever left the save
   * stack unbalanced would otherwise silently shift the entire world.
   *
   * The FX canvas is left alone on purpose. Particles and the buddy are placed
   * from raw client coordinates, so their context stays 1:1 with the viewport.
   */
  private applyWorldScale(): void {
    // world unit -> client px is `(u * scale - css/2) * zoom + css/2 + pan + origin`
    // (the inverse of `viewToCanvas`), then client px -> buffer px is `* dpr`.
    const { zoom, tx, ty } = this.view;
    const k = this.dpr * this.scale * zoom;
    const ox = this.dpr * ((this.css.w / 2) * (1 - zoom) + tx + this.origin.left);
    const oy = this.dpr * ((this.css.h / 2) * (1 - zoom) + ty + this.origin.top);
    this.bgCtx.setTransform(k, 0, 0, k, ox, oy);
    // Pixel art: when anything does get resampled, keep the edges hard.
    this.bgCtx.imageSmoothingEnabled = false;
  }

  private buildWorld(): void {
    const state = this.state;
    const emit = (ev: AmbienceEvent) => {
      if (this.profile === 'calm' && (ev.type === 'discovery' || ev.type === 'achievement')) return;
      if (ev.type === 'discovery') {
        music.duck();
      }
      this.options.onEvent?.(ev);
      if (ev.type === 'discovery' || ev.type === 'achievement' || ev.type === 'completion') this.persist();
    };
    const world = createWorld(state, {
      blip: (f, d, t, v) => this.blip(f, d, t, v),
      burst: (x, y, c, n, s) => this.particles.burst(x, y, c, n, s),
      emit,
    });
    state.world = world;
    // journal restore
    let raw: string | null = null;
    try { raw = window.localStorage.getItem(JOURNAL_KEY); } catch { /* ignore */ }
    const saved = loadJournalState(state, raw);
    if (saved) {
      (saved.unlocked || []).forEach((name) => { state.unlocked[name] = true; });
      checkAchievements(state, emit);
    }
  }

  private blip(f: number, d?: number, t?: OscillatorType, v?: number): void {
    // SFX routed through the shared audio; respects the engine's soundOn flag
    if (!this.state.soundOn) return;
    sfxBlip(this.state, f, d, t, v);
  }

  /**
   * The cursor is at this viewport point: feed parallax, the companion, the
   * trail and the hover test. Shared by real pointer moves and the virtual
   * cursor.
   *
   * Pointer coordinates arrive viewport-relative (clientX/Y), but the world
   * draws into a box whose own origin is bounds.left/top. Offset once before
   * normalising or before feeding anything that paints on the fx canvas or
   * hits the world. The default bounds are {0,0} + the window, so this is a
   * no-op on desktop.
   */
  private pointTo(clientX: number, clientY: number): void {
    const state = this.state;
    const b = this.bounds();
    const x = clientX - b.left;
    const y = clientY - b.top;
    state.mx = x / (b.w || 1);
    state.my = y / (b.h || 1);
    // A gap in pointer events means the cursor was somewhere we could not see
    // it — off-window, or over another surface — and it can come back
    // anywhere. Easing from the stale position looks like the companion is
    // lost; snap it to where the cursor actually is and carry on.
    const now = performance.now();
    const gap = now - this.lastPointerMs;
    this.lastPointerMs = now;
    if (gap > 400) {
      buddySnap(this.buddy, x, y);
      this.particles.pointerJumped?.(x, y);
    }
    this.particles.pointerMoved(x, y);
    buddyTarget(this.buddy, x, y);
    this.pointer = { clientX, clientY, x, y };
    const p = this.toCanvas(x, y);
    state.pcx = p.x;
    state.pcy = p.y;
    if (state.world) {
      const w = state.world as WorldState;
      const m = w.hit(p.x, p.y);
      if (m !== w.hover) this.scheduleStill();
      w.hover = m;
      this.bg.style.cursor = m ? 'pointer' : this.view.zoom > 1 ? 'grab' : 'default';
    }
  }

  private attach(): void {
    const state = this.state;
    // A host box scopes the world's interaction to itself: pointermove follows
    // the cursor only while it is over the host, and a click anywhere else on
    // the page must not poke the world. Without a host everything stays on
    // window/document exactly as before.
    const host = this.options.host ?? null;
    const on = (el: EventTarget, type: string, fn: EventListener, capture?: boolean) => {
      el.addEventListener(type, fn, capture);
      this.listeners.push({ el, type, fn, capture });
    };
    const resize = () => { this.size(); };
    on(window, 'resize', resize);
    on(window, 'load', resize);
    // A boxed host can resize without a window resize (layout shift, font
    // load, container query), so watch it directly. Disconnected in stop(),
    // keeping the mount/unmount leak contract intact.
    if (host) {
      const ro = new ResizeObserver(resize);
      ro.observe(host);
      this.resizers.push(ro);
    }

    // Pointer coordinates arrive viewport-relative (clientX/Y), but the world
    // draws into a box whose own origin is bounds.left/top. Offset once before
    // normalising or before feeding anything that paints on the fx canvas or
    // hits the world. The default bounds are {0,0} + the window, so this is a
    // no-op on desktop.
    const pointerMove = ((e: PointerEvent) => {
      this.virtualCursor = false;
      this.pointTo(e.clientX, e.clientY);
    }) as EventListener;
    on(host ?? window, 'pointermove', pointerMove);

    // World clicks: any click that is not on an interactive control is a
    // background click, which the world owns (capture-phase so it sees the
    // click before any panel handles it; we never stopPropagation).
    const click = ((e: PointerEvent) => {
      if (Date.now() - (state.justDragged || 0) < 250) return;
      const target = e.target as Element | null;
      if (target && target.closest && target.closest(INTERACTIVE_SELECTOR)) return;
      const b = this.bounds();
      this.worldClick(e.clientX - b.left, e.clientY - b.top);
    }) as EventListener;
    on(host ?? document, 'click', click, true);

    /**
     * Drag to pan, but only while zoomed in and only when the press did not
     * land on something interactive. Zooming into a corner of the world is
     * useless if you cannot then move around it; and a drag that started on a
     * prop or a UI control must stay that interaction, not become a pan.
     */
    let dragging: { x: number; y: number; tx: number; ty: number } | null = null;
    const panDown = ((e: PointerEvent) => {
      if (this.view.zoom <= 1 || e.button !== 0) return;
      const target = e.target as Element | null;
      if (target && target.closest && target.closest(INTERACTIVE_SELECTOR)) return;
      const world = state.world as WorldState | null;
      const b = this.bounds();
      const p = this.toCanvas(e.clientX - b.left, e.clientY - b.top);
      if (world && world.hit(p.x, p.y)) return;   // that press belongs to the prop
      dragging = { x: e.clientX, y: e.clientY, tx: this.view.tx, ty: this.view.ty };
      this.bg.style.cursor = 'grabbing';
    }) as EventListener;
    const panMove = ((e: PointerEvent) => {
      if (!dragging) return;
      const dx = e.clientX - dragging.x;
      const dy = e.clientY - dragging.y;
      this.setPan(dragging.tx + dx, dragging.ty + dy);
      // Suppress the click that ends a real drag, so panning never also pokes
      // whatever ended up under the cursor.
      if (Math.abs(dx) + Math.abs(dy) > 4) state.justDragged = Date.now();
    }) as EventListener;
    const panUp = (() => {
      if (!dragging) return;
      dragging = null;
      this.bg.style.cursor = this.view.zoom > 1 ? 'grab' : 'default';
    }) as EventListener;
    // A pan may only START inside the box; move/up/cancel stay on window so a
    // drag that leaves the host still ends cleanly.
    on(host ?? window, 'pointerdown', panDown);
    on(window, 'pointermove', panMove);
    on(window, 'pointerup', panUp);
    on(window, 'pointercancel', panUp);

    // global click burst (capture-phase, nothing can suppress it — trap 1)
    const burstClick = ((e: MouseEvent) => {
      if (this.state.reduce || this.profile === 'calm') return;
      const b = this.bounds();
      this.particles.burst(e.clientX - b.left, e.clientY - b.top, '#9FF5E2', 6, 5);
    }) as EventListener;
    on(host ?? document, 'click', burstClick, true);

    // blink timer for the buddy (prototype's setInterval(4200))
    const blinkTimer = window.setInterval(() => {
      if (this.buddy.on && !this.state.reduce) this.blinkUntil = performance.now() + 130;
    }, 4200);
    this.timers.push(blinkTimer);

    // visibility: pause everything while hidden (trap 2)
    const vis = () => music.handleVisibility(document.hidden);
    on(document, 'visibilitychange', vis);
  }

  private frame = (ts: number): void => {
    if (!this.running) return;
    const state = this.state;
    // time of day + weather advance with the real clock (plan §2: no debug
    // slider, no weather toggle)
    const todSpeed = this.options.todSpeed ?? 1 / 900;
    advanceClock(state, this.clock, 0.016, todSpeed);

    this.applyWorldScale();
    skyFrame(state, this.bgCtx, ts);
    this.particles.frame(this.fxCtx);

    // buddy (drawn on the fx canvas, moved by the same loop)
    buddyStep(this.buddy, ts);
    if (this.buddy.on && !state.reduce) {
      drawBuddy(this.fxCtx, this.buddy, ts < this.blinkUntil);
    }
    this.drawReticle();
    this.raf = requestAnimationFrame(this.frame);
  };
}
