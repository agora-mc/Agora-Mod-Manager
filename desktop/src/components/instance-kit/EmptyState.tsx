/**
 * "Nothing here yet", with the hero's badge, a sentence on what to do, and the buttons that do
 * it. Minecraft's empty instance list (`pages/Instances.tsx`), usable for any empty list.
 */

import type { ReactNode } from 'react';
import type { LucideIcon } from 'lucide-react';
import { cn } from '@/lib/utils';

interface EmptyStateProps {
  icon: LucideIcon;
  title: ReactNode;
  body?: ReactNode;
  /** Buttons (`PageHeroAction` reads right here too). */
  actions?: ReactNode;
  className?: string;
  testId?: string;
}

export function EmptyState({ icon: Icon, title, body, actions, className, testId }: EmptyStateProps) {
  return (
    <div className={cn('rounded-xl border border-dashed border-border bg-card p-8 text-center', className)} data-testid={testId}>
      <span className="settings-hero-badge mx-auto" aria-hidden="true">
        <Icon className="h-5 w-5" />
      </span>
      <h3 className="mt-4 font-semibold text-foreground">{title}</h3>
      {body && <p className="mx-auto mt-2 max-w-md text-sm text-muted-foreground">{body}</p>}
      {actions && <div className="mt-4 flex flex-wrap justify-center gap-2">{actions}</div>}
    </div>
  );
}
