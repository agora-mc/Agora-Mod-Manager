/**
 * One open instance's state, shared by the header (name, Launch) and its pages: the instance, its
 * launch checks, and a launch in flight. The header shows on every page, so the state lives here
 * rather than in the Overview.
 *
 * The last answer for each instance is kept for the rest of the session, so reopening an instance
 * shows its checks at once and refreshes them in the background instead of starting blank.
 *
 * The launch itself (request, answer, the virtual file system question) lives in
 * `launchController.ts`, so a launch started from an instance card shows here too.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  checkGameInstance,
  getGameInstance,
  gamesErrorMessage,
  onInstanceExited,
  setGameInstanceDeployment,
  type DeploymentChoice,
  type DeploymentMode,
  type InstanceCheck,
  type InstanceDetail,
  type LaunchReply,
} from './api';
import { clearGameLaunches, dismissGameLaunch, gameLaunchState, startGameLaunch, useGameLaunch } from './launchController';

/** What the page says about the tools a launch needs first. */
export interface BeforeLaunchAnswerOptions {
  beforeLaunch?: Record<string, 'run' | 'launch_anyway'>;
  skipBeforeLaunch?: boolean;
  runningTool?: string;
}

export interface InstanceSnapshot {
  detail: InstanceDetail;
  check: InstanceCheck | null;
  checkError: string | null;
  /** When the checks above answered (ms since the epoch), or null when they have not. */
  checkedAt: number | null;
}

export type InstancePageState =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | ({ status: 'loaded'; refreshing: boolean; refreshError: string | null } & InstanceSnapshot);

export interface InstanceSession {
  instanceId: string;
  page: InstancePageState;
  busy: 'deployment' | 'launch' | null;
  launchStartedAt: number | null;
  /** The before-launch tool being run now, when one is. */
  runningTool: string | null;
  reply: LaunchReply | null;
  actionError: string | null;
  launchAnyway: boolean;
  setLaunchAnyway: (value: boolean) => void;
  /** Read the instance and run its checks again, keeping the last answer on screen meanwhile. */
  reload: () => void;
  /** Read the instance again (name, running, play time) without re-running the checks. */
  refreshInstance: () => void;
  changeDeployment: (choice: DeploymentChoice) => Promise<void>;
  launch: (deployment?: DeploymentMode, before?: BeforeLaunchAnswerOptions) => Promise<void>;
  answerFallback: (next: DeploymentMode, always: boolean) => Promise<void>;
  dismissReply: () => void;
}

/** How often the page asks whether the game is still running, while it is running or starting. */
const RUNNING_POLL_MS = 2000;

const lastSeen = new Map<string, InstanceSnapshot>();

/** Forget every remembered answer and launch. Tests use it; nothing else needs to. */
export function clearInstanceSessionCache(): void {
  lastSeen.clear();
  clearGameLaunches();
}

const InstanceSessionContext = createContext<InstanceSession | null>(null);

/** The open instance's session. Throws outside an `InstanceSessionProvider`. */
export function useInstanceSession(): InstanceSession {
  const session = useContext(InstanceSessionContext);
  if (session === null) throw new Error('useInstanceSession needs an InstanceSessionProvider');
  return session;
}

/** The open instance's session, or null on a page rendered on its own. */
export function useOptionalInstanceSession(): InstanceSession | null {
  return useContext(InstanceSessionContext);
}

