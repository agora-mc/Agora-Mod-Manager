/**
 * IPC wrappers for the instances slice (the game instance list, an instance's page, its
 * checks and launch). Every reply is checked against the shape the backend documents, so a missing
 * answer is an error here, never "no instances".
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { isGamesError } from '../shared/types';

export type DeploymentMode = 'virtual' | 'links' | 'copies';
/** `auto` is the game's default, with any step down announced. A reply reads it as `null`. */
export type DeploymentChoice = DeploymentMode | 'auto';
export type BaseMode = 'linked' | 'copied';

/** What the page may offer for this game and instance. */
export interface InstanceCapabilities {
  launch: boolean;
  stop: boolean;
  rename: boolean;
  clone: boolean;
  trash: boolean;
  openFolder: boolean;
  playTime: boolean;
}

export interface InstanceCard {
  instanceId: string;
  gameId: string;
  name: string;
  store: string | null;
  version: string | null;
  pinned: boolean;
  deployment: DeploymentMode | null;
  lastLaunchedAt: string | null;
  lastPlayedAt: string | null;
  playTimeMs: number;
  running: boolean;
  capabilities: InstanceCapabilities;
}

export interface InstanceList {
  instances: InstanceCard[];
}

export interface InstanceDetail {
  instanceId: string;
  gameId: string;
  gameName: string;
  name: string;
  pinned: boolean;
  store: string | null;
  version: string | null;
  build: string | null;
  baseId: string | null;
  installId: string | null;
  unpinnedReason: string | null;
  deployment: DeploymentMode | null;
  saves: 'shared' | 'own';
  createdAt: string;
  lastLaunchedAt: string | null;
  lastPlayedAt: string | null;
  playTimeMs: number;
  running: boolean;
  capabilities: InstanceCapabilities;
}

export type FindingKind = 'framework' | 'load_order' | 'tool' | 'base';

export interface CheckFinding {
  kind: FindingKind;
  /** True when this stops a launch unless the user launches anyway. */
  blocks: boolean;
  title: string;
  detail: string;
  why: string;
  repair: string | null;
}

export interface PassedCheck {
  kind: FindingKind;
  title: string;
  detail: string;
}

export interface InstanceCheck {
  instanceId: string;
  checked: boolean;
  status: 'ok' | 'warnings' | 'findings';
  findings: CheckFinding[];
  /** The checks that ran and passed. Empty means none applied, never that all passed. */
  passed: PassedCheck[];
}

export type LaunchStatus = 'launched' | 'refused' | 'vfs_unavailable';

export interface LaunchReply {
  status: LaunchStatus;
  pid: number | null;
  deployment: DeploymentMode | null;
  notice: string | null;
  findings: CheckFinding[];
  reason: string | null;
  next: DeploymentMode | null;
}

export interface CreatedInstance {
  instanceId: string;
  name: string;
  base: 'pinned' | 'unpinned';
  reusedBase: boolean;
  baseId: string | null;
  unpinnedReason: string | null;
}

export interface DeletedInstance {
  instanceId: string;
  orphanedBase: string | null;
  /** A deployed game folder Agora could not prove was the instance's, left where it was. */
  leftInPlace: string | null;
  /** The Recently deleted entry it can be restored from. */
  trashEntryId: string | null;
}

export interface RenamedInstance {
  instanceId: string;
  name: string;
}

export interface ClonedInstance {
  instanceId: string;
  name: string;
}

export interface StoppedInstance {
  instanceId: string;
  processesEnded: number;
  playTimeMs: number;
}

/** One entry in Recently deleted. */
export interface DeletedEntry {
  entryId: string;
  instanceId: string;
  gameId: string;
  name: string;
  deletedAt: string;
  expiresAt: string;
  sizeBytes: number;
}

export interface DeletedList {
  entries: DeletedEntry[];
  /** The player's retention window, in days. */
  retentionDays: number;
}

