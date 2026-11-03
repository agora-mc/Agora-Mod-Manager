import { invoke } from '@tauri-apps/api/core';
import { addInstanceContent, removeStored } from '@/features/games/content/api';
import { minecraftAddStoredContent } from '@/lib/tauri';

/** One instance that has a library item. */
export interface LibraryInstance {
  instanceId: string;
  name: string;
  gameId: string;
}

/** One item in the offline library. */
export interface LibraryItem {
  itemId: string;
  name: string;
  /** `null` means no description was saved. It never contains invented text. */
  description: string | null;
  version: string | null;
  /** The one game the item belongs to, when there is exactly one answer. */
  gameId: string | null;
  /**
   * Every game the item is tied to: recorded, catalog, the item it was made from, and each
   * instance that uses it. Empty means "not tied to a game"; nothing is guessed.
   */
  gameIds: string[];
  pageUrl: string | null;
  /** The saved icon's object hash. Its bytes are loaded lazily by `loadLibraryIcon`. */
  iconSha256: string | null;
  sourceKind: string;
  sourceLabel: string;
  instances: LibraryInstance[];
  fileCount: number;
  totalSize: number;
  addedAtUnixMs: number;
  /** Plugin files at the item's top level (games with a plugin list only). */
  pluginFiles: string[];
  /** The item carries a FOMOD installer, which asks its questions when it is added. */
  hasInstaller: boolean;
}

export interface DamagedLibraryItem {
  itemId: string;
  message: string;
}

export interface LibraryListing {
  items: LibraryItem[];
  damaged: DamagedLibraryItem[];
  warnings: string[];
}

export interface LibraryInstanceOptions {
  instances: LibraryInstance[];
  warnings: string[];
}

export interface LibraryIconCached {
  saved: boolean;
  matched: number;
  iconDataUrl: string | null;
}

/** One item no instance uses. */
export interface UnusedItem {
  itemId: string;
  name: string;
  totalSize: number;
  gameId: string | null;
  /** The games the item is tied to without an instance. Empty means "not tied to a game". */
  gameIds: string[];
}

/** Items no instance uses, with their total size. Warnings mean the list may be incomplete. */
export interface UnusedListing {
  items: UnusedItem[];
  totalSize: number;
  warnings: string[];
}