export function InstanceSessionProvider({ instanceId, children }: { instanceId: string; children: ReactNode }) {
  const [page, setPage] = useState<InstancePageState>({ status: 'loading' });
  const [changing, setChanging] = useState(false);
  const [ownError, setActionError] = useState<string | null>(null);
  const launchState = useGameLaunch(instanceId);
  const busy: InstanceSession['busy'] = launchState.busy ? 'launch' : changing ? 'deployment' : null;
  const launchStartedAt = launchState.startedAt;
  const reply = launchState.reply;
  const runningTool = launchState.runningTool;
  const actionError = ownError ?? launchState.error;
  const [launchAnyway, setLaunchAnyway] = useState(false);
  // An answer that arrives after the user moved to another instance is remembered, not shown.
  const shownId = useRef(instanceId);
  shownId.current = instanceId;

  // The instance first, so the header fills in while the slower checks run; then the checks.
  const load = useCallback(async () => {
    const detail = await getGameInstance(instanceId);
    if (shownId.current !== instanceId) return;
    setPage((current) =>
      current.status === 'loaded'
        ? { ...current, detail, refreshing: true }
        : { status: 'loaded', detail, check: null, checkError: null, checkedAt: null, refreshing: true, refreshError: null },
    );
    let check: InstanceCheck | null = null;
    let checkError: string | null = null;
    try {
      check = await checkGameInstance(instanceId);
    } catch (failure) {
      checkError = gamesErrorMessage(failure);
    }
    const snapshot: InstanceSnapshot = { detail, check, checkError, checkedAt: Date.now() };
    lastSeen.set(instanceId, snapshot);
    if (shownId.current !== instanceId) return;
    setPage({ status: 'loaded', ...snapshot, refreshing: false, refreshError: null });
  }, [instanceId]);

  // A failed refresh keeps the last answer on screen and says so; with nothing to show, the page
  // says the instance could not be loaded.
  const loadReporting = useCallback(() => {
    load().catch((failure: unknown) => {
      const message = gamesErrorMessage(failure);
      setPage((current) =>
        current.status === 'loaded' ? { ...current, refreshing: false, refreshError: message } : { status: 'failed', message },
      );
    });
  }, [load]);

  // Each instance starts from its own last answer or from nothing, never from another instance's.
  // The "Launch anyway" choice goes too: a tick made for one instance must not carry to another.
  useEffect(() => {
    const cached = lastSeen.get(instanceId);
    setPage(cached ? { status: 'loaded', ...cached, refreshing: true, refreshError: null } : { status: 'loading' });
    // A launch that started is over as far as this page is concerned; a question or a refusal
    // (from a card's launch, say) still waits for an answer here.
    if (gameLaunchState(instanceId).reply?.status === 'launched') dismissGameLaunch(instanceId);
    setActionError(null);
    setLaunchAnyway(false);
    loadReporting();
  }, [instanceId, loadReporting]);

  const reload = useCallback(() => {
    setPage((current) => (current.status === 'loaded' ? { ...current, refreshing: true, refreshError: null } : { status: 'loading' }));
    loadReporting();
  }, [loadReporting]);

  const refreshDetail = useCallback(() => {
    getGameInstance(instanceId)
      .then((detail) => {
        const cached = lastSeen.get(instanceId);
        if (cached) lastSeen.set(instanceId, { ...cached, detail });
        setPage((current) => (current.status === 'loaded' ? { ...current, detail } : current));
      })
      .catch((failure: unknown) => setActionError(gamesErrorMessage(failure)));
  }, [instanceId]);

  const running = page.status === 'loaded' && page.detail.running;

  // The game's exit is reported by an event; the poll covers a page that opened while it ran.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    onInstanceExited((exited) => {
      if (exited.instanceId !== instanceId) return;
      if (exited.restoreError !== null) {
        setActionError(`The game closed, but Agora could not put your files back: ${exited.restoreError}`);
      }
      refreshDetail();
    })
      .then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [instanceId, refreshDetail]);

  useEffect(() => {
    if (!running && busy !== 'launch') return;
    const timer = window.setInterval(refreshDetail, RUNNING_POLL_MS);
    return () => window.clearInterval(timer);
  }, [running, busy, refreshDetail]);

  const changeDeployment = useCallback(async (choice: DeploymentChoice) => {
    setChanging(true);
    setActionError(null);
    try {
      await setGameInstanceDeployment(instanceId, choice);
    } catch (failure) {
      setActionError(`The way this instance runs was not changed. ${gamesErrorMessage(failure)}`);
      return;
    } finally {
      setChanging(false);
    }
    // The change is saved at this point; a failed refresh must not read as a failed change.
    refreshDetail();
  }, [instanceId, refreshDetail]);

  const launch = useCallback(async (deployment?: DeploymentMode, before: BeforeLaunchAnswerOptions = {}) => {
    setActionError(null);
    const answer = await startGameLaunch(instanceId, {
      launchAnyway,
      deployment,
      beforeLaunch: before.beforeLaunch,
      skipBeforeLaunch: before.skipBeforeLaunch,
      runningTool: before.runningTool,
    });
    if (answer === null) return;
    // Launched, or failed outright: either way the header's running state may have changed.
    if (answer.status === 'launched') refreshDetail();
  }, [instanceId, launchAnyway, refreshDetail]);

  // The question the virtual file system raises: run once from the next rung, always from it, or
  // stop. Nothing runs until the user picks one.
  const answerFallback = useCallback(async (next: DeploymentMode, always: boolean) => {
    dismissGameLaunch(instanceId);
    if (always) {
      setChanging(true);
      try {
        await setGameInstanceDeployment(instanceId, next);
      } catch (failure) {
        setActionError(`The way this instance runs was not changed. ${gamesErrorMessage(failure)}`);
        return;
      } finally {
        setChanging(false);
      }
      await launch();
      return;
    }
    await launch(next);
  }, [instanceId, launch]);

  const dismissReply = useCallback(() => dismissGameLaunch(instanceId), [instanceId]);

  const session = useMemo<InstanceSession>(() => ({
    instanceId,
    page,
    busy,
    launchStartedAt,
    runningTool,
    reply,
    actionError,
    launchAnyway,
    setLaunchAnyway,
    reload,
    refreshInstance: refreshDetail,
    changeDeployment,
    launch,
    answerFallback,
    dismissReply,
  }), [instanceId, page, busy, launchStartedAt, runningTool, reply, actionError, launchAnyway, reload, refreshDetail, changeDeployment, launch, answerFallback, dismissReply]);

  return <InstanceSessionContext.Provider value={session}>{children}</InstanceSessionContext.Provider>;
}
