import type { ComponentType, ReactNode } from 'react';
import { cn } from '@/lib/utils';

interface PageHeroProps {
  icon: ComponentType<{ className?: string }>;
  title: ReactNode;
  subtitle?: ReactNode;
  /** A quieter third line under the subtitle. */
  note?: ReactNode;
  /** Chips or other inline content under the intro. */
  children?: ReactNode;
  /** Controls aligned with the title on wide screens. */
  actions?: ReactNode;
  /** A block that spans the full width below the title row. */
  panel?: ReactNode;
  className?: string;
  testId?: string;
}

export function PageHero({
  icon: Icon,
  title,
  subtitle,
  note,
  children,
  actions,
  panel,
  className,
  testId,
}: PageHeroProps) {
  return (
    <section className={cn('agora-hero relative overflow-hidden', className)} data-testid={testId}>
      <span className="hero-ring -right-16 -top-20 h-56 w-56" aria-hidden="true" />
      <span className="hero-ring -bottom-28 right-20 h-64 w-64" aria-hidden="true" />
      <div className="relative flex items-center gap-4">
        <span className="hero-badge" aria-hidden="true">
          <Icon className="h-6 w-6" />
        </span>
        <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-6 gap-y-4">
          <div className="hero-copy flex-1">
            <h2 className="text-2xl font-bold sm:text-3xl">{title}</h2>
            {subtitle && (
              <p className="mt-2 max-w-2xl text-sm leading-6 text-white/80 sm:text-base">
                {subtitle}
              </p>
            )}
            {note && (
              <p className="mt-2 max-w-2xl text-xs leading-5 text-muted-foreground">
                {note}
              </p>
            )}
            {children}
          </div>
          {actions && (
            <div className="ml-auto flex flex-wrap items-center justify-end gap-3">
              {actions}
            </div>
          )}
        </div>
      </div>
      {panel && <div className="relative mt-5">{panel}</div>}
    </section>
  );
}
