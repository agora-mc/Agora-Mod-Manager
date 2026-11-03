/**
 * How long a deleted instance waits in Recently deleted, for every game. One setting governs
 * Minecraft's list and every other game's: core reads it (`game_instance::trash::retention_days`)
 * and the hosts pass it to Minecraft's own list, so this file only reads it for the copy.
 */

import { useEffect, useState } from 'react';
import { getSetting } from './tauri';

/** The setting core reads for the window (`game_instance::trash::RETENTION_SETTING`). */
export const RECENTLY_DELETED_DAYS_KEY = 'games_recently_deleted_days';
/** Core's default and its upper bound (`DEFAULT_RETENTION_DAYS`, `MAX_RETENTION_DAYS`). */
export const RECOMMENDED_RETENTION_DAYS = 30;
export const MAX_RETENTION_DAYS = 366;

/** Read the stored window the way core does: a number or a numeric string, clamped; else 30. */
export function parseRetentionDays(value: unknown): number {
  const number = typeof value === 'number'
    ? value
    : typeof value === 'string' && value.trim() !== '' ? Number(value.trim()) : Number.NaN;
  if (!Number.isFinite(number)) return RECOMMENDED_RETENTION_DAYS;
  return Math.min(MAX_RETENTION_DAYS, Math.max(1, Math.trunc(number)));
}

/** The player's window now. Unreadable reads as core's default, which is what core falls back to. */
export async function readRecentlyDeletedDays(): Promise<number> {
  try {
    return parseRetentionDays(await getSetting(RECENTLY_DELETED_DAYS_KEY));
  } catch {
    return RECOMMENDED_RETENTION_DAYS;
  }
}

/** The player's window, 30 until it is read (or when it cannot be). */
export function useRecentlyDeletedDays(): number {
  const [days, setDays] = useState(RECOMMENDED_RETENTION_DAYS);
  useEffect(() => {
    let cancelled = false;
    void readRecentlyDeletedDays().then((value) => {
      if (!cancelled) setDays(value);
    });
    return () => {
      cancelled = true;
    };
  }, []);
  return days;
}