export interface RestoredInstance {
  instanceId: string;
  originalInstanceId: string;
  renamed: boolean;
}

export interface PurgedDeleted {
  purged: number;
  failed: number;
  kept: number;
}

export interface InstanceDeployment {
  instanceId: string;
  deployment: DeploymentMode | null;
}

export interface CreateProgress {
  filesDone: number;
  filesTotal: number;
  bytesHashed: number;
  bytesTotal: number;
}

export interface InstanceExited {
  instanceId: string;
  restoreError: string | null;
}

const CREATE_PROGRESS_EVENT = 'games-instance-create-progress';
const INSTANCE_EXITED_EVENT = 'games-instance-exited';

const DEPLOYMENTS: readonly string[] = ['virtual', 'links', 'copies'];
const LAUNCH_STATUSES: readonly string[] = ['launched', 'refused', 'vfs_unavailable'];
const FINDING_KINDS: readonly string[] = ['framework', 'load_order', 'tool', 'base'];
const CHECK_STATUSES: readonly string[] = ['ok', 'warnings', 'findings'];

/** The sentence a page shows for a rejected command. A rejection that is not ours is said plainly. */
export function gamesErrorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong, and Agora did not say what.';
}

/** The code of a rejected command, or null when the rejection is not one of ours. */
export function gamesErrorCode(error: unknown): string | null {
  return isGamesError(error) ? error.code : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isOptionalString(value: unknown): boolean {
  return value === null || value === undefined || typeof value === 'string';
}

function isDeployment(value: unknown): value is DeploymentMode | null {
  return value === null || (typeof value === 'string' && DEPLOYMENTS.includes(value));
}

function isCapabilities(value: unknown): value is InstanceCapabilities {
  if (!isRecord(value)) return false;
  return ['launch', 'stop', 'rename', 'clone', 'trash', 'openFolder', 'playTime']
    .every((key) => typeof value[key] === 'boolean');
}

function isCard(value: unknown): value is InstanceCard {
  if (!isRecord(value)) return false;
  return typeof value.instanceId === 'string'
    && typeof value.gameId === 'string'
    && typeof value.name === 'string'
    && typeof value.pinned === 'boolean'
    && typeof value.running === 'boolean'
    && typeof value.playTimeMs === 'number'
    && isOptionalString(value.store)
    && isOptionalString(value.version)
    && isOptionalString(value.lastLaunchedAt)
    && isOptionalString(value.lastPlayedAt)
    && isDeployment(value.deployment)
    && isCapabilities(value.capabilities);
}

function isDeletedEntry(value: unknown): value is DeletedEntry {
  if (!isRecord(value)) return false;
  return typeof value.entryId === 'string'
    && typeof value.instanceId === 'string'
    && typeof value.gameId === 'string'
    && typeof value.name === 'string'
    && typeof value.deletedAt === 'string'
    && typeof value.expiresAt === 'string'
    && typeof value.sizeBytes === 'number';
}

function isFinding(value: unknown): value is CheckFinding {
  if (!isRecord(value)) return false;
  return typeof value.kind === 'string'
    && FINDING_KINDS.includes(value.kind)
    && typeof value.blocks === 'boolean'
    && typeof value.title === 'string'
    && typeof value.detail === 'string'
    && typeof value.why === 'string'
    && isOptionalString(value.repair);
}

function isPassed(value: unknown): value is PassedCheck {
  if (!isRecord(value)) return false;
  return typeof value.kind === 'string'
    && FINDING_KINDS.includes(value.kind)
    && typeof value.title === 'string'
    && typeof value.detail === 'string';
}

function isLaunchReply(value: unknown): value is LaunchReply {
  if (!isRecord(value)) return false;
  return typeof value.status === 'string'
    && LAUNCH_STATUSES.includes(value.status)
    && Array.isArray(value.findings)
    && value.findings.every(isFinding)
    && (value.pid === null || typeof value.pid === 'number')
    && isDeployment(value.deployment)
    && isOptionalString(value.notice)
    && isOptionalString(value.reason)
    && isDeployment(value.next);
}

function shapeError(command: string): Error {
  return new Error(`${command} did not return what the Instances page expects.`);
}

/** The instances of one game. Rejects if the reply is not a list. */
export async function listGameInstances(gameId: string): Promise<InstanceList> {
  const reply: unknown = await invoke<unknown>('games_instance_list', { gameId });
  if (!isRecord(reply) || !Array.isArray(reply.instances) || !reply.instances.every(isCard)) {
    throw shapeError('games_instance_list');
  }
  return reply as unknown as InstanceList;
}

/** One instance, for its page's header. */
export async function getGameInstance(instanceId: string): Promise<InstanceDetail> {
  const reply: unknown = await invoke<unknown>('games_instance_get', { instanceId });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || typeof reply.name !== 'string'
    || typeof reply.pinned !== 'boolean'
    || typeof reply.running !== 'boolean'
    || typeof reply.playTimeMs !== 'number'
    || !isOptionalString(reply.lastPlayedAt)
    || !isCapabilities(reply.capabilities)
    || (reply.saves !== 'shared' && reply.saves !== 'own')
  ) {
    throw shapeError('games_instance_get');
  }
  return reply as unknown as InstanceDetail;
}

