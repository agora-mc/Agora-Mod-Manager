/**
 * Each game instance's own image, shared by its card and its page header: setting one on the
 * page shows on the card at once. An instance with no image (or one that could not be read)
 * shows the generated tile, so a failed read is never an error on screen.
 */

import { useEffect, useSyncExternalStore } from 'react';
import { pickOpenFile } from '@/lib/tauri';
import { getGameInstanceIcon, setGameInstanceIcon } from './api';

/** The image types core accepts, the same as for a Minecraft instance. */
export const INSTANCE_IMAGE_EXTENSIONS = ['png', 'jpg', 'jpeg', 'webp', 'gif'];

const known = new Map<string, string | null>();
const pending = new Set<string>();
const listeners = new Set<() => void>();

function publish(instanceId: string, icon: string | null) {
  known.set(instanceId, icon);
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function load(instanceId: string) {
  if (known.has(instanceId) || pending.has(instanceId)) return;
  pending.add(instanceId);
  getGameInstanceIcon(instanceId)
    .then((icon) => publish(instanceId, icon))
    // Decoration: the generated tile stands in, and the next visit asks again.
    .catch(() => undefined)
    .finally(() => pending.delete(instanceId));
}

/** The instance's image as a data URL, or null while unknown or when it has none. */
export function useGameInstanceIcon(instanceId: string): string | null {
  const icon = useSyncExternalStore(subscribe, () => known.get(instanceId) ?? null);
  useEffect(() => load(instanceId), [instanceId]);
  return icon;
}

/**
 * Ask for an image and make it the instance's. Resolves false when the player cancelled the
 * picker; rejects with core's reason when the file is refused (type, size, not an image).
 */
export async function chooseGameInstanceIcon(instanceId: string): Promise<boolean> {
  const sourcePath = await pickOpenFile('Choose instance image', INSTANCE_IMAGE_EXTENSIONS);
  if (!sourcePath) return false;
  publish(instanceId, await setGameInstanceIcon(instanceId, sourcePath));
  return true;
}

/** Forget every answer, so one test's images do not show in the next. */
export function resetGameInstanceIconsForTests() {
  known.clear();
  pending.clear();
}
