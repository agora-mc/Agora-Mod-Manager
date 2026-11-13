/**
 * The tab strip under an instance header (`.agora-tabbar`), with an optional count per tab
 * ("Content (5)"). The active tab is marked with `aria-current="page"`: each tab is a page.
 */

import { cn } from '@/lib/utils';

export interface InstanceTab<Id extends string> {
  id: Id;
  label: string;
  /** Shown as "Label (count)". Leave out when the count is not known. */
  count?: number | null;
}

interface InstanceTabBarProps<Id extends string> {
  tabs: readonly InstanceTab<Id>[];
  active: Id;
  onSelect: (id: Id) => void;
  label?: string;
  className?: string;
}

export function InstanceTabBar<Id extends string>({
  tabs,
  active,
  onSelect,
  label = 'Instance pages',
  className,
}: InstanceTabBarProps<Id>) {
  return (
    <nav aria-label={label} className={cn('agora-tabbar', className)}>
      {tabs.map((tab) => {
        const isActive = tab.id === active;
        return (
          <button
            key={tab.id}
            type="button"
            aria-current={isActive ? 'page' : undefined}
            data-active={isActive}
            onClick={() => onSelect(tab.id)}
            className="agora-tab text-sm"
          >
            {typeof tab.count === 'number' ? `${tab.label} (${tab.count})` : tab.label}
          </button>
        );
      })}
    </nav>
  );
}