export async function createGameInstance(
  gameId: string,
  installId: string,
  name: string,
  baseMode: BaseMode,
  deployment: DeploymentChoice = 'auto',
): Promise<CreatedInstance> {
  const reply: unknown = await invoke<unknown>('games_instance_create', {
    gameId,
    installId,
    name,
    baseMode,
    deployment,
  });
  if (!isRecord(reply) || typeof reply.instanceId !== 'string' || typeof reply.name !== 'string') {
    throw shapeError('games_instance_create');
  }
  return reply as unknown as CreatedInstance;
}

export async function deleteGameInstance(instanceId: string, deleteAnyway = false): Promise<DeletedInstance> {
  const reply: unknown = await invoke<unknown>('games_instance_delete', { instanceId, deleteAnyway });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || !isOptionalString(reply.orphanedBase)
    || !isOptionalString(reply.leftInPlace)
    || !isOptionalString(reply.trashEntryId)
  ) {
    throw shapeError('games_instance_delete');
  }
  return reply as unknown as DeletedInstance;
}

/** Give an instance a new display name. Its id and folder do not change. */
export async function renameGameInstance(instanceId: string, name: string): Promise<RenamedInstance> {
  const reply: unknown = await invoke<unknown>('games_instance_rename', { instanceId, name });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || typeof reply.name !== 'string'
  ) {
    throw shapeError('games_instance_rename');
  }
  return reply as unknown as RenamedInstance;
}

/** Copy an instance's content, plugin order, INIs and settings. Saves only when asked. */
export async function cloneGameInstance(
  instanceId: string,
  name: string,
  copySaves = false,
): Promise<ClonedInstance> {
  const reply: unknown = await invoke<unknown>('games_instance_clone', { instanceId, name, copySaves });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || typeof reply.name !== 'string'
  ) {
    throw shapeError('games_instance_clone');
  }
  return reply as unknown as ClonedInstance;
}

/** Stop the game Agora started for an instance, then run the normal session end. */
export async function stopGameInstance(instanceId: string): Promise<StoppedInstance> {
  const reply: unknown = await invoke<unknown>('games_instance_stop', { instanceId });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || typeof reply.processesEnded !== 'number'
    || typeof reply.playTimeMs !== 'number'
  ) {
    throw shapeError('games_instance_stop');
  }
  return reply as unknown as StoppedInstance;
}

