/**
 * IPC wrappers for noticing mod downloads. The Rust side (`desktop/src-tauri/src/downloads.rs`)
 * recognises archives by their contents and never imports anything on its own.
 */

import { invoke } from '@tauri-apps/api/core';

/** One recognised download in the Library's "New downloads" section. */
export interface NewDownload {
  path: string;
  fileName: string;
  size: number;
  sha256: string;
  gameId: string;
  kind: string;
  /** Why Agora recognised it, e.g. "has fabric.mod.json". */
  reason: string;
  /** A Nexus mod id the file name suggests. A label only; it decides nothing. */
  nexusModId: string | null;
  /** The store already holds these bytes, so the row is a note, not an offer. */
  alreadyInLibrary: boolean;
}

/** What adding a download to the library did. */
export type AddDownloadResult =
  | { status: 'added'; itemId: string }
  | { status: 'needs_installer'; itemId: string; title: string };

function isNewDownload(value: unknown): value is NewDownload {
  if (value === null || typeof value !== 'object') return false;
  const row = value as Record<string, unknown>;
  return (
    typeof row.path === 'string'
    && typeof row.fileName === 'string'
    && typeof row.size === 'number'
    && typeof row.sha256 === 'string'
    && typeof row.gameId === 'string'
    && typeof row.kind === 'string'
    && typeof row.reason === 'string'
    && (row.nexusModId === null || typeof row.nexusModId === 'string')
    && typeof row.alreadyInLibrary === 'boolean'
  );
}

/** The new downloads Agora recognised. Rejects a reply that is not a list of rows. */
export async function listNewDownloads(): Promise<NewDownload[]> {
  const reply: unknown = await invoke<unknown>('downloads_list');
  if (!Array.isArray(reply) || !reply.every(isNewDownload)) {
    throw new Error('downloads_list did not return a list of new downloads.');
  }
  return reply;
}

/** Poll the folder now. Returns how many new notices this poll added. */
export async function scanNewDownloads(): Promise<number> {
  const reply: unknown = await invoke<unknown>('downloads_scan');
  if (typeof reply !== 'number') {
    throw new Error('downloads_scan did not return a count.');
  }
  return reply;
}

/** Stop offering a download, by hash. */
export async function ignoreDownload(sha256: string): Promise<void> {
  await invoke<void>('downloads_ignore', { sha256 });
}

/** Store a download through its game's own add path. */
export async function addDownload(sha256: string): Promise<AddDownloadResult> {
  const reply: unknown = await invoke<unknown>('downloads_add', { sha256 });
  if (reply === null || typeof reply !== 'object') {
    throw new Error('downloads_add did not return an outcome.');
  }
  const result = reply as { status?: unknown; itemId?: unknown; title?: unknown };
  if (result.status === 'added' && typeof result.itemId === 'string') {
    return { status: 'added', itemId: result.itemId };
  }
  if (
    result.status === 'needs_installer'
    && typeof result.itemId === 'string'
    && typeof result.title === 'string'
  ) {
    return { status: 'needs_installer', itemId: result.itemId, title: result.title };
  }
  throw new Error('downloads_add returned an unknown outcome.');
}

/** The platform Downloads folder, for the setting's default. */
export async function defaultDownloadsFolder(): Promise<string | null> {
  const reply: unknown = await invoke<unknown>('downloads_default_folder');
  return typeof reply === 'string' ? reply : null;
}

/** A size as a person reads it. */
export function formatDownloadSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} bytes`;
}
