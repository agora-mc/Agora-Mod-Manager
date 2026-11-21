/**
 * One instance in a list: tile, name, chips, a status line and facts on the left, a lock-state
 * label in the corner, banners under them and the action bar at the foot. The markup and classes
 * are Minecraft's card (`pages/Instances.tsx`), so the two games' cards are the same card.
 */

import type { CSSProperties, ReactNode } from 'react';
import { cn } from '@/lib/utils';

interface InstanceCardFrameProps {
  /** The tile (`InstanceIcon`, or a button around it when the image can be changed). */
  tile: ReactNode;
  name: string;
  /** A small badge under the name (where an instance came from, for example). */
  badge?: ReactNode;
  /** Chips under the name: identity first (`HueChip`), then facts (`MetaChip`). */
  chips?: ReactNode;
  /** The first line under the chips: running, last played, play time. */
  status?: ReactNode;
  /** Further fact lines (created date, import origin). Empty entries are skipped. */
  facts?: readonly ReactNode[];
  /** The top-right label: lock state today. Nothing is shown when the game has no lock. */
  cornerLabel?: ReactNode;
  /** The game is running: the card wears the accent ring. */
  running?: boolean;
  /** The action row (`InstanceActionBar`). */
  actions?: ReactNode;
  /** Banners between the facts and the actions (readiness, launch answers, progress). */
  children?: ReactNode;
  className?: string;
  testId?: string;
}

export function InstanceCardFrame({
  tile,
  name,
  badge,
  chips,
  status,
  facts = [],
  cornerLabel,
  running = false,
  actions,
  children,
  className,
  testId,
}: InstanceCardFrameProps) {
  const shownFacts = facts.filter((fact) => fact !== null && fact !== undefined && fact !== false && fact !== '');
  return (
    <li
      className={cn('instance-card rounded-xl border border-border bg-card p-4', running && 'instance-card--running', className)}
      data-testid={testId}
      data-running={running || undefined}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-start gap-3">
          {tile}
          <div className="min-w-0">
            <h3 className="wrap-break-word font-semibold">{name}</h3>
            {badge}
            {chips && <div className="mt-1.5 flex flex-wrap items-center gap-1.5">{chips}</div>}
            {status && <p className="mt-1.5 text-xs text-muted-foreground">{status}</p>}
            {shownFacts.map((fact, index) => (
              <p key={index} className="mt-0.5 wrap-break-word text-xs text-muted-foreground">{fact}</p>
            ))}
          </div>
        </div>
        {cornerLabel && (
          <span className="shrink-0 text-xs uppercase tracking-wide text-muted-foreground">{cornerLabel}</span>
        )}
      </div>
      {children}
      {actions}
    </li>
  );
}

/** "Running" with the pulsing dot, in the green the card and the header both use. */
export function RunningIndicator({ children }: { children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5 text-green-600 dark:text-green-400">
      <span className="instance-status-dot" aria-hidden="true" />
      {children}
    </span>
  );
}

/**
 * The identity chip: a coloured dot and a label, in the shape of Minecraft's loader chip. The hue
 * is the game's choice (Minecraft: the loader's; another game: its store's).
 */
export function HueChip({ hue, children }: { hue: number; children: ReactNode }) {
  return (
    <span className="instance-chip" style={{ '--instance-chip-hue': String(hue) } as CSSProperties}>
      <span className="instance-chip__dot" aria-hidden="true" />
      {children}
    </span>
  );
}

/** A compact banner inside a card: a launch answer, a problem, with an optional action. */
export function InstanceCardBanner({
  tone,
  title,
  body,
  action,
  onDismiss,
  testId,
}: {
  tone: 'info' | 'warn' | 'danger';
  title: ReactNode;
  body?: ReactNode;
  action?: { label: string; onSelect: () => void; ariaLabel?: string };
  onDismiss?: () => void;
  testId?: string;
}) {
  const frame = tone === 'danger'
    ? 'border-destructive/60 bg-destructive/10'
    : tone === 'warn'
      ? 'border-amber-500/60 bg-amber-500/10'
      : 'border-border bg-background/40';
  const heading = tone === 'danger'
    ? 'text-destructive'
    : tone === 'warn'
      ? 'text-amber-700 dark:text-amber-300'
      : 'text-foreground';
  return (
    <div
      className={cn('mt-3 flex items-start justify-between gap-3 rounded-lg border px-3 py-2 text-xs', frame)}
      role={tone === 'info' ? 'status' : 'alert'}
      data-testid={testId}
    >
      <div className="min-w-0">
        <p className={cn('font-medium', heading)}>{title}</p>
        {body && <p className="mt-0.5 text-muted-foreground">{body}</p>}
      </div>
      <div className="flex shrink-0 items-center gap-1.5">
        {action && (
          <button
            type="button"
            onClick={action.onSelect}
            aria-label={action.ariaLabel}
            className="rounded-sm border border-current/30 px-2 py-1 font-medium hover:bg-background/40"
          >
            {action.label}
          </button>
        )}
        {onDismiss && (
          <button
            type="button"
            onClick={onDismiss}
            className="rounded-sm px-1.5 py-1 text-muted-foreground hover:text-foreground hover:underline"
          >
            Dismiss
          </button>
        )}
      </div>
    </div>
  );
}
