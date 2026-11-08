/**
 * IPC wrappers for the plugins slice (an instance's plugin list, load order, locks and checks).
 *
 * Every reply is checked against its shape before a caller sees it. A reply that is not the
 * documented shape rejects, so a caller never reads a missing answer as "no plugins" or "no
 * findings" (MASTER_SPEC §26.6).
 */

import { invoke } from '@tauri-apps/api/core';
import { isGamesError } from '../shared/types';

export interface PluginRow {
  /** 1-based, counting the always-loaded plugins. The number `move` takes. */
  position: number;
  name: string;
  /** Always loaded by the game. Not in the instance's list, so it cannot be moved or locked. */
  implicit: boolean;
  active: boolean;
  /** Whether the plugin's file is where the game looks for it. */
  present: boolean;
  /** Whether Agora added this line because a content layer deploys it. */
  managed: boolean;
  locked: boolean;
  /** Short flags the game's rules show beside the item, such as `Master` or `Light`. */
  badges: string[];
  /** The items the game's rules say it needs first. */
  masters: string[];
}

export interface PluginFinding {
  message: string;
  /** Whether a launch refuses to start because of this finding. The others are warnings. */
  refusesLaunch: boolean;
  /** The plugins the finding is about. Empty for a limit on the whole list. */
  plugins: string[];
}

export interface MasterlistView {
  /** RFC 3339, as recorded when the copy was fetched. */
  fetchedAt: string;
  /** The contents API URL the copy came from. */
  source: string;
}

export interface LoadOrderListRow {
  id: string;
  name: string;
  /** `none` or `script`. */
  rulesKind: string;
}

export interface LoadOrderListsView {
  lists: LoadOrderListRow[];
}

export interface PluginsView {
  /** False when the game keeps no plugin list. The tab says so instead of showing an empty list. */
  supported: boolean;
  /** The list's id within the game. */
  listId: string;
  /** The list's name as a person reads it. */
  listName: string;
  /** `none` or `script`. */
  rulesKind: string;
  /** This list's intro sentence, when the game declares one. */
  intro: string | null;
  /** The column heading for an item's name, when the game declares one. */
  itemHeading: string | null;
  /** Whether this list declares a LOOT masterlist source. */
  hasLoot: boolean;
  /** Whether the game's own rules judge this list, so it can be sorted. */
  rulesApply: boolean;
  /** False until the instance has a copy of the list, which is made at deploy time. */
  listExists: boolean;
  rows: PluginRow[];
  findings: PluginFinding[];
  /** The LOOT masterlist cached for this game, when one has been downloaded. */
  masterlist: MasterlistView | null;
}

export interface PluginChange {
  /** Whether the line or the lock actually changed. */
  changed: boolean;
  /**
   * The list after the change. `null` only when the change was saved but the list could not be
   * read back; `reloadError` then says why.
   */
  plugins: PluginsView | null;
  /** Why the list could not be read back after a change that was saved. */
  reloadError: string | null;
}

export interface MoveReport {
  plugin: string;
  from: number;
  to: number;
  written: boolean;
}

export interface MoveChange {
  report: MoveReport;
  plugins: PluginsView;
}

export interface SortMove {
  plugin: string;
  from: number;
  to: number;
}

export interface SortBlocked {
  plugin: string;
  master: string;
}

export type SortMode = 'loot' | 'masters';

export interface SortReport {
  moves: SortMove[];
  /** Plugins that load above a master the sort may not move. The sort cannot fix those. */
  blocked: SortBlocked[];
  /** False for a dry run, which writes nothing. */
  written: boolean;
  /** What ordered the free plugins: LOOT's masterlist, or only their masters. */
  mode: SortMode;
  /** A sentence for the player when the full sort could not run. */
  message: string | null;
}

export interface SortChange {
  report: SortReport;
  plugins: PluginsView;
}

export interface CheckView {
  /** `findings` when a launch would refuse, `warnings` when there is something to read, `ok` otherwise. */
  status: 'ok' | 'warnings' | 'findings';
  findings: PluginFinding[];
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === 'string');
}

function isPluginRow(value: unknown): value is PluginRow {
  if (!isObject(value)) return false;
  return typeof value.position === 'number'
    && typeof value.name === 'string'
    && typeof value.implicit === 'boolean'
    && typeof value.active === 'boolean'
    && typeof value.present === 'boolean'
    && typeof value.managed === 'boolean'
    && typeof value.locked === 'boolean'
    && isStringArray(value.badges)
    && isStringArray(value.masters);
}

function isPluginFinding(value: unknown): value is PluginFinding {
  if (!isObject(value)) return false;
  return typeof value.message === 'string'
    && typeof value.refusesLaunch === 'boolean'
    && isStringArray(value.plugins);
}

function isMasterlistView(value: unknown): value is MasterlistView {
  return isObject(value)
    && typeof value.fetchedAt === 'string'
    && typeof value.source === 'string';
}

function isPluginsView(value: unknown): value is PluginsView {
  if (!isObject(value)) return false;
  return typeof value.supported === 'boolean'
    && typeof value.listId === 'string'
    && typeof value.listName === 'string'
    && typeof value.rulesKind === 'string'
    && typeof value.hasLoot === 'boolean'
    && typeof value.rulesApply === 'boolean'
    && typeof value.listExists === 'boolean'
    && Array.isArray(value.rows)
    && value.rows.every(isPluginRow)
    && Array.isArray(value.findings)
    && value.findings.every(isPluginFinding)
    && (value.masterlist === null || isMasterlistView(value.masterlist));
}

