/**
 * Living Background — a full, unobstructed view of the living world so you
 * can actually see it and play with it. The normal shell (page background +
 * blur) is dropped while this page is open; the ambience canvas shows through
 * edge to edge, and the controls below are the same ones the prototype's
 * "@top control bar" exposed (sound, weather, time, music, volume) plus a few
 * extras (zoom, cursor companion).
 *
 * It only appears in the sidebar while the living background is enabled.
 */

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Mountain, Music, Volume2, CloudRain, CloudSnow, Sun, ZoomIn, ZoomOut, Sparkles, Rainbow, Maximize2, ChevronRight, Lock, LockOpen, RotateCcw } from 'lucide-react';
import { MUSIC_TRACK_CHOICES, INSTRUMENT_CHOICES } from '../features/ambience/engine/audio/trackChoices';
import { useAmbience } from '../features/ambience/AmbienceProvider';
import { Switch } from '../components/ui/switch';
import { PageHero } from '../components/PageHero';
import { useController } from '../features/controller/ControllerProvider';
import { useControllerLayer } from '../features/controller/useControllerLayer';
import type { ControllerIntent } from '../features/controller/intents';
import { readGamepadAnalog } from '../lib/useGamepad';
import { reloadPage } from '../lib/reloadPage';

/** Stick deflection below this is drift, not intent. */
const STICK_DEADZONE = 0.16;
/** Cursor and pan speed at full deflection, in px per second. */
const CURSOR_SPEED = 620;
const PAN_SPEED = 520;

/** Deadzone, then a gentle curve so small movements stay precise. */
function shapeAxis(v: number): number {
  const m = Math.abs(v);
  if (m < STICK_DEADZONE) return 0;
  const k = (m - STICK_DEADZONE) / (1 - STICK_DEADZONE);
  return Math.sign(v) * k * (0.35 + 0.65 * k);
}

// Fifth tick of the old range (0.5 + 4×0.1) is now BOTH the minimum and the
// default: below this the world was scaled so far down that the extra terrain
// generated to cover the viewport was mostly wasted, and the props got tiny.
const MIN_ZOOM = 0.9;
const DEFAULT_ZOOM = MIN_ZOOM;

/**
 * The zoom slider, shared by the control panel and the floating copy that
 * takes over its job when the panel is folded away. `action` lets the floating
 * copy carry the scene reset too, since the panel (and its own reset) is gone.
 */
function ZoomControl({ zoom, onChange, readoutTestId, action }: {
  zoom: number;
  onChange: (z: number) => void;
  readoutTestId: string;
  action?: ReactNode;
}) {
  return (
    <div className="space-y-1 text-sm">
      <div className="flex items-center gap-2">
        <span className="inline-flex items-center gap-2 font-medium">
          <Mountain className="h-4 w-4" aria-hidden="true" />
          Zoom
          <span className="text-xs font-normal text-muted-foreground" data-testid={readoutTestId}>{zoom.toFixed(1)}×</span>
        </span>
        {action && <div className="ml-auto">{action}</div>}
      </div>
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => onChange(zoom - 0.1)}
          aria-label="Zoom out"
          className="rounded-lg border border-input bg-muted p-1.5 text-foreground hover:bg-muted"
        >
          <ZoomOut className="h-4 w-4" aria-hidden="true" />
        </button>
        <input
          type="range"
          aria-label="Zoom"
          min={MIN_ZOOM}
          max="2"
          step="0.1"
          value={zoom}
          onChange={(e) => onChange(Number(e.target.value))}
          className="w-full accent-primary"
        />
        <button
          type="button"
          onClick={() => onChange(zoom + 0.1)}
          aria-label="Zoom in"
          className="rounded-lg border border-input bg-muted p-1.5 text-foreground hover:bg-muted"
        >
          <ZoomIn className="h-4 w-4" aria-hidden="true" />
        </button>
      </div>
      <span className="block text-xs text-muted-foreground">(click and drag to move the view)</span>
    </div>
  );
}

/**
 * "Leave this alone" for one of the world's own clocks. Pressed, the day stops
 * turning / the weather stops cycling and stays exactly where it is.
 */
