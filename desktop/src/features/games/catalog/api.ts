/**
 * IPC wrappers for the catalog slice (the curated catalog for games other than Minecraft).
 * Mirrors `desktop/src-tauri/src/games/catalog.rs`. A reply that is not the documented shape
 * rejects, so a failure is never shown as an empty catalog.
 */

import { invoke } from '@tauri-apps/api/core';

/** Where a differing hash expectation came from (`HashOrigin` in agora-core). */
export type HashOrigin = 'curator_pin' | 'previous_install';

export interface CatalogRequirement {
  framework: string;
  /** The game's name for it, or the id when the game does not declare it. */
  name: string;
  minVersion: string | null;
}

export interface CatalogCompatibility {
  stores: string[];
  gameVersions: string[];
  requires: CatalogRequirement[];
  asset: string | null;
  /** One short line shown with a manual download, such as which file to pick. */
  downloadHint: string | null;
}

export interface CatalogEntry {
  id: string;
  name: string;
  author: string | null;
  description: string | null;
  /** `github_release` or `direct_hash`. */
  downloadStrategy: string;
  status: string;
  pageUrl: string | null;
  compatibility: CatalogCompatibility[];
}

export interface InstanceChoice {
  instanceId: string;
  name: string;
}

export interface CatalogListing {
  entries: CatalogEntry[];
  instances: InstanceChoice[];
}

export interface CatalogFramework {
  framework: string;
  name: string;
  minVersion: string | null;
  foundVersion: string | null;
}

export interface CatalogHash {
  sha256: string;
  verified: boolean;
  basis: string;
  confirmedPast: HashOrigin | null;
}

/** A required framework the instance does not satisfy (`UnmetFramework` in agora-core). */
export interface CatalogUnmetFramework {
  framework: string;
  name: string;
  minVersion: string | null;
  problem: 'absent' | 'too_old' | 'undetectable' | 'undeclared';
  /** The version found, when it is `too_old`. */
  foundVersion: string | null;
  /** The warning to show, as the core words it. */
  message: string;
  /** The catalog entry that provides it, when there is one. */
  catalogEntry: { id: string; name: string } | null;
}

/** The plan fields every outcome carries. */
export interface CatalogPlan {
  itemId: string;
  itemName: string;
  instanceId: string;
  source: string;
  fileName: string;
  release: string | null;
  size: number | null;
  frameworks: CatalogFramework[];
  /** Only a dry run, or an install the user chose to run past them, gets as far as naming these. */
  unmetFrameworks: CatalogUnmetFramework[];
  warnings: string[];
}

/** A `manual_link` entry that needs the player's file. Nothing changed. */
export interface CatalogManualDownload {
  state: 'manual_download';
  itemId: string;
  itemName: string;
  pageUrl: string;
  /** The file-name pattern the chosen file must match. */
  asset: string;
  hint: string | null;
}

/** The chosen file's name does not match the entry's pattern. Nothing changed. */
export interface CatalogWrongFileName {
  state: 'wrong_file_name';
  itemId: string;
  itemName: string;
  pageUrl: string;
  asset: string;
  fileName: string;
  hint: string | null;
}

/** The page publishes no checksum Agora can read. Nothing changed until the player confirms. */
export interface CatalogNeedsChecksum {
  state: 'needs_checksum';
  itemId: string;
  itemName: string;
  pageUrl: string;
  asset: string;
  fileName: string;
  hint: string | null;
}

export type CatalogInstallReply =
  | ({ state: 'dry_run' } & CatalogPlan)
  | ({ state: 'already_installed'; installedRelease: string | null; sameRelease: boolean } & CatalogPlan)
  | ({ state: 'placed'; contentItemId: string; mountPath: string; storedSize: number; hash: CatalogHash } & CatalogPlan)
  | ({
      state: 'needs_installer';
      contentItemId: string;
      storedSize: number;
      hash: CatalogHash;
      reason: string;
      topLevel: string[];
    } & CatalogPlan)
  | CatalogManualDownload
  | CatalogWrongFileName
  | CatalogNeedsChecksum;

export interface CatalogInstallRequest {
  gameId: string;
  instanceId: string;
  entryId: string;
  /** Accept a file that differs from a curator pin or an earlier install. Frameworks are separate. */
  installAnyway: boolean;
  /** Install although a required framework is unmet, as the user chose. */
  allowMissingFrameworks: boolean;
  dryRun: boolean;
  /** The file the player chose for a `manual_link` entry. */
  filePath?: string | null;
  /** The player has seen the no-checksum notice and chose to install. */
  confirmNoChecksum?: boolean;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isCatalogEntry(value: unknown): value is CatalogEntry {
  return isRecord(value)
    && typeof value.id === 'string'
    && typeof value.name === 'string'
    && Array.isArray(value.compatibility);
}

function isInstanceChoice(value: unknown): value is InstanceChoice {
  return isRecord(value) && typeof value.instanceId === 'string' && typeof value.name === 'string';
}

/** The curated entries for a game, and the instances they can be installed into. */
export async function listCatalogEntries(gameId: string): Promise<CatalogListing> {
  const reply: unknown = await invoke<unknown>('games_catalog_list', { gameId });
  if (
    !isRecord(reply)
    || !Array.isArray(reply.entries)
    || !reply.entries.every(isCatalogEntry)
    || !Array.isArray(reply.instances)
    || !reply.instances.every(isInstanceChoice)
  ) {
    throw new Error('games_catalog_list did not return a catalog.');
  }
  return reply as unknown as CatalogListing;
}

/** Plan (`dryRun`) or install one entry into an instance. */
export async function installCatalogEntry(request: CatalogInstallRequest): Promise<CatalogInstallReply> {
  const reply: unknown = await invoke<unknown>('games_catalog_install', { request });
  if (!isRecord(reply) || typeof reply.state !== 'string' || typeof reply.itemId !== 'string') {
    throw new Error('games_catalog_install did not return an install result.');
  }
  // A manual-link question carries no plan; every other state carries one.
  if (
    reply.state !== 'manual_download'
    && reply.state !== 'wrong_file_name'
    && reply.state !== 'needs_checksum'
    && !Array.isArray(reply.unmetFrameworks)
  ) {
    throw new Error('games_catalog_install did not return an install result.');
  }
  return reply as unknown as CatalogInstallReply;
}
