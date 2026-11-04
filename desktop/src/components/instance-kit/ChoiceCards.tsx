/**
 * Pick one of a few options, each a card with a sentence on what it means. The option Agora
 * advises carries a "Recommended" badge. That is separate from the selected value: a user whose
 * own default is another option still sees which one Agora recommends.
 *
 * A radio group: one tab stop, arrow keys move the choice (controller navigation treats radios as
 * candidates of their own). `compact` draws the options as a segmented control.
 */

import { useRef, type KeyboardEvent, type ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface ChoiceOption<Value extends string> {
  value: Value;
  label: string;
  /** What choosing it gives and costs, in a sentence or two. Not shown when `compact`. */
  description?: ReactNode;
  recommended?: boolean;
}

/** "Label (recommended)", for a `<select>` option where a badge cannot go. */
export function recommendedLabel(option: { label: string; recommended?: boolean }): string {
  return option.recommended ? `${option.label} (recommended)` : option.label;
}

/** The small badge a recommended option wears. */
export function RecommendedBadge() {
  return (
    <span className="inline-flex shrink-0 items-center rounded-full border border-primary/40 bg-primary/15 px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-foreground">
      Recommended
    </span>
  );
}

interface ChoiceCardsProps<Value extends string> {
  /** The group's accessible name. */
  label: string;
  options: readonly ChoiceOption<Value>[];
  value: Value;
  onChange: (value: Value) => void;
  disabled?: boolean;
  compact?: boolean;
  className?: string;
}

export function ChoiceCards<Value extends string>({
  label,
  options,
  value,
  onChange,
  disabled = false,
  compact = false,
  className,
}: ChoiceCardsProps<Value>) {
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  // A value that is none of the options (nothing chosen yet) checks no card; the first is then
  // the group's tab stop.
  const selectedIndex = options.findIndex((option) => option.value === value);
  const tabStop = Math.max(0, selectedIndex);

  const move = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const step = event.key === 'ArrowRight' || event.key === 'ArrowDown'
      ? 1
      : event.key === 'ArrowLeft' || event.key === 'ArrowUp'
        ? -1
        : 0;
    if (step === 0 || options.length === 0) return;
    event.preventDefault();
    const next = (index + step + options.length) % options.length;
    onChange(options[next].value);
    refs.current[next]?.focus();
  };

  return (
    <div
      role="radiogroup"
      aria-label={label}
      aria-disabled={disabled || undefined}
      className={cn(
        compact ? 'grid grid-cols-2 gap-1 rounded-lg bg-muted p-1' : 'grid gap-2 sm:grid-cols-2',
        className,
      )}
    >
      {options.map((option, index) => {
        const chosen = index === selectedIndex;
        return (
          <button
            key={option.value}
            ref={(node) => { refs.current[index] = node; }}
            type="button"
            role="radio"
            aria-checked={chosen}
            tabIndex={index === tabStop ? 0 : -1}
            disabled={disabled}
            onClick={() => onChange(option.value)}
            onKeyDown={(event) => move(event, index)}
            data-recommended={option.recommended || undefined}
            className={cn(
              'transition-colors focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50',
              compact
                ? cn(
                    'rounded-md px-2 py-1.5 text-center text-sm font-medium leading-tight',
                    chosen ? 'bg-background text-foreground shadow-sm' : 'text-muted-foreground hover:text-foreground',
                  )
                : cn(
                    'rounded-lg border p-3 text-left text-sm',
                    chosen ? 'border-primary bg-primary/10 ring-1 ring-primary/50' : 'border-border hover:bg-accent',
                  ),
            )}
          >
            <span className={cn('flex items-center gap-2', compact ? 'justify-center' : 'justify-between')}>
              <span className={compact ? undefined : 'font-semibold'}>{option.label}</span>
              {option.recommended && (compact ? <span className="sr-only"> (recommended)</span> : <RecommendedBadge />)}
            </span>
            {!compact && option.description && (
              <span className="mt-1 block text-xs text-muted-foreground">{option.description}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}
