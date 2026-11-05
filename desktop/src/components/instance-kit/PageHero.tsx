/**
 * The warm panel every main page opens on: a badge, the title, a sentence, small chips and the
 * page's actions. Minecraft's `.agora-hero.compact` block from "My Instances", so a page built on
 * it is the same page in every game. The colours follow the theme and the accent (`index.css`).
 */

import type { ReactNode } from 'react';
import type { LucideIcon } from 'lucide-react';
import { cn } from '@/lib/utils';
import { PageHero as BasePageHero } from '@/components/PageHero';

interface PageHeroProps {
  icon: LucideIcon;
  title: ReactNode;
  description?: ReactNode;
  /** Small facts under the description ("3 instances", "● Grounded running"). */
  chips?: readonly ReactNode[];
  /** The page's buttons, right of the text (`PageHeroAction`). */
  actions?: ReactNode;
  className?: string;
  testId?: string;
}

export function PageHero({ icon, title, description, chips = [], actions, className, testId }: PageHeroProps) {
  const shownChips = chips.filter((chip) => chip !== null && chip !== undefined && chip !== false);
  return (
    <BasePageHero
      icon={icon}
      title={title}
      subtitle={description}
      actions={actions}
      className={className}
      testId={testId}
    >
      {shownChips.length > 0 && (
        <div className="mt-2 flex flex-wrap items-center gap-2">
          {shownChips.map((chip, index) => (
            <span key={index} className="hero-chip">{chip}</span>
          ))}
        </div>
      )}
    </BasePageHero>
  );
}

interface PageHeroActionProps {
  icon?: LucideIcon;
  children: ReactNode;
  onClick: () => void;
  /** The page's main action: filled with the accent. One per hero. */
  primary?: boolean;
  disabled?: boolean;
  title?: string;
  'aria-label'?: string;
  testId?: string;
}

/** A hero button in Minecraft's two weights: filled primary, or a card-coloured outline. */
export function PageHeroAction({
  icon: Icon,
  children,
  onClick,
  primary = false,
  disabled,
  title,
  'aria-label': ariaLabel,
  testId,
}: PageHeroActionProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={ariaLabel}
      data-testid={testId}
      className={cn(
        'inline-flex items-center gap-2 whitespace-nowrap rounded-lg px-4 py-2 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-50',
        primary
          ? 'bg-primary text-primary-foreground hover:bg-primary/90'
          : 'border border-border bg-card hover:bg-accent',
      )}
    >
      {Icon && <Icon className="h-4 w-4" aria-hidden="true" />}
      {children}
    </button>
  );
}
