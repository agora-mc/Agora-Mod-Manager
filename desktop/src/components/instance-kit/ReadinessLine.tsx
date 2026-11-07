/**
 * "Is this instance ready to play?", the short line under Launch. Coloured by level; when the
 * page can show the checks, the line is a button that opens them.
 */

import { cn } from '@/lib/utils';
import type { ReadinessLevel, ReadinessView } from './types';

export const READINESS_TEXT_CLASS: Record<ReadinessLevel, string> = {
  blocked: 'text-destructive',
  check: 'text-amber-700 dark:text-amber-300',
  ready: 'text-green-700 dark:text-green-400',
  unknown: 'text-muted-foreground',
};

interface ReadinessLineProps {
  readiness: ReadinessView;
  /** Opens the checks. Without it the line is plain text. */
  onSelect?: () => void;
  testId?: string;
  className?: string;
}

export function ReadinessLine({ readiness, onSelect, testId = 'instance-readiness', className }: ReadinessLineProps) {
  const details = readiness.details?.filter((detail) => detail !== '') ?? [];
  const headline = (
    <>
      <span className={cn('font-medium', READINESS_TEXT_CLASS[readiness.level])}>{readiness.headline}</span>
      {readiness.refreshing && <span className="ml-1.5 font-normal text-muted-foreground">· checking again…</span>}
    </>
  );
  const body = details.length > 0 && <span className="block text-muted-foreground">{details.join(' · ')}</span>;

  if (onSelect) {
    return (
      <button
        type="button"
        onClick={onSelect}
        className={cn('max-w-sm text-left text-xs underline-offset-4 hover:underline xl:text-right', className)}
        data-testid={testId}
        data-level={readiness.level}
      >
        {headline}
        {body}
      </button>
    );
  }
  return (
    <div className={cn('max-w-sm text-left text-xs xl:text-right', className)} data-testid={testId} data-level={readiness.level}>
      <p>{headline}</p>
      {body}
    </div>
  );
}
