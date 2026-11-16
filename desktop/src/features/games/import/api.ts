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

/** The stage of a running import. Only `storing` can be measured; the others have no percentage. */
export type ImportPhase = 'storing' | 'overwrite' | 'instance' | 'layers' | 'finishing';

export interface ImportProgress {
  phase: ImportPhase;
  /** The mod being stored now, when there is one. */
  currentMod: string | null;
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

// ---------------------------------------------------------------------------
// Importing several profiles
// ---------------------------------------------------------------------------

export type ImportGrouping = 'merged' | 'separate';

export interface ProfileOverlap {
  a: string;
  b: string;
  shared: number;
  union: number;
  ratio: number;
}

export interface ProfileSuggestion {
  threshold: number;
  groups: { profiles: string[]; pairs: ProfileOverlap[] }[];
  pairs: ProfileOverlap[];
  note: string;
}

export interface MultiProfileSummary {
  profile: string;
  enabled: number;
  files: number;
  bytes: number;
}

export interface MultiPlanDto {
  manager: 'vortex' | 'mo2';
  grouping: ImportGrouping;
  profiles: string[];
  bytes: number;
  freeBytes: number | null;
  fits: boolean;
  suggestion: ProfileSuggestion;
  plans: MultiProfileSummary[];
  warnings: string[];
}

export interface MultiRunDto {
  instances: string[];
  loadouts: string[];
}

export type MultiImportReply =
  | ({ kind: 'plan' } & MultiPlanDto)
  | ({ kind: 'run' } & MultiRunDto);

export interface VortexMultiRequest {
  store: string | null;
  game: string;
  profiles: string[];
  mode: ImportGrouping;
  name: string | null;
  dryRun: boolean;
  install: string | null;
  addFolder: boolean;
  copySaves: boolean;
  allMods: boolean;
}

export interface Mo2MultiRequest {
  iniPath: string;
  profiles: string[];
  mode: ImportGrouping;
  name: string | null;
  dryRun: boolean;
  copySaves: boolean;
  install: string | null;
}

function isMultiReply(reply: unknown): reply is MultiImportReply {
  return isRecord(reply) && (reply.kind === 'plan' || reply.kind === 'run');
}

/** Agora's suggested grouping of a Vortex game's profiles. */
export async function suggestVortexProfiles(
  store: string | null,
  game: string,
): Promise<ProfileSuggestion> {
  return invoke<ProfileSuggestion>('games_import_vortex_suggest', { store, game });
}

/** Agora's suggested grouping of an MO2 setup's profiles. */
export async function suggestMo2Profiles(iniPath: string): Promise<ProfileSuggestion> {
  return invoke<ProfileSuggestion>('games_import_mo2_suggest', { iniPath });
}

export async function importVortexProfiles(request: VortexMultiRequest): Promise<MultiImportReply> {
  const reply: unknown = await invoke<unknown>('games_import_vortex_multi', { request });
  if (!isMultiReply(reply)) {
    throw new Error('games_import_vortex_multi did not return a plan or a report.');
  }
  return reply;
}

export async function importMo2Profiles(request: Mo2MultiRequest): Promise<MultiImportReply> {
  const reply: unknown = await invoke<unknown>('games_import_mo2_multi', { request });
  if (!isMultiReply(reply)) {
    throw new Error('games_import_mo2_multi did not return a plan or a report.');
  }
  return reply;
}

/** Subscribe to the progress of a running import. Call the returned function to unsubscribe. */
export function listenImportProgress(onProgress: (progress: ImportProgress) => void): Promise<UnlistenFn> {
  return listen<ImportProgress>(IMPORT_PROGRESS_EVENT, (event) => onProgress(event.payload));
}

// ---------------------------------------------------------------------------
// Vortex (MASTER_SPEC §26.10)
// ---------------------------------------------------------------------------

export interface VortexScanProfile {
  id: string;
  name: string;
  modsEnabled: number;
  modsTotal: number;
  /** The mods this profile enables, for comparing profiles. */
  enabledModIds?: string[];
  pluginsTxt: boolean;
  loadorderTxt: boolean;
  localSettings: boolean;
  localSaves: boolean;
}

export interface VortexScanGame {
  vortexGame: string;
  game: string | null;
  gameName: string | null;
  supported: boolean;
  reason: string | null;
  discoveredPath: string | null;
  install: string | null;
  needsFolderAdd: boolean;
  detection: string | null;
  staging: string | null;
  stagingError: string | null;
  stagingMarker: boolean;
  mods: number;
  profiles: VortexScanProfile[];
  manifestFound: boolean;
  manifestFiles: number;
  manifestSearched: string[];
}

export interface VortexScanReply {
  store: string;
  games: VortexScanGame[];
  warnings: string[];
}

export interface VortexPlanDto {
  store: string;
  vortexGame: string;
  gameId: string;
  gameName: string;
  profile: string;
  instanceName: string;
  installId: string;
  installLocation: string;
  /** The folder is a real install but Agora has not added it; a real import adds it first. */
  wouldAddFolder: boolean;
  staging: string;
  mods: {
    modId: string;
    name: string;
    version: string | null;
    nexusModId: number | null;
    nexusFileId: number | null;
    folder: string;
    enabled: boolean;
    files: number;
    bytes: number;
    provenance: string;
  }[];
  modsEnabled: number;
  modsDisabled: number;
  /** Disabled mods left out by the default; `--all-mods` brings them. */
  modsLeftOut: string[];
  files: number;
  bytes: number;
  manifest: {
    path: string;
    found: boolean;
    files: number;
    matched: number;
    damaged: string | null;
    searched: string[];
  };
  order: string[];
  disagreements: { file: string; manifestSupplied: string; rulesSupply: string }[];
  plugins: { kept: boolean; listed: number; active: number; notes: string[] };
  userlist: {
    path: string | null;
    rules: { item: string; after: string }[];
    refused: { item: string; after: string; reason: string }[];
    notImported: string[];
  };
  localSettings: boolean;
  inis: string[];
  localSaves: boolean;
  saves: { local: boolean; files: number; bytes: number; note: string };
  problems: Problem[];
  warnings: string[];
}

export interface VortexRunDto {
  instanceId: string | null;
  instanceName: string;
  storedMods: number;
  contentLayers: number;
  disabledLayers: number;
  duplicates: string[];
  pluginsWritten: number | null;
  pluginFindings: string[];
  rulesAdded: number;
  rulesRefused: string[];
  inisCopied: string[];
  savesCopied: number;
  savesSkippedExisting: number;
  savesFolder: string | null;
  nextSteps: string[];
}

export type VortexImportReply =
  | ({ kind: 'plan' } & VortexPlanDto)
  | ({ kind: 'run' } & VortexRunDto);

export interface VortexImportRequest {
  /** The folder that holds `state.v2`; `null` uses `%APPDATA%\Vortex\state.v2`. */
  store: string | null;
  game: string;
  profile: string;
  name: string | null;
  dryRun: boolean;
  install: string | null;
  addFolder: boolean;
  copySaves: boolean;
  allMods: boolean;
}

function isVortexScanGame(value: unknown): value is VortexScanGame {
  return isRecord(value)
    && typeof value.vortexGame === 'string'
    && typeof value.supported === 'boolean'
    && typeof value.mods === 'number'
    && Array.isArray(value.profiles);
}

/** The games and profiles a Vortex store holds. Rejects on a reply that is not a list. */
export async function scanVortex(store?: string | null): Promise<VortexScanReply> {
  const reply: unknown = await invoke<unknown>('games_import_vortex_scan', { store: store ?? null });
  if (
    !isRecord(reply)
    || !Array.isArray(reply.games)
    || !reply.games.every(isVortexScanGame)
    || !isStringArray(reply.warnings)
    || typeof reply.store !== 'string'
  ) {
    throw new Error('games_import_vortex_scan did not return a list of games.');
  }
  return reply as unknown as VortexScanReply;
}

/** Plan (`dryRun`) or import one Vortex profile. Rejects on a reply that is neither a plan nor a report. */
export async function importVortexProfile(request: VortexImportRequest): Promise<VortexImportReply> {
  const reply: unknown = await invoke<unknown>('games_import_vortex', { request });
  if (!isRecord(reply) || (reply.kind !== 'plan' && reply.kind !== 'run')) {
    throw new Error('games_import_vortex did not return a plan or a report.');
  }
  return reply as unknown as VortexImportReply;
}