function isLibraryInstance(value: unknown): value is LibraryInstance {
  if (value === null || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return (
    typeof row.instanceId === 'string'
    && typeof row.name === 'string'
    && typeof row.gameId === 'string'
  );
}

function isStringList(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((entry) => typeof entry === 'string');
}

function isLibraryItem(value: unknown): value is LibraryItem {
  if (value === null || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return (
    typeof row.itemId === 'string'
    && typeof row.name === 'string'
    && (row.description === null || typeof row.description === 'string')
    && (row.version === null || typeof row.version === 'string')
    && (row.gameId === null || typeof row.gameId === 'string')
    && isStringList(row.gameIds)
    && (row.pageUrl === null || typeof row.pageUrl === 'string')
    && (row.iconSha256 === null || typeof row.iconSha256 === 'string')
    && typeof row.sourceKind === 'string'
    && typeof row.sourceLabel === 'string'
    && Array.isArray(row.instances)
    && row.instances.every(isLibraryInstance)
    && typeof row.fileCount === 'number'
    && typeof row.totalSize === 'number'
    && typeof row.addedAtUnixMs === 'number'
    && isStringList(row.pluginFiles)
    && typeof row.hasInstaller === 'boolean'
  );
}

function isDamaged(value: unknown): value is DamagedLibraryItem {
  if (value === null || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return typeof row.itemId === 'string' && typeof row.message === 'string';
}

/** Every stored item with its saved metadata. Rejects a reply that is not a listing. */
export async function listLibrary(): Promise<LibraryListing> {
  const reply: unknown = await invoke<unknown>('games_library_list');
  if (
    reply === null
    || typeof reply !== 'object'
    || !Array.isArray((reply as { items?: unknown }).items)
    || !(reply as { items: unknown[] }).items.every(isLibraryItem)
    || !Array.isArray((reply as { damaged?: unknown }).damaged)
    || !(reply as { damaged: unknown[] }).damaged.every(isDamaged)
    || !Array.isArray((reply as { warnings?: unknown }).warnings)
    || !(reply as { warnings: unknown[] }).warnings.every((line) => typeof line === 'string')
  ) {
    throw new Error('games_library_list did not return a library listing.');
  }
  return reply as LibraryListing;
}

function isUnusedItem(value: unknown): value is UnusedItem {
  if (value === null || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return (
    typeof row.itemId === 'string'
    && typeof row.name === 'string'
    && typeof row.totalSize === 'number'
    && (row.gameId === null || typeof row.gameId === 'string')
    && isStringList(row.gameIds)
  );
}

/** Every item no instance uses. Rejects a reply that is not an unused listing. */
export async function listUnusedItems(): Promise<UnusedListing> {
  const reply: unknown = await invoke<unknown>('games_library_unused');
  if (
    reply === null
    || typeof reply !== 'object'
    || !Array.isArray((reply as { items?: unknown }).items)
    || !(reply as { items: unknown[] }).items.every(isUnusedItem)
    || typeof (reply as { totalSize?: unknown }).totalSize !== 'number'
    || !Array.isArray((reply as { warnings?: unknown }).warnings)
    || !(reply as { warnings: unknown[] }).warnings.every((line) => typeof line === 'string')
  ) {
    throw new Error('games_library_unused did not return an unused-items listing.');
  }
  return reply as UnusedListing;
}

/** Remove one item from the store. Refused while an instance uses it, or when a manifest is unreadable. */
export async function removeLibraryItem(itemId: string): Promise<void> {
  await removeStored(itemId);
}

/** Every instance of every game, for the Add-to-instance picker. */
export async function listLibraryInstances(): Promise<LibraryInstanceOptions> {
  const reply: unknown = await invoke<unknown>('games_library_instances');
  if (
    reply === null
    || typeof reply !== 'object'
    || !Array.isArray((reply as { instances?: unknown }).instances)
    || !(reply as { instances: unknown[] }).instances.every(isLibraryInstance)
    || !Array.isArray((reply as { warnings?: unknown }).warnings)
  ) {
    throw new Error('games_library_instances did not return a list of instances.');
  }
  return reply as LibraryInstanceOptions;
}

/** What adding an item to an instance did. */
export type AddOutcome =
  | { status: 'added' }
  | { status: 'needs_installer' };

/**
 * Add a library item to an instance of its game. Minecraft uses its own store command; every other
 * game uses the generic content layer command.
 */
export async function addLibraryItem(
  item: LibraryItem,
  instance: LibraryInstance,
): Promise<AddOutcome> {
  if (instance.gameId === 'minecraft') {
    const installed = await minecraftAddStoredContent(instance.instanceId, item.itemId);
    if (typeof installed?.filename !== 'string') {
      throw new Error('minecraft_add_stored_content did not return the installed mod.');
    }
    return { status: 'added' };
  }
  const outcome = await addInstanceContent(instance.instanceId, item.itemId);
  return outcome.status === 'needs_installer' ? { status: 'needs_installer' } : { status: 'added' };
}

/**
 * Instances an item can be added to: those of a game the item is tied to. An item tied to no game
 * may go to any instance of any game.
 */
export function instancesForItem(
  item: Pick<LibraryItem, 'gameIds'>,
  instances: LibraryInstance[],
): LibraryInstance[] {
  if (item.gameIds.length === 0) return instances;
  return instances.filter((instance) => item.gameIds.includes(instance.gameId));
}

/**
 * Which of the Library's two lists to show for a game: the items tied to that game, or the items
 * tied to no game at all (every game's Library offers those under their own filter, so nothing is
 * hidden or filed under the wrong game).
 */
export type LibraryView = 'game' | 'untied';

/** The items a game's Library shows in `view`. */
export function itemsInView<T extends { gameIds: string[] }>(
  items: T[],
  gameId: string,
  view: LibraryView,
): T[] {
  return view === 'untied'
    ? items.filter((item) => item.gameIds.length === 0)
    : items.filter((item) => item.gameIds.includes(gameId));
}

/** Search names, descriptions, versions, sources and instances. Empty means everything. */
export function searchLibrary(items: LibraryItem[], query: string): LibraryItem[] {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return items;
  return items.filter((item) => {
    const haystack = [
      item.name,
      item.description ?? '',
      item.version ?? '',
      item.sourceLabel,
      ...item.pluginFiles,
      ...item.instances.map((instance) => instance.name),
    ]
      .join(' ')
      .toLowerCase();
    return words.every((word) => haystack.includes(word));
  });
}

const iconByHash = new Map<string, string>();
const iconReads = new Map<string, Promise<string | null>>();

/**
 * Read one saved icon by its object hash. The listing carries only the hash; a row asks for the
 * bytes when it renders. Results (including a failure) are kept for the session.
 */
export function loadLibraryIcon(sha256: string | null): Promise<string | null> {
  if (!sha256) return Promise.resolve(null);
  const cached = iconByHash.get(sha256);
  if (cached) return Promise.resolve(cached);
  const pending = iconReads.get(sha256);
  if (pending) return pending;
  const read = invoke<string | null>('games_library_icon', { sha256 })
    .then((reply) => {
      if (typeof reply === 'string' && reply.startsWith('data:image/')) {
        iconByHash.set(sha256, reply);
        return reply;
      }
      return null;
    })
    .catch(() => null)
    .finally(() => {
      iconReads.delete(sha256);
    });
  iconReads.set(sha256, read);
  return read;
}

/** Icons already asked for this session, so a shown row does not ask again. */
const attemptedIcons = new Set<string>();

async function saveIcon(key: string, args: Record<string, unknown>): Promise<LibraryIconCached | null> {
  if (attemptedIcons.has(key)) return null;
  attemptedIcons.add(key);
  try {
    const reply = await invoke<LibraryIconCached>('games_library_cache_icon', args);
    return reply ?? null;
  } catch {
    // An icon that will not save is never an error for the page.
    return null;
  }
}

/** Save an item's icon after showing it online, so it can be shown offline next time. */
export function cacheLibraryIcon(itemId: string, url: string | null): Promise<LibraryIconCached | null> {
  if (!url) return Promise.resolve(null);
  return saveIcon(`item:${itemId}:${url}`, { itemId, catalogId: null, url });
}

/** Save a catalog entry's icon on every stored item that came from it (used while browsing). */
export function cacheCatalogIcon(catalogId: string, url: string | null): Promise<LibraryIconCached | null> {
  if (!url) return Promise.resolve(null);
  return saveIcon(`catalog:${catalogId}:${url}`, { itemId: null, catalogId, url });
}

export interface LibraryFillSkip {
  itemId: string;
  name: string;
  reason: string;
}

export interface LibraryFillReport {
  considered: number;
  updated: number;
  skipped: LibraryFillSkip[];
  warnings: string[];
}

/** Fill missing descriptions, titles, versions, pages and icons from Modrinth. */
export async function fillMissingLibraryDetails(): Promise<LibraryFillReport> {
  const reply = await invoke<LibraryFillReport>('games_library_fill_details');
  if (
    reply === null
    || typeof reply !== 'object'
    || typeof reply.considered !== 'number'
    || typeof reply.updated !== 'number'
    || !Array.isArray(reply.skipped)
    || !Array.isArray(reply.warnings)
  ) {
    throw new Error('games_library_fill_details did not return a report.');
  }
  return reply;
}