function LockButton({ locked, label, onClick }: { locked: boolean; label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={locked}
      aria-label={label}
      title={locked ? 'Locked — the world will not change this on its own' : 'Lock this where it is now'}
      className={`inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px] font-semibold ${locked
          ? 'border-primary bg-primary text-primary-foreground'
          : 'border-border bg-muted text-muted-foreground hover:text-foreground'
        }`}
    >
      {locked ? <Lock className="h-3 w-3" aria-hidden="true" /> : <LockOpen className="h-3 w-3" aria-hidden="true" />}
      {locked ? 'Locked' : 'Lock'}
    </button>
  );
}

export function LivingBackground() {
  const {
    soundOn,
    setSoundOn,
    soundVolume,
    setSoundVolume,
    musicVolume,
    setMusicVolume,
    musicOn,
    setMusicOn,
    clearBackground,
    setClearBackground,
    setTod,
    setWeather,
    setTodLocked,
    setWeatherLocked,
    readClock,
    readView,
    setZoom,
    setRainbow,
    isRainbowPinned,
    setTrack,
    setInstrument,
    currentTrackId,
    setMusicAuto,
    shuffleMusic,
    setBuddy,
    moveCursorBy,
    panBy,
    clickAtCursor,
    dropCarried,
    setCursorVisible,
    ready,
  } = useAmbience();
  const { connected: padConnected } = useController();

  const [time, setTime] = useState(0.3);
  const [zoom, setZoomValue] = useState(DEFAULT_ZOOM);
  const [buddy, setBuddyValue] = useState(true);
  const [weather, setWeatherValue] = useState<'clear' | 'rain' | 'snow'>('clear');
  const [todLocked, setTodLockedValue] = useState(false);
  const [weatherLocked, setWeatherLockedValue] = useState(false);
  const [rainbow, setRainbowValue] = useState(false);
  const [track, setTrackValue] = useState('');
  const [instrument, setInstrumentValue] = useState('');
  // The piece the engine is playing right now — surfaced as a marker in the
  // piece dropdown while "Let it choose" is selected. Polled because playback
  // changes never pass through this component's state.
  const [nowPlaying, setNowPlaying] = useState<string | null>(null);
  // The panel covers a good part of the world it is configuring, so it folds
  // away — but the reopen control stays put, never hunted for.
  const [panelOpen, setPanelOpen] = useState(true);

  // Announce the page to the engine (companion on) and mirror state the engine
  // owns: a rainbow pinned from an earlier visit to this page is still up in
  // the world, and a checkbox that reads unchecked while an arc sits in the sky
  // makes the rainbow impossible to turn off (it also read as "stuck on").
  useEffect(() => {
    setBuddy(buddy);
    setRainbowValue(isRainbowPinned());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const sync = () => setNowPlaying(currentTrackId());
    sync();
    const id = window.setInterval(sync, 500);
    return () => window.clearInterval(id);
  }, [currentTrackId]);

  /**
   * Keep the engine in step with what the piece selector is showing.
   *
   * The selector opens on "Let it choose" every time the panel mounts, but a
   * piece pinned earlier in the session is still pinned in the engine — and
   * re-picking the option React already has selected fires no change event, so
   * the player had no way to say so. Autoplay resumes at the end of the piece
   * that is running; this does not cut it off (see the engine's setMusicAuto).
   */
  useEffect(() => {
    if (track === '') setMusicAuto(true);
  }, [track, setMusicAuto]);

  /**
   * Mirror the world's own clock.
   *
   * The day keeps turning and the weather keeps cycling while this panel is
   * open, so a slider that only ever moved when dragged was wrong within
   * seconds of being set — it showed the last thing you asked for, not the time
   * it actually is out there. Read the engine back instead. `lastTouch` yields
   * to a drag in progress so the poll cannot fight your thumb.
   */
  const lastTouch = useRef(0);
  const readClockRef = useRef(readClock);
  readClockRef.current = readClock;
  useEffect(() => {
    const id = window.setInterval(() => {
      const c = readClockRef.current();
      if (!c) return;
      setWeatherValue(c.weather);
      setWeatherLockedValue(c.weatherLocked);
      setTodLockedValue(c.todLocked);
      if (Date.now() - lastTouch.current > 1000) setTime(Math.round(c.tod * 100) / 100);
    }, 500);
    return () => window.clearInterval(id);
  }, []);

  const changeTime = (t: number) => {
    lastTouch.current = Date.now();
    setTime(t);
    setTod(t);
  };
  const changeWeather = (w: 'clear' | 'rain' | 'snow') => {
    setWeatherValue(w);
    setWeather(w);
    // Asking for a weather by hand pins it — otherwise the cycle would undo the
    // choice within a frame. Reflect that here so the lock reads true.
    setWeatherLockedValue(true);
  };
  const toggleTodLock = () => {
    const next = !todLocked;
    setTodLockedValue(next);
    setTodLocked(next);
  };
  const toggleWeatherLock = () => {
    const next = !weatherLocked;
    setWeatherLockedValue(next);
    setWeatherLocked(next);
  };
  // Mirror the engine's zoom instead of pushing the default: the world (and its
  // zoom) live on the global canvas and survive page navigation, so resetting
  // to DEFAULT_ZOOM here snapped the world back every time the user left this
  // page and came back. Read once when the engine is ready; the slider then
  // drives it as usual.
  const zoomSyncedRef = useRef(false);
  useEffect(() => {
    if (!ready || zoomSyncedRef.current) return;
    const view = readView();
    if (!view) return;
    zoomSyncedRef.current = true;
    setZoomValue(Math.max(MIN_ZOOM, Math.min(2, view.zoom)));
  }, [ready, readView]);

  const changeZoom = (z: number) => {
    const clamped = Math.max(MIN_ZOOM, Math.min(2, Math.round(z * 10) / 10));
    setZoomValue(clamped);
    setZoom(clamped);
  };
  /**
   * Start the scene over by reloading the app window: the world is rebuilt from
   * scratch and the view, the clocks and the rainbow all go back to how they
   * open. Saved settings (sound, music, the hidden background) are kept, and
   * the app comes back on this page.
   */
  const resetScene = () => reloadPage();
  /**
   * Controller play. With the controls folded away, a pad drives the world
   * itself: the left stick (or D-pad) moves a cursor, the right stick looks
   * around a zoomed view, and the face buttons poke and put down. With the
   * panel open the pad goes back to moving between its controls as usual.
   */
  const worldPlay = !panelOpen && padConnected;
  const pageRef = useRef<HTMLDivElement>(null);
  const zoomRef = useRef(zoom);
  zoomRef.current = zoom;
  useControllerLayer({
    active: worldPlay,
    rootRef: pageRef,
    // Only the bindings below are taken; the menu button still reaches the app.
    transparent: true,
    onIntent: (intent: ControllerIntent) => {
      switch (intent.type) {
        case 'accept': clickAtCursor(); return true;
        case 'secondary': dropCarried(); return true;
        // Back puts down what is being carried first, then brings the controls back.
        case 'cancel': if (!dropCarried()) setPanelOpen(true); return true;
        case 'page': changeZoom(zoomRef.current + (intent.direction === 'next' ? 0.1 : -0.1)); return true;
        // The sticks are read every frame below, so their coarse repeats are spent here.
        case 'navigate': case 'scroll': return true;
        default: return false;
      }
    },
  });
  useEffect(() => {
    if (!worldPlay) return undefined;
    setCursorVisible(true);
    let frame = 0;
    let last = performance.now();
    let zoomHold = 0.14;
    const tick = (now: number) => {
      const dt = Math.min(0.05, (now - last) / 1000);
      last = now;
      const pad = readGamepadAnalog();
      if (pad) {
        const lx = shapeAxis(pad.lx), ly = shapeAxis(pad.ly);
        if (lx || ly) moveCursorBy(lx * CURSOR_SPEED * dt, ly * CURSOR_SPEED * dt);
        const rx = shapeAxis(pad.rx), ry = shapeAxis(pad.ry);
        // pushing the stick right looks right, so the world slides left
        if (rx || ry) panBy(-rx * PAN_SPEED * dt, -ry * PAN_SPEED * dt);
        // triggers zoom while held, a step at a time
        if (Math.abs(pad.triggers) > 0.5) {
          zoomHold += dt;
          if (zoomHold >= 0.14) { zoomHold = 0; changeZoom(zoomRef.current + Math.sign(pad.triggers) * 0.1); }
        } else zoomHold = 0.14;
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => {
      cancelAnimationFrame(frame);
      setCursorVisible(false);
    };
    // changeZoom reads the zoom through a ref, so it is safe to leave out.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [worldPlay, moveCursorBy, panBy, setCursorVisible]);

  const toggleBuddy = () => {
    const next = !buddy;
    setBuddyValue(next);
    setBuddy(next);
  };

  return (
    <div ref={pageRef} className="space-y-6" data-testid="living-background-page">
      {/* Collapsed, the page gets out of the way entirely — no title, no
          description, no panel chrome — because the whole point of this screen is
          to look at the world. Only the reopen button stays. */}
      {panelOpen ? (
        <PageHero
          icon={Mountain}
          title="Living Background"
          subtitle="The full world, unobstructed. Poke the hills, wait for night, change the weather — everything here is decorative and never touches your game."
        />
      ) : (
        <button
          type="button"
          onClick={() => setPanelOpen(true)}
          aria-expanded={false}
          aria-controls="living-bg-panel-body"
          className="inline-flex items-center gap-2 rounded-full border border-primary/30 bg-background/80 px-4 py-2 text-sm font-semibold shadow-lg backdrop-blur-md hover:bg-background"
          data-testid="living-bg-reopen"
        >
          <ChevronRight className="h-4 w-4" aria-hidden="true" />
          Show controls
        </button>
      )}
      {!panelOpen && (
        <div
          className="fixed right-6 top-6 z-40 w-72 max-w-[calc(100vw-3rem)] rounded-2xl border border-border bg-card/85 p-3 shadow-lg backdrop-blur-md"
          aria-label="Living background zoom"
          data-testid="living-bg-zoom-float"
        >
          <ZoomControl
            zoom={zoom}
            onChange={changeZoom}
            readoutTestId="living-bg-zoom-float-readout"
            action={(
              <button
                type="button"
                onClick={resetScene}
                className="inline-flex items-center gap-1.5 rounded-full border border-border bg-muted px-3 py-1 text-xs font-semibold text-foreground hover:bg-muted/70"
                data-testid="living-bg-zoom-float-reset"
              >
                <RotateCcw className="h-3 w-3" aria-hidden="true" />
                Reload Scene
              </button>
            )}
          />
        </div>
      )}
      {worldPlay && (
        <p
          className="w-fit rounded-full border border-primary/30 bg-background/80 px-4 py-1.5 text-xs text-muted-foreground shadow-lg backdrop-blur-md"
          data-testid="living-bg-pad-hint"
        >
          Left stick moves the pointer · A pokes · X puts down · Right stick looks around · Bumpers or triggers zoom · B brings the controls back
        </p>
      )}

      {/* The control panel — the same set as the prototype's top control bar,
          plus extras (zoom, cursor companion). Floating panel over the world
          so it reads as a proper control deck, not a buried settings block. */}
      {panelOpen && (
        <section
          className="rounded-2xl border border-border bg-card/85 p-4 shadow-lg backdrop-blur-md"
          aria-label="Living background controls"
          data-testid="living-bg-controls"
        >
          <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
            <span className="text-xs font-bold uppercase tracking-[0.2em] text-muted-foreground">
              Control panel
            </span>
            <button
              type="button"
              onClick={() => setPanelOpen(false)}
              aria-expanded={panelOpen}
              aria-controls="living-bg-panel-body"
              className="inline-flex items-center gap-2 rounded-lg bg-primary px-4 py-2 text-sm font-semibold text-primary-foreground shadow-md transition-colors hover:bg-primary/90 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2"
              data-testid="living-bg-hide"
            >
              <Maximize2 className="h-4 w-4" aria-hidden="true" />
              Open Full Living Background
            </button>
          </div>
          {padConnected && (
            <p className="mb-3 text-xs text-muted-foreground" data-testid="living-bg-pad-tip">
              Playing with a controller? Open the full view and the left stick becomes a pointer you can poke the world with.
            </p>
          )}
          <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
            <p className="text-xs text-muted-foreground">
              Locks stop the world changing the time of day or the weather on its own; unlocked, both keep cycling.
            </p>
            <button
              type="button"
              onClick={resetScene}
              className="inline-flex items-center gap-1.5 rounded-full border border-border bg-muted px-3 py-1 text-xs font-semibold text-foreground hover:bg-muted/70"
              data-testid="living-bg-reset"
            >
              <RotateCcw className="h-3 w-3" aria-hidden="true" />
              Reload Scene
            </button>
          </div>
          <div id="living-bg-panel-body" className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            <div className="space-y-1 text-sm">
              <div className="flex items-center gap-3">
                <Switch
                  aria-label="Ambient sounds"
                  checked={soundOn}
                  onCheckedChange={setSoundOn}
                />
                <span className="inline-flex items-center gap-2">
                  <Volume2 className="h-4 w-4" aria-hidden="true" />
                  Ambient sounds
                </span>
              </div>
              <div className="flex items-center gap-2">
                <input
                  type="range"
                  aria-label="Ambient sound volume"
                  min="0"
                  max="100"
                  step="5"
                  disabled={!soundOn}
                  value={Math.round(soundVolume * 100)}
                  onChange={(e) => setSoundVolume(Number(e.target.value) / 100)}
                  className="w-full accent-primary disabled:opacity-40"
                />
                <div><span className="w-10 text-right text-xs text-muted-foreground">{Math.round(soundVolume * 100)}%</span></div>
              </div>
            </div>

            <div className="space-y-1 text-sm">
              <span className="flex items-center gap-2 font-medium">
                <Music className="h-4 w-4" aria-hidden="true" />
                Music
                <button
                  type="button"
                  onClick={() => setMusicOn(!musicOn)}
                  aria-pressed={musicOn}
                  aria-label="Toggle music"
                  className={`inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-[11px] font-semibold ${musicOn
                      ? 'border-primary bg-primary text-primary-foreground'
                      : 'border-border bg-muted text-muted-foreground hover:text-foreground'
                    }`}
                >
                  {musicOn ? 'Playing' : 'Muted'}
                </button>
              </span>
              <div className="flex items-center gap-2">
                <input
                  type="range"
                  aria-label="Music volume"
                  min="0"
                  max="100"
                  step="5"
                  disabled={!musicOn}
                  value={Math.round(musicVolume * 100)}
                  onChange={(e) => setMusicVolume(Number(e.target.value) / 100)}
                  className="w-full accent-primary disabled:opacity-40"
                />
                <div><span className="w-10 text-right text-xs text-muted-foreground">{Math.round(musicVolume * 100)}%</span></div>
              </div>
            </div>

            {/* Not a <label>: the lock is a button, and interactive content
              inside a label makes the label click through to the slider. */}
            <div className="space-y-1 text-sm">
              <span className="flex items-center gap-2 font-medium">
                <Sun className="h-4 w-4" aria-hidden="true" />
                Time of day
                <LockButton locked={todLocked} label="Lock time of day" onClick={toggleTodLock} />
              </span>
              <input
                type="range"
                aria-label="Time of day"
                min="0"
                max="1"
                step="0.01"
                value={time}
                onChange={(e) => changeTime(Number(e.target.value))}
                className="w-full accent-primary"
              />
              <span className="block text-xs text-muted-foreground">
                {time < 0.25 ? 'Deep night' : time < 0.45 ? 'Dawn' : time < 0.7 ? 'Day' : 'Dusk'}
                {todLocked ? ' · held' : ''}
              </span>
            </div>

            <div className="space-y-1 text-sm">
              <span className="flex items-center gap-2 font-medium">
                <CloudRain className="h-4 w-4" aria-hidden="true" />
                Weather
                <LockButton locked={weatherLocked} label="Lock weather" onClick={toggleWeatherLock} />
              </span>
              <div className="flex flex-wrap gap-1.5">
                {(['clear', 'rain', 'snow'] as const).map((w) => (
                  <button
                    key={w}
                    type="button"
                    onClick={() => changeWeather(w)}
                    aria-pressed={weather === w}
                    className={`rounded-full border px-3 py-1 text-xs font-semibold capitalize ${weather === w ? 'border-primary bg-primary text-primary-foreground' : 'border-border bg-muted text-foreground hover:bg-muted'}`}
                  >
                    {w === 'clear' ? <CloudRain className="mr-1 inline h-3 w-3" /> : w === 'rain' ? <CloudRain className="mr-1 inline h-3 w-3" /> : <CloudSnow className="mr-1 inline h-3 w-3" />}
                    {w}
                  </button>
                ))}
              </div>
            </div>

            <ZoomControl zoom={zoom} onChange={changeZoom} readoutTestId="living-bg-zoom-readout" />

            <div className="flex items-center gap-3 text-sm">
              <Switch
                aria-label="Cursor companion"
                checked={buddy}
                onCheckedChange={() => toggleBuddy()}
              />
              <span className="inline-flex items-center gap-2">
                <Sparkles className="h-4 w-4" aria-hidden="true" />
                Cursor companion
              </span>
            </div>

            <div>
              <div className="flex items-start justify-between gap-3 text-sm">
                <span>
                  <span className="inline-flex items-center gap-2 font-medium">
                    <Rainbow className="h-4 w-4" aria-hidden="true" />
                    Rainbow
                  </span>
                  <span className="block text-xs text-muted-foreground">Keeps one up. A rainstorm's own fades after a few seconds.</span>
                </span>
                <Switch
                  aria-label="Rainbow"
                  checked={rainbow}
                  onCheckedChange={(checked) => { setRainbowValue(checked); setRainbow(checked); }}
                />
              </div>
              <br></br>
              <div className="flex items-start justify-between gap-3 text-sm">
                <span>
                  <span className="block font-medium">Hide the standard background</span>
                  <span className="block text-xs text-muted-foreground">Page background to 0% opacity behind the world.</span>
                </span>
                <Switch
                  aria-label="Hide the standard background"
                  checked={clearBackground}
                  onCheckedChange={setClearBackground}
                />
              </div>
            </div>

            <label className="space-y-1 text-sm">
              <span className="font-medium">Music piece</span>
              <span className="block text-xs text-muted-foreground">
                Let it choose shuffles the whole library and moves on when a piece ends. Naming a piece keeps that one playing.
              </span>
              <select
                aria-label="Music piece"
                value={track}
                onChange={(e) => {
                  setTrackValue(e.target.value);
                  // "Let it choose" IS autoplay -- there is no separate switch.
                  if (e.target.value) setTrack(e.target.value);
                  else shuffleMusic();
                }}
                className="block w-full rounded-lg border border-input bg-background px-3 py-2 text-sm text-foreground"
              >
                <option value="">Let it choose</option>
                {MUSIC_TRACK_CHOICES.map((t) => {
                  const playing = track === '' && nowPlaying === t.id;
                  return (
                    <option
                      key={t.id}
                      value={t.id}
                      className={playing ? 'bg-primary/25 font-semibold' : undefined}
                    >
                      {playing ? '▶ ' : ''}{t.name} · {t.mood}{playing ? ' — now playing' : ''}
                    </option>
                  );
                })}
              </select>
            </label>

            <label className="space-y-1 text-sm">
              <span className="font-medium">Instrument</span>
              <select
                aria-label="Instrument"
                value={instrument}
                onChange={(e) => { setInstrumentValue(e.target.value); if (e.target.value) setInstrument(e.target.value); }}
                className="block w-full rounded-lg border border-input bg-background px-3 py-2 text-sm text-foreground"
              >
                <option value="">Suits the piece</option>
                {INSTRUMENT_CHOICES.map((i) => (
                  <option key={i.id} value={i.id}>{i.name}</option>
                ))}
              </select>
            </label>


          </div>
        </section>
      )}
    </div>
  );
}
