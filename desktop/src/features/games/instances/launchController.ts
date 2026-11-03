/**
 * Launches of game instances, kept outside any one page so a launch started from an instance card
 * is the same launch the instance page shows: its spinner, its answer, and the question the
 * virtual file system can raise. One state per instance, held for the app session.
 *
 * This is the minimal lift of the launch out of `InstanceSessionProvider`: the request and its
 * answer live here; the session keeps what only the open page needs (launch-anyway, the fallback
 * question's "always", refreshing the header).
 */

import { useCallback, useSyncExternalStore } from 'react';
import {
  gamesErrorCode,
  gamesErrorMessage,
  launchGameInstance,
  type DeploymentMode,
  type LaunchReply,
} from './api';

export interface GameLaunchState {
  /** A launch request is in flight. */
  busy: boolean;
  /** When the request started (ms since the epoch), or null. */
  startedAt: number | null;
  /** The last answer, until it is dismissed or another launch starts. */
  reply: LaunchReply | null;
  /** The last launch failed outright, in a sentence. */
  error: string | null;
}

export interface LaunchOptions {
  /** Start past checks that would stop the launch. Only the instance page offers this. */
  launchAnyway?: boolean;
  /** Run this once with another deployment (the answer to the virtual file system question). */
  deployment?: DeploymentMode;
}

const IDLE: GameLaunchState = Object.freeze({ busy: false, startedAt: null, reply: null, error: null });

const states = new Map<string, GameLaunchState>();
const listeners = new Set<() => void>();

function update(instanceId: string, next: GameLaunchState): void {
  if (next.busy || next.reply !== null || next.error !== null) states.set(instanceId, next);
  else states.delete(instanceId);
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** The launch state of one instance (the same object until it changes). */
export function gameLaunchState(instanceId: string): GameLaunchState {
  return states.get(instanceId) ?? IDLE;
}

/**
 * Launch an instance. Resolves with the answer, or null when the launch failed outright (the
 * state's `error` says why) or another launch of the same instance is already in flight.
 */
export async function startGameLaunch(instanceId: string, options: LaunchOptions = {}): Promise<LaunchReply | null> {
  if (gameLaunchState(instanceId).busy) return null;
  update(instanceId, { busy: true, startedAt: Date.now(), reply: null, error: null });
  try {
    const reply = await launchGameInstance(instanceId, {
      launchAnyway: options.launchAnyway ?? false,
      deployment: options.deployment,
    });
    update(instanceId, { ...IDLE, reply });
    return reply;
  } catch (failure) {
    update(instanceId, {
      ...IDLE,
      error: gamesErrorCode(failure) === 'instance_running'
        ? 'The game is already running from this instance.'
        : `The game did not start. ${gamesErrorMessage(failure)}`,
    });
    return null;
  }
}

/** Forget the last answer (and error) of an instance that is not launching. */
export function dismissGameLaunch(instanceId: string): void {
  const current = gameLaunchState(instanceId);
  if (current.busy || (current.reply === null && current.error === null)) return;
  update(instanceId, IDLE);
}

/** Forget every launch state. Tests use it; nothing else needs to. */
export function clearGameLaunches(): void {
  states.clear();
  for (const listener of listeners) listener();
}

/** One instance's launch, kept current: what a card or a header shows. */
export function useGameLaunch(instanceId: string): GameLaunchState {
  const read = useCallback(() => gameLaunchState(instanceId), [instanceId]);
  return useSyncExternalStore(subscribe, read, read);
}

/** True while any instance is launching, so a list page does not start a second one meanwhile. */
export function useAnyGameLaunching(): boolean {
  const read = useCallback(() => {
    for (const state of states.values()) if (state.busy) return true;
    return false;
  }, []);
  return useSyncExternalStore(subscribe, read, read);
}
