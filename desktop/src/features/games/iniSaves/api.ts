/**
 * IPC wrappers for the INI and saves slice (the game's configuration files and save folder,
 * per instance, MASTER_SPEC §26.7).
 *
 * Every reply is checked against its shape before a caller sees it. A reply that is not the
 * documented shape rejects, so a caller never reads a missing answer as "no keys" or "no saves".
 */

import { invoke } from '@tauri-apps/api/core';
import { isGamesError } from '../shared/types';

/** Where the text of an INI file came from: the instance's copy, the game's own file, or nothing. */
export type IniSourceName = 'copy' | 'game_file' | 'nothing' | 'override' | 'mod' | 'writable';

export interface IniFileRow {
  /** The file's name inside the instance, as the game names it (for example `Skyrim.ini`). */
  instancePath: string;
  /** Whether the instance has its own copy. A copy is made on the first save of a key. */
  copyExists: boolean;
  gameFile: string;
  gameFileExists: boolean;
}

export interface IniEntryRow {
  section: string;
  key: string;
  value: string;
}

export interface IniContents {
  instancePath: string;
  source: IniSourceName;
  entries: IniEntryRow[];
}

export interface IniView {
  files: IniFileRow[];
  /** Set when a file was named. Reading never creates the instance's copy. */
  contents: IniContents | null;
}

export interface IniChange {
  /** Whether the file actually changed. False when the key already had that value. */
  changed: boolean;
  view: IniView;
}

export interface ModIniRow {
  /** The path inside the game folder, with forward slashes. */
  path: string;
  /** The content item's id. Empty for an override that no mod ships any more. */
  modId: string;
  /** The mod's name. Empty for an override that no mod ships any more. */
  modName: string;
  /** Whether the instance has its own copy in `overrides/`. */
  overridden: boolean;
  /** A later enabled mod that ships the same path, when one explains the file. */
  shadowedBy: string | null;
  /** The game wrote this file since the override or mod file, so the game's copy runs. */
  gameChanged: boolean;
  /** False when the mod is turned off, or no mod ships the file any more. */
  modEnabled: boolean;
}


export type SaveChoice = 'own' | 'shared';

export interface SaveFolderRow {
  path: string;
  /** False when the folder has not been made yet. The game makes it. */
  exists: boolean;
  saves: number;
  newest: string | null;
}

export interface SavesView {
  instanceId: string;
  choice: SaveChoice;
  /** The folder the game reads and writes for this instance. */
  inUse: SaveFolderRow;
  /** The other folder. Its saves stay where they are when the choice changes. */
  other: SaveFolderRow;
  settingFile: string;
  /** `section/key` of the game setting that points at a folder. */
  setting: string;
  settingValue: string | null;
  settingSource: IniSourceName;
}

export interface SettingChangeRow {
  action: 'set' | 'unchanged' | 'restored' | 'removed' | 'left_alone' | 'nothing_recorded';
  value: string | null;
}

