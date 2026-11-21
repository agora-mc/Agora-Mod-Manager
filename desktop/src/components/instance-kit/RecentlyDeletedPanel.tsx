/**
 * Recently deleted, under an instance list: deleted instances with Restore and Delete now, and an
 * optional Empty for the whole list. The markup and classes are Minecraft's
 * (`components/RecentlyDeleted.tsx`), so every game's list is the same list. Presentational: the
 * game loads the entries and answers the buttons.
 */

import type { ReactNode } from 'react';
import { ArchiveRestore, Trash2 } from 'lucide-react';

export interface RecentlyDeletedItem {
  id: string;
  name: string;
  /** When it was deleted (an ISO time). */
  deletedAt: string;
  /** When it leaves for the OS trash (an ISO time). */
  expiresAt: string;
  sizeBytes: number;
}

export function formatDeletedSize(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  if (bytes >= 1024 ** 2) return `${Math.round(bytes / 1024 ** 2)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

export function formatDeletedDay(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '';
  return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' }).format(date);
}

/** "1 day" or "30 days". */
export function daysText(days: number): string {
  return `${days} day${days === 1 ? '' : 's'}`;
}

interface RecentlyDeletedPanelProps {
  entries: readonly RecentlyDeletedItem[];
  /** How long an entry stays, for the line under the heading. */
  retentionDays: number;
  /** An action is in flight: every button waits. */
  busy: boolean;
  notice?: string | null;
  error?: string | null;
  onRestore: (entry: RecentlyDeletedItem) => void;
  onDeleteNow: (entry: RecentlyDeletedItem) => void;
  /** "Empty": delete every entry now. Left out, the button is not shown. */
  onEmpty?: () => void;
  /** A small tile before each entry's name. */
  tile?: (entry: RecentlyDeletedItem) => ReactNode;
}

/** Renders nothing while there is nothing to show. */
export function RecentlyDeletedPanel({
  entries,
  retentionDays,
  busy,
  notice = null,
  error = null,
  onRestore,
  onDeleteNow,
  onEmpty,
  tile,
}: RecentlyDeletedPanelProps) {
  if (entries.length === 0 && !error && !notice) return null;

  const heading = (
    <div>
      <h2 id="recently-deleted-heading" className="text-base font-semibold">Recently deleted</h2>
      <p className="text-sm text-muted-foreground">
        Deleted instances stay here for {daysText(retentionDays)}, then move to your Recycle Bin.
      </p>
    </div>
  );

  return (
    <section aria-labelledby="recently-deleted-heading" className="space-y-3 rounded-xl border border-border bg-card p-4" data-testid="recently-deleted">
      {onEmpty && entries.length > 0 ? (
        <div className="flex flex-wrap items-start justify-between gap-3">
          {heading}
          <button
            type="button"
            disabled={busy}
            onClick={onEmpty}
            className="inline-flex items-center gap-1.5 rounded-lg border border-destructive/30 px-3 py-1.5 text-sm font-medium text-destructive hover:bg-destructive/10 disabled:opacity-50"
          >
            <Trash2 className="h-4 w-4" aria-hidden="true" />
            Empty
          </button>
        </div>
      ) : heading}
      {notice && <p role="status" className="text-sm text-foreground">{notice}</p>}
      {error && (
        <p role="alert" className="rounded-lg bg-destructive p-2 text-sm text-destructive-foreground">{error}</p>
      )}
      <ul className="divide-y divide-border">
        {entries.map((entry) => (
          <li key={entry.id} className="flex flex-wrap items-center justify-between gap-3 py-2">
            <div className={tile ? 'flex min-w-0 items-center gap-3' : 'min-w-0'}>
              {tile?.(entry)}
              <div className="min-w-0">
                <p className="wrap-break-word font-medium">{entry.name}</p>
                <p className="text-xs text-muted-foreground">
                  Deleted {formatDeletedDay(entry.deletedAt)} · {formatDeletedSize(entry.sizeBytes)} · to the Recycle Bin after {formatDeletedDay(entry.expiresAt)}
                </p>
              </div>
            </div>
            <div className="flex gap-2">
              <button
                type="button"
                disabled={busy}
                onClick={() => onRestore(entry)}
                className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-card px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
              >
                <ArchiveRestore className="h-4 w-4" aria-hidden="true" />
                Restore
              </button>
              <button
                type="button"
                disabled={busy}
                onClick={() => onDeleteNow(entry)}
                className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-card px-3 py-1.5 text-sm font-medium text-destructive hover:bg-accent disabled:opacity-50"
              >
                <Trash2 className="h-4 w-4" aria-hidden="true" />
                Delete now
              </button>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}
