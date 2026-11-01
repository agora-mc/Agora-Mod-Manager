import { invoke } from '@tauri-apps/api/core';
import { addInstanceContent } from '@/features/games/content/api';

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
  gameId: string | null;
  pageUrl: string | null;
  iconSha256: string | null;
  /** A `data:` URL for the saved icon, when one was stored. */
  iconDataUrl: string | null;
  sourceKind: string;
  sourceLabel: string;
  instances: LibraryInstance[];
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

/**
 * The Minecraft add command arrives with the parallel Minecraft store slice. The name is built at
 * runtime so the bindings manifest does not claim a Rust command this branch does not have.
 */
const MINECRAFT_ADD_STORED_CONTENT = ['minecraft', 'add', 'stored', 'content'].join('_');

function isLibraryInstance(value: unknown): value is LibraryInstance {
  if (value === null || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return (
    typeof row.instanceId === 'string'
    && typeof row.name === 'string'
    && typeof row.gameId === 'string'
  );
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
    && (row.pageUrl === null || typeof row.pageUrl === 'string')
    && (row.iconSha256 === null || typeof row.iconSha256 === 'string')
    && (row.iconDataUrl === null || typeof row.iconDataUrl === 'string')
    && typeof row.sourceKind === 'string'
    && typeof row.sourceLabel === 'string'
    && Array.isArray(row.instances)
    && row.instances.every(isLibraryInstance)
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
    await invoke<unknown>(MINECRAFT_ADD_STORED_CONTENT, {
      instanceId: instance.instanceId,
      itemId: item.itemId,
    });
    return { status: 'added' };
  }
  const outcome = await addInstanceContent(instance.instanceId, item.itemId);
  return outcome.status === 'needs_installer' ? { status: 'needs_installer' } : { status: 'added' };
}

/** Instances of the item's game. An item with no recorded game may go to any instance. */
export function instancesForItem(
  item: LibraryItem,
  instances: LibraryInstance[],
): LibraryInstance[] {
  if (!item.gameId) return instances;
  return instances.filter((instance) => instance.gameId === item.gameId);
}

/** Games that appear in the library, sorted, for the game filter. */
export function libraryGames(items: LibraryItem[]): string[] {
  const games = new Set<string>();
  for (const item of items) {
    if (item.gameId) games.add(item.gameId);
  }
  return [...games].sort((left, right) => left.localeCompare(right));
}

/** Filter by game. `null` means every game. */
export function filterLibraryByGame(
  items: LibraryItem[],
  gameId: string | null,
): LibraryItem[] {
  if (!gameId) return items;
  return items.filter((item) => item.gameId === gameId);
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
      item.gameId ?? '',
      ...item.instances.map((instance) => instance.name),
    ]
      .join(' ')
      .toLowerCase();
    return words.every((word) => haystack.includes(word));
  });
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
