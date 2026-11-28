/**
 * The instance list layout: a grid that fills the window (and grows with the text-size setting),
 * optional collapsible groups, skeleton cards while loading, and an empty state. Minecraft's
 * layout from `pages/Instances.tsx`.
 */

import type { ReactNode } from 'react';

/** The auto-fill grid Minecraft's list uses: as many 16rem columns as fit, scaled with the text. */
export const INSTANCE_GRID_CLASS =
  'grid grid-cols-[repeat(auto-fill,minmax(min(100%,calc(16rem*var(--font-scale))),1fr))] gap-4';

/** A placeholder card with the real card's footprint, so the page does not jump when rows land. */
export function InstanceCardSkeleton() {
  return (
    <li className="rounded-xl border border-border bg-card p-4" data-testid="instance-card-skeleton">
      <div className="flex items-start gap-3">
        <div className="instance-skeleton__block h-14 w-14 shrink-0" />
        <div className="min-w-0 flex-1 space-y-2">
          <div className="instance-skeleton__block h-4 w-1/2" />
          <div className="instance-skeleton__block h-3 w-3/4" />
          <div className="instance-skeleton__block h-3 w-1/3" />
        </div>
      </div>
      <div className="mt-4 flex gap-2">
        <div className="instance-skeleton__block h-8 w-20" />
        <div className="instance-skeleton__block h-8 w-16" />
        <div className="instance-skeleton__block h-8 w-16" />
      </div>
    </li>
  );
}

export interface InstanceGridGroup {
  /** Stable key; also what `collapsedGroups` holds. `''` is the ungrouped bucket. */
  id: string;
  /** The heading shown when there is more than one group. */
  label: string;
  /** The cards (`InstanceCardFrame` items). */
  cards: ReactNode;
  count: number;
}

interface InstanceGridProps {
  groups: readonly InstanceGridGroup[];
  /** Skeleton cards instead of the groups. */
  loading?: boolean;
  /** How many skeleton cards to show while loading. */
  skeletonCount?: number;
  /** Shown when nothing is loading and every group is empty. */
  empty?: ReactNode;
  /** The ids of collapsed groups. Group headings only show with more than one group. */
  collapsedGroups?: readonly string[];
  onToggleGroup?: (id: string) => void;
  /** The accessible name of the list when there is a single group. */
  label?: string;
}

export function InstanceGrid({
  groups,
  loading = false,
  skeletonCount = 2,
  empty,
  collapsedGroups = [],
  onToggleGroup,
  label = 'Instances',
}: InstanceGridProps) {
  if (loading) {
    return (
      <ul className="grid grid-cols-1 gap-4 md:grid-cols-2" aria-label="Loading instances…" aria-busy="true">
        {Array.from({ length: skeletonCount }, (_, index) => (
          <InstanceCardSkeleton key={index} />
        ))}
      </ul>
    );
  }

  const total = groups.reduce((sum, group) => sum + group.count, 0);
  if (total === 0) return <>{empty}</>;

  if (groups.length === 1) {
    return (
      <ul className={INSTANCE_GRID_CLASS} aria-label={label}>
        {groups[0].cards}
      </ul>
    );
  }

  return (
    <div className="space-y-6">
      {groups.map((group) => {
        const collapsed = collapsedGroups.includes(group.id);
        return (
          <section key={group.id || '__ungrouped'} aria-label={group.label}>
            <button
              type="button"
              onClick={() => onToggleGroup?.(group.id)}
              aria-expanded={!collapsed}
              className="mb-3 flex items-center gap-2 text-sm font-semibold text-muted-foreground hover:text-foreground"
            >
              <span aria-hidden="true">{collapsed ? '▸' : '▾'}</span>
              {group.label} <span className="font-normal">({group.count})</span>
            </button>
            {!collapsed && (
              <ul className={INSTANCE_GRID_CLASS} aria-label={group.label}>
                {group.cards}
              </ul>
            )}
          </section>
        );
      })}
    </div>
  );
}
