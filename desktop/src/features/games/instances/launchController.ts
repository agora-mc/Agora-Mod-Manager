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
  /** The before-launch tool Agora is running now, when exactly one is running. */
  runningTool: string | null;
  /** How many before-launch tools the launch is running, so the page can avoid naming one. */
  runningTools: number;
}

export interface LaunchOptions {
  /** Start past checks that would stop the launch. Only the instance page offers this. */
  launchAnyway?: boolean;
  /** Run this once with another deployment (the answer to the virtual file system question). */
  deployment?: DeploymentMode;
  /** How to answer each before-launch tool; leave out to be asked. */
  beforeLaunch?: Record<string, 'run' | 'launch_anyway'>;
  /** Launch without running any before-launch tool. */
  skipBeforeLaunch?: boolean;
  /** The tool whose run the progress line names, when exactly one is running. */
  runningTool?: string;
  /** How many before-launch tools are running. */
  runningTools?: number;
}

const IDLE: GameLaunchState = Object.freeze({ busy: false, startedAt: null, reply: null, error: null, runningTool: null, runningTools: 0 });

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
  update(instanceId, {
    busy: true,
    startedAt: Date.now(),
    reply: null,
    error: null,
    runningTool: options.runningTool ?? null,
    runningTools: options.runningTools ?? 0,
  });
  try {
    let reply = await launchGameInstance(instanceId, {
      launchAnyway: options.launchAnyway ?? false,
      deployment: options.deployment,
      beforeLaunch: options.beforeLaunch,
      skipBeforeLaunch: options.skipBeforeLaunch,
    });
    // Run-mode tools run by themselves, visibly. Ask-mode tools stay for the page's dialog.
    if (
      reply.status === 'before_launch_needed'
      && options.beforeLaunch === undefined
      && options.skipBeforeLaunch !== true
    ) {
      const needs = reply.beforeLaunch ?? [];
      const asks = needs.filter((need) => need.mode === 'ask');
      const runs = needs.filter((need) => need.mode === 'run');
      if (asks.length === 0 && runs.length > 0) {
        const answers: Record<string, 'run' | 'launch_anyway'> = {};
        for (const need of runs) answers[need.tool] = 'run';
        update(instanceId, {
          busy: true,
          startedAt: Date.now(),
          reply: null,
          error: null,
          runningTool: runs.length === 1 ? runs[0]?.name ?? null : null,
          runningTools: runs.length,
        });
        reply = await launchGameInstance(instanceId, {
          launchAnyway: options.launchAnyway ?? false,
          deployment: options.deployment,
          beforeLaunch: answers,
        });
      }
    }
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
