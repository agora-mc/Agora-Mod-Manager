/**
 * One open instance's state, shared by the header (name, Launch) and its pages: the instance, its
 * launch checks, and a launch in flight. The header shows on every page, so the state lives here
 * rather than in the Overview.
 *
 * The last answer for each instance is kept for the rest of the session, so reopening an instance
 * shows its checks at once and refreshes them in the background instead of starting blank.
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  checkGameInstance,
  getGameInstance,
  gamesErrorCode,
  gamesErrorMessage,
  launchGameInstance,
  onInstanceExited,
  setGameInstanceDeployment,
  type DeploymentChoice,
  type DeploymentMode,
  type InstanceCheck,
  type InstanceDetail,
  type LaunchReply,
} from './api';

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
  reply: LaunchReply | null;
  actionError: string | null;
  launchAnyway: boolean;
  setLaunchAnyway: (value: boolean) => void;
  /** Read the instance and run its checks again, keeping the last answer on screen meanwhile. */
  reload: () => void;
  changeDeployment: (choice: DeploymentChoice) => Promise<void>;
  launch: (deployment?: DeploymentMode) => Promise<void>;
  answerFallback: (next: DeploymentMode, always: boolean) => Promise<void>;
  dismissReply: () => void;
}

/** How often the page asks whether the game is still running, while it is running or starting. */
const RUNNING_POLL_MS = 2000;

const lastSeen = new Map<string, InstanceSnapshot>();

/** Forget every remembered answer. Tests use it; nothing else needs to. */
export function clearInstanceSessionCache(): void {
  lastSeen.clear();
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
  const [busy, setBusy] = useState<'deployment' | 'launch' | null>(null);
  const [launchStartedAt, setLaunchStartedAt] = useState<number | null>(null);
  const [reply, setReply] = useState<LaunchReply | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
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
    setReply(null);
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
    setBusy('deployment');
    setActionError(null);
    try {
      await setGameInstanceDeployment(instanceId, choice);
    } catch (failure) {
      setActionError(`The way this instance runs was not changed. ${gamesErrorMessage(failure)}`);
      return;
    } finally {
      setBusy(null);
    }
    // The change is saved at this point; a failed refresh must not read as a failed change.
    refreshDetail();
  }, [instanceId, refreshDetail]);

  const launch = useCallback(async (deployment?: DeploymentMode) => {
    setBusy('launch');
    setLaunchStartedAt(Date.now());
    setReply(null);
    setActionError(null);
    try {
      const answer = await launchGameInstance(instanceId, { launchAnyway, deployment });
      setReply(answer);
      if (answer.status === 'launched') refreshDetail();
    } catch (failure) {
      setActionError(
        gamesErrorCode(failure) === 'instance_running'
          ? 'The game is already running from this instance.'
          : `The game did not start. ${gamesErrorMessage(failure)}`,
      );
      refreshDetail();
    } finally {
      setBusy(null);
      setLaunchStartedAt(null);
    }
  }, [instanceId, launchAnyway, refreshDetail]);

  // The question the virtual file system raises: run once from the next rung, always from it, or
  // stop. Nothing runs until the user picks one.
  const answerFallback = useCallback(async (next: DeploymentMode, always: boolean) => {
    setReply(null);
    if (always) {
      setBusy('deployment');
      try {
        await setGameInstanceDeployment(instanceId, next);
      } catch (failure) {
        setActionError(`The way this instance runs was not changed. ${gamesErrorMessage(failure)}`);
        return;
      } finally {
        setBusy(null);
      }
      await launch();
      return;
    }
    await launch(next);
  }, [instanceId, launch]);

  const dismissReply = useCallback(() => setReply(null), []);

  const session = useMemo<InstanceSession>(() => ({
    instanceId,
    page,
    busy,
    launchStartedAt,
    reply,
    actionError,
    launchAnyway,
    setLaunchAnyway,
    reload,
    changeDeployment,
    launch,
    answerFallback,
    dismissReply,
  }), [instanceId, page, busy, launchStartedAt, reply, actionError, launchAnyway, reload, changeDeployment, launch, answerFallback, dismissReply]);

  return <InstanceSessionContext.Provider value={session}>{children}</InstanceSessionContext.Provider>;
}