export interface SavesChange {
  previous: SaveChoice;
  setting: SettingChangeRow;
  saves: SavesView;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isNullableString(value: unknown): value is string | null {
  return value === null || typeof value === 'string';
}

function isIniSource(value: unknown): value is IniSourceName {
  return value === 'copy' || value === 'game_file' || value === 'nothing'
    || value === 'override' || value === 'mod' || value === 'writable';
}

function isSaveChoice(value: unknown): value is SaveChoice {
  return value === 'own' || value === 'shared';
}

function isModIniRow(value: unknown): value is ModIniRow {
  return isObject(value)
    && typeof value.path === 'string'
    && typeof value.modId === 'string'
    && typeof value.modName === 'string'
    && typeof value.overridden === 'boolean'
    && isNullableString(value.shadowedBy)
    && typeof value.gameChanged === 'boolean'
    && typeof value.modEnabled === 'boolean';
}


function isIniFileRow(value: unknown): value is IniFileRow {
  return isObject(value)
    && typeof value.instancePath === 'string'
    && typeof value.copyExists === 'boolean'
    && typeof value.gameFile === 'string'
    && typeof value.gameFileExists === 'boolean';
}

function isIniEntryRow(value: unknown): value is IniEntryRow {
  return isObject(value)
    && typeof value.section === 'string'
    && typeof value.key === 'string'
    && typeof value.value === 'string';
}

function isIniContents(value: unknown): value is IniContents {
  return isObject(value)
    && typeof value.instancePath === 'string'
    && isIniSource(value.source)
    && Array.isArray(value.entries)
    && value.entries.every(isIniEntryRow);
}

function isIniView(value: unknown): value is IniView {
  return isObject(value)
    && Array.isArray(value.files)
    && value.files.every(isIniFileRow)
    && (value.contents === null || isIniContents(value.contents));
}

function isIniChange(value: unknown): value is IniChange {
  return isObject(value) && typeof value.changed === 'boolean' && isIniView(value.view);
}

function isSaveFolderRow(value: unknown): value is SaveFolderRow {
  return isObject(value)
    && typeof value.path === 'string'
    && typeof value.exists === 'boolean'
    && typeof value.saves === 'number'
    && isNullableString(value.newest);
}

function isSavesView(value: unknown): value is SavesView {
  return isObject(value)
    && typeof value.instanceId === 'string'
    && isSaveChoice(value.choice)
    && isSaveFolderRow(value.inUse)
    && isSaveFolderRow(value.other)
    && typeof value.settingFile === 'string'
    && typeof value.setting === 'string'
    && isNullableString(value.settingValue)
    && isIniSource(value.settingSource);
}

function isSavesChange(value: unknown): value is SavesChange {
  return isObject(value)
    && isSaveChoice(value.previous)
    && isObject(value.setting)
    && typeof value.setting.action === 'string'
    && isNullableString(value.setting.value)
    && isSavesView(value.saves);
}

/** Returns the reply when it has the documented shape, and rejects by command name when it does not. */
function checkedReply<T>(command: string, reply: unknown, ok: (value: unknown) => value is T): T {
  if (!ok(reply)) {
    throw new Error(`${command} did not return the expected INI and saves data.`);
  }
  return reply;
}

/** The words to show for a failed command: the backend's own message when it sent one. */
export function describeFailure(error: unknown): string {
  if (isGamesError(error)) return error.message;
  return 'Agora could not finish that. Try again.';
}

/**
 * The instance's INI files. Pass `file` to read that file's keys as well. Reading never makes the
 * instance's copy.
 */
export async function getIni(instanceId: string, file: string | null = null): Promise<IniView> {
  const reply = await invoke<unknown>('games_ini_get', { instanceId, file });
  return checkedReply('games_ini_get', reply, isIniView);
}

/**
 * Set a key in one of the instance's INI files. A `null` value removes the key, so the game uses
 * its own default for it.
 */
export async function setIniValue(
  instanceId: string,
  file: string,
  section: string,
  key: string,
  value: string | null,
): Promise<IniChange> {
  const reply = await invoke<unknown>('games_ini_set', { instanceId, file, section, key, value });
  return checkedReply('games_ini_set', reply, isIniChange);
}

/** The instance's save choice, its two folders, and the game's setting that points at one. */
export async function getSaves(instanceId: string): Promise<SavesView> {
  const reply = await invoke<unknown>('games_saves_get', { instanceId });
  return checkedReply('games_saves_get', reply, isSavesView);
}

/** Make the instance's save choice `own` or `shared`. Never moves, copies or deletes a save. */
export async function setSaves(instanceId: string, choice: SaveChoice): Promise<SavesChange> {
  const reply = await invoke<unknown>('games_saves_set', { instanceId, choice });
  return checkedReply('games_saves_set', reply, isSavesChange);
}

/** The INI files the instance's enabled mods ship, with the instance's own overrides listed too. */
export async function listModInis(instanceId: string): Promise<ModIniRow[]> {
  const reply = await invoke<unknown>('games_mod_inis_list', { instanceId });
  if (!Array.isArray(reply) || !reply.every(isModIniRow)) {
    throw new Error('games_mod_inis_list did not return the expected mod INI data.');
  }
  return reply;
}

/** One mod INI file's keys: the instance's copy when it has one, otherwise the mod's file. */
export async function getModIni(instanceId: string, path: string): Promise<IniContents> {
  const reply = await invoke<unknown>('games_mod_ini_get', { instanceId, path });
  return checkedReply('games_mod_ini_get', reply, isIniContents);
}

/**
 * Set a key in the instance's copy of a mod INI file. A `null` value removes the key. The first
 * edit makes the copy from the mod's file; the stored mod is never changed.
 */
export async function setModIniValue(
  instanceId: string,
  path: string,
  section: string,
  key: string,
  value: string | null,
): Promise<IniContents> {
  const reply = await invoke<unknown>('games_mod_ini_set', { instanceId, path, section, key, value });
  return checkedReply('games_mod_ini_set', reply, isIniContents);
}

/** Adopt the game's version: copy the game's writable copy into the override. */
export async function keepModIniGameVersion(instanceId: string, path: string): Promise<ModIniRow[]> {
  const reply = await invoke<unknown>('games_mod_ini_keep_game_version', { instanceId, path });
  if (!Array.isArray(reply) || !reply.every(isModIniRow)) {
    throw new Error('games_mod_ini_keep_game_version did not return the expected mod INI data.');
  }
  return reply;
}

/** Go back to the instance's version: move the game's writable copy aside. */
export async function restoreModIniMine(instanceId: string, path: string): Promise<ModIniRow[]> {
  const reply = await invoke<unknown>('games_mod_ini_restore_mine', { instanceId, path });
  if (!Array.isArray(reply) || !reply.every(isModIniRow)) {
    throw new Error('games_mod_ini_restore_mine did not return the expected mod INI data.');
  }
  return reply;
}

/** Remove the instance's copy of a mod INI file, so the mod's own file is used again. */
export async function resetModIni(instanceId: string, path: string): Promise<ModIniRow[]> {
  const reply = await invoke<unknown>('games_mod_ini_reset', { instanceId, path });
  if (!Array.isArray(reply) || !reply.every(isModIniRow)) {
    throw new Error('games_mod_ini_reset did not return the expected mod INI data.');
  }
  return reply;
}
