/**
 * IPC wrappers for the import slice (Mod Organizer 2 setups). Mirrors
 * `desktop/src-tauri/src/games/import.rs`. Every reply is checked for the shape the screen
 * reads: a reply that is not that shape rejects, so it is never shown as "nothing found".
 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { InstallSummary } from '../shared/types';

export interface SetupFound {
  iniPath: string;
  gameName: string | null;
  profiles: string[];
  /** `null` when the setup's mods folder cannot be read. */
  modCount: number | null;
  foundBy: string;
}

export interface Mo2ScanReply {
  setups: SetupFound[];
  warnings: string[];
}

export interface PlannedMod {
  line: number;
  folder: string;
  enabled: boolean;
  files: number;
  bytes: number;
  /** What the mod's meta.ini claims, in words. A claim, never a checked fact. */
  provenance: string;
  pluginFiles: string[];
  metaIniExcluded: boolean;
}

export interface Problem {
  line: number | null;
  entry: string;
  reason: string;
}

export interface Mo2PlanDto {
  setup: string;
  profile: string;
  mo2GameName: string;
  gameId: string;
  installId: string;
  installLocation: string;
  /** The game folder the setup itself names; differs from `installLocation` when another install was chosen. */
  setupGamePath: string;
  store: string;
  instanceName: string;
  mods: PlannedMod[];
  modsEnabled: number;
  modsDisabled: number;
  files: number;
  bytes: number;
  metaIniExcluded: number;
  separators: number;
  unmanaged: string[];
  problems: Problem[];
  overwrite: {
    present: boolean;
    files: number;
    bytes: number;
    generated: { name: string; files: number }[];
    restFiles: number;
  };
  plugins: {
    kept: boolean;
    listed: number;
    active: number;
    locked: string[];
    implicitSkipped: string[];
    notes: string[];
  };
  localSettings: boolean;
  inis: string[];
  saves: { local: boolean; files: number; bytes: number; note: string };
  warnings: string[];
}

export interface Mo2RunDto {
  instanceId: string | null;
  instanceName: string;
  baseId: string | null;
  baseBuilt: boolean | null;
  storedMods: number;
  contentLayers: number;
  disabledLayers: number;
  duplicates: string[];
  generatedLayers: string[];
  pluginsWritten: number | null;
  pluginFindings: string[];
  inisCopied: string[];
  savesCopied: number;
  savesSkippedExisting: number;
  savesFolder: string | null;
  nextSteps: string[];
}

/**
 * A preview whose setup names a game folder Agora did not find, while the game is installed
 * elsewhere: the installs the setup could be imported onto instead.
 */
export interface ChooseInstallDto {
  gameId: string;
  gameName: string;
  setupGamePath: string;
  installs: InstallSummary[];
}

/**
 * The import reply: `kind` says whether this was a dry run's plan, a finished import, or a dry run
 * that needs the user to choose an install first.
 */
export type Mo2ImportReply =
  | ({ kind: 'plan' } & Mo2PlanDto)
  | ({ kind: 'run' } & Mo2RunDto)
  | ({ kind: 'choose_install' } & ChooseInstallDto);

/** The error code a preview or import fails with when the game has no install Agora found. */
export const NO_INSTALL_CODE = 'no_install';

export interface ImportProgress {
  modsDone: number;
  modsTotal: number;
  bytesDone: number;
  bytesTotal: number;
}

export const IMPORT_PROGRESS_EVENT = 'games-import-progress';

export interface Mo2ImportRequest {
  setupPath: string;
  profile: string;
  name: string | null;
  copySaves: boolean;
  dryRun: boolean;
  /** The install to import onto, by id; null uses the one at the setup's own game folder. */
  install: string | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === 'string');
}

function isSetupFound(value: unknown): value is SetupFound {
  return isRecord(value)
    && typeof value.iniPath === 'string'
    && isStringArray(value.profiles)
    && typeof value.foundBy === 'string';
}

/** Setups on this machine, or the one folder the user chose (`chosen`). Rejects on a bad reply. */
export async function scanMo2Setups(chosen?: string): Promise<Mo2ScanReply> {
  const reply: unknown = await invoke<unknown>('games_import_mo2_scan', { chosen: chosen ?? null });
  if (
    !isRecord(reply)
    || !Array.isArray(reply.setups)
    || !reply.setups.every(isSetupFound)
    || !isStringArray(reply.warnings)
  ) {
    throw new Error('games_import_mo2_scan did not return a list of setups.');
  }
  return reply as unknown as Mo2ScanReply;
}

/** Plan (`dryRun`) or import one profile. Rejects on a reply that is neither a plan nor a report. */
export async function importMo2Profile(request: Mo2ImportRequest): Promise<Mo2ImportReply> {
  const reply: unknown = await invoke<unknown>('games_import_mo2', { request });
  if (!isRecord(reply)) {
    throw new Error('games_import_mo2 did not return a plan or a report.');
  }
  if (reply.kind === 'choose_install') {
    if (typeof reply.setupGamePath !== 'string' || !Array.isArray(reply.installs)) {
      throw new Error('games_import_mo2 did not list the installs to choose from.');
    }
  } else if (reply.kind !== 'plan' && reply.kind !== 'run') {
    throw new Error('games_import_mo2 did not return a plan or a report.');
  }
  return reply as unknown as Mo2ImportReply;
}

/** Subscribe to the progress of a running import. Call the returned function to unsubscribe. */
export function listenImportProgress(onProgress: (progress: ImportProgress) => void): Promise<UnlistenFn> {
  return listen<ImportProgress>(IMPORT_PROGRESS_EVENT, (event) => onProgress(event.payload));
}