/** The instances of one game in Recently deleted, newest first. */
export async function listDeletedGameInstances(gameId: string): Promise<DeletedList> {
  const reply: unknown = await invoke<unknown>('games_instance_deleted_list', { gameId });
  if (
    !isRecord(reply)
    || !Array.isArray(reply.entries)
    || !reply.entries.every(isDeletedEntry)
    || typeof reply.retentionDays !== 'number'
  ) {
    throw shapeError('games_instance_deleted_list');
  }
  return reply as unknown as DeletedList;
}

/**
 * Restore a deleted instance. Without a name its original id is used and a taken id is refused
 * (`instance_id_taken`); with a name it comes back under a new id.
 */
export async function restoreDeletedGameInstance(
  entryId: string,
  name?: string,
): Promise<RestoredInstance> {
  const reply: unknown = await invoke<unknown>('games_instance_restore_deleted', {
    entryId,
    name: name ?? null,
  });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || typeof reply.originalInstanceId !== 'string'
    || typeof reply.renamed !== 'boolean'
  ) {
    throw shapeError('games_instance_restore_deleted');
  }
  return reply as unknown as RestoredInstance;
}

/**
 * Send one deleted entry to the OS trash, or every entry of `gameId` when no `entryId` is given.
 * Another game's deleted instances are never touched by "all".
 */
export async function purgeDeletedGameInstances(gameId: string, entryId?: string): Promise<PurgedDeleted> {
  const reply: unknown = await invoke<unknown>('games_instance_purge_deleted', {
    gameId,
    entryId: entryId ?? null,
  });
  if (
    !isRecord(reply)
    || typeof reply.purged !== 'number'
    || typeof reply.failed !== 'number'
    || typeof reply.kept !== 'number'
  ) {
    throw shapeError('games_instance_purge_deleted');
  }
  return reply as unknown as PurgedDeleted;
}

export async function checkGameInstance(instanceId: string): Promise<InstanceCheck> {
  const reply: unknown = await invoke<unknown>('games_instance_check', { instanceId });
  if (
    !isRecord(reply)
    || typeof reply.instanceId !== 'string'
    || typeof reply.checked !== 'boolean'
    || typeof reply.status !== 'string'
    || !CHECK_STATUSES.includes(reply.status)
    || !Array.isArray(reply.findings)
    || !reply.findings.every(isFinding)
    || !Array.isArray(reply.passed)
    || !reply.passed.every(isPassed)
  ) {
    throw shapeError('games_instance_check');
  }
  return reply as unknown as InstanceCheck;
}

export async function setGameInstanceDeployment(instanceId: string, mode: DeploymentChoice): Promise<InstanceDeployment> {
  const reply: unknown = await invoke<unknown>('games_instance_set_deployment', { instanceId, mode });
  if (!isRecord(reply) || typeof reply.instanceId !== 'string' || !isDeployment(reply.deployment)) {
    throw shapeError('games_instance_set_deployment');
  }
  return reply as unknown as InstanceDeployment;
}

/**
 * Launch an instance. `deployment` runs this launch only; leave it out to use the instance's own
 * choice. Rejects when the launch itself failed. A launch that a check or the virtual file system
 * stopped comes back as a reply whose `status` says so.
 */
export async function launchGameInstance(
  instanceId: string,
  options: { launchAnyway: boolean; deployment?: DeploymentMode },
): Promise<LaunchReply> {
  const reply: unknown = await invoke<unknown>('games_instance_launch', {
    instanceId,
    launchAnyway: options.launchAnyway,
    deployment: options.deployment ?? null,
  });
  if (!isLaunchReply(reply)) throw shapeError('games_instance_launch');
  return reply;
}

export function onCreateProgress(handler: (progress: CreateProgress) => void): Promise<UnlistenFn> {
  return listen<CreateProgress>(CREATE_PROGRESS_EVENT, (event) => handler(event.payload));
}

export function onInstanceExited(handler: (exited: InstanceExited) => void): Promise<UnlistenFn> {
  return listen<InstanceExited>(INSTANCE_EXITED_EVENT, (event) => handler(event.payload));
}