function isLoadOrderListsView(value: unknown): value is LoadOrderListsView {
  return isObject(value)
    && Array.isArray(value.lists)
    && value.lists.every((row) =>
      isObject(row)
      && typeof row.id === 'string'
      && typeof row.name === 'string'
      && typeof row.rulesKind === 'string',
    );
}

function isPluginChange(value: unknown): value is PluginChange {
  if (!isObject(value) || typeof value.changed !== 'boolean') return false;
  if (isPluginsView(value.plugins)) return value.reloadError === null || value.reloadError === undefined;
  return value.plugins === null && typeof value.reloadError === 'string' && value.reloadError.length > 0;
}

function isMoveChange(value: unknown): value is MoveChange {
  return isObject(value)
    && isObject(value.report)
    && typeof value.report.plugin === 'string'
    && typeof value.report.from === 'number'
    && typeof value.report.to === 'number'
    && typeof value.report.written === 'boolean'
    && isPluginsView(value.plugins);
}

function isSortChange(value: unknown): value is SortChange {
  return isObject(value)
    && isObject(value.report)
    && Array.isArray(value.report.moves)
    && Array.isArray(value.report.blocked)
    && typeof value.report.written === 'boolean'
    && (value.report.mode === 'loot' || value.report.mode === 'masters')
    && (value.report.message === null || typeof value.report.message === 'string')
    && isPluginsView(value.plugins);
}

function isCheckView(value: unknown): value is CheckView {
  return isObject(value)
    && (value.status === 'ok' || value.status === 'warnings' || value.status === 'findings')
    && Array.isArray(value.findings)
    && value.findings.every(isPluginFinding);
}

/** Returns the reply when it has the documented shape, and rejects by command name when it does not. */
function checkedReply<T>(command: string, reply: unknown, ok: (value: unknown) => value is T): T {
  if (!ok(reply)) {
    throw new Error(`${command} did not return the expected plugin data.`);
  }
  return reply;
}

/** Every load order list this game declares, in the order it declares them. */
export async function listLoadOrderLists(instanceId: string): Promise<LoadOrderListsView> {
  const reply = await invoke<unknown>('games_plugins_lists', { instanceId });
  return checkedReply('games_plugins_lists', reply, isLoadOrderListsView);
}

/** An instance's plugins in load order, with what the game's rules say about them. */
export async function listGamePlugins(instanceId: string, listId?: string): Promise<PluginsView> {
  const reply = await invoke<unknown>('games_plugins_list', { instanceId, listId: listId ?? null });
  return checkedReply('games_plugins_list', reply, isPluginsView);
}

/** Turn a plugin on or off in the instance's list. Refused while the game is running. */
export async function setGamePluginActive(instanceId: string, plugin: string, active: boolean, listId?: string): Promise<PluginChange> {
  const reply = await invoke<unknown>('games_plugins_set_active', { instanceId, plugin, active, listId: listId ?? null });
  return checkedReply('games_plugins_set_active', reply, isPluginChange);
}

/** Lock or unlock a plugin's place, so sort and move leave it where it is. */
export async function setGamePluginLocked(instanceId: string, plugin: string, locked: boolean, listId?: string): Promise<PluginChange> {
  const reply = await invoke<unknown>('games_plugins_set_locked', { instanceId, plugin, locked, listId: listId ?? null });
  return checkedReply('games_plugins_set_locked', reply, isPluginChange);
}

/** Move a plugin to a 1-based position that counts the always-loaded plugins. */
export async function moveGamePlugin(instanceId: string, plugin: string, to: number, listId?: string): Promise<MoveChange> {
  const reply = await invoke<unknown>('games_plugins_move', { instanceId, plugin, to, listId: listId ?? null });
  return checkedReply('games_plugins_move', reply, isMoveChange);
}

/** Sort by the game's master rules. A dry run reports the moves and writes nothing. */
export async function sortGamePlugins(instanceId: string, dryRun: boolean, listId?: string): Promise<SortChange> {
  const reply = await invoke<unknown>('games_plugins_sort', { instanceId, dryRun, listId: listId ?? null });
  return checkedReply('games_plugins_sort', reply, isSortChange);
}

/** Download and verify LOOT's masterlist for the game. */
export async function updateGameMasterlist(instanceId: string, listId?: string): Promise<MasterlistView> {
  const reply = await invoke<unknown>('games_plugins_update_masterlist', { instanceId, listId: listId ?? null });
  return checkedReply('games_plugins_update_masterlist', reply, isMasterlistView);
}

/** The load order's findings, read again. Read only. */
export async function checkGamePlugins(instanceId: string, listId?: string): Promise<CheckView> {
  const reply = await invoke<unknown>('games_plugins_check', { instanceId, listId: listId ?? null });
  return checkedReply('games_plugins_check', reply, isCheckView);
}

/** The words to show for a failed command: the backend's own message when it sent one. */
export function describeFailure(error: unknown): string {
  if (isGamesError(error)) return error.message;
  return 'Agora could not finish that. Try again.';
}
