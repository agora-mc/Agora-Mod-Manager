/**
 * The header on top of every page of one instance, in the shape of Minecraft's instance page
 * (`pages/InstanceEditor.tsx`): tile, name, chips and facts on the left; secondary actions, the
 * big Launch (or Stop) and how ready the instance is on the right; and a full-width strip under
 * both for whatever a launch answers. Every part is a slot, so each game fills in its own.
 */

import { Fragment, type ReactNode } from 'react';
import { Loader2, Play, Square } from 'lucide-react';
import { cn } from '@/lib/utils';
import { emphasisOf, supportedActions, type InstanceAction, type InstanceCapabilities } from './types';

interface InstancePageHeaderProps {
  tile: ReactNode;
  name: string;
  /** Beside the name, on its baseline (Minecraft's "Rename"). Not part of the heading's name. */
  nameAccessory?: ReactNode;
  /** Beside the heading ("Running now"). */
  status?: ReactNode;
  chips?: ReactNode;
  /** Under the chips: notices that belong to the instance itself. */
  notices?: ReactNode;
  /** Leads the facts row (the lock state and its buttons). */
  factsLead?: ReactNode;
  /** Facts, shown on one row separated by " · " ("Last played …", "7 h played", "Created …"). */
  facts?: readonly ReactNode[];
  /** After the facts, with no separator (Minecraft's "Set image"). */
  factsTrail?: ReactNode;
  /** Outlined buttons above Launch. Unsupported ones are left out when `capabilities` is given. */
  secondaryActions?: readonly InstanceAction[];
  capabilities?: InstanceCapabilities;
  /** The big button: Launch, or Stop while the game runs. */
  primaryAction?: InstanceAction;
  /** Under the big button (`ReadinessLine`). */
  readiness?: ReactNode;
  running?: boolean;
  /** Launch progress, questions and answers: a full-width strip under a rule. */
  banners?: ReactNode;
  testId?: string;
}

function hasContent(node: ReactNode): boolean {
  return node !== null && node !== undefined && node !== false && node !== '';
}

export function InstancePageHeader({
  tile,
  name,
  nameAccessory,
  status,
  chips,
  notices,
  factsLead,
  facts = [],
  factsTrail,
  secondaryActions = [],
  capabilities,
  primaryAction,
  readiness,
  running = false,
  banners,
  testId,
}: InstancePageHeaderProps) {
  const shownFacts = facts.filter(hasContent);
  const shownSecondary = supportedActions(secondaryActions, capabilities);
  const shownPrimary = primaryAction && supportedActions([primaryAction], capabilities)[0];

  return (
    <section
      className={cn('rounded-xl border border-border bg-card p-6', running && 'instance-card--running')}
      data-testid={testId}
      data-running={running || undefined}
    >
      <div className="flex flex-col gap-4 xl:flex-row xl:items-start xl:justify-between">
        <div className="flex min-w-0 items-start gap-4">
          {tile}
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-3">
              {/* The accessory sits on the heading's baseline, as Minecraft's Rename does, but
                  outside it, so the heading's name is the instance's name alone. */}
              <div className="flex min-w-0 flex-wrap items-baseline gap-x-1.5">
                <h2 className="wrap-break-word text-2xl font-bold">{name}</h2>
                {nameAccessory}
              </div>
              {status}
            </div>
            {chips && <div className="mt-1.5 flex flex-wrap items-center gap-1.5">{chips}</div>}
            {notices}
            {(factsLead || shownFacts.length > 0 || factsTrail) && (
              <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground" data-testid="instance-facts">
                {factsLead}
                {shownFacts.map((fact, index) => (
                  <Fragment key={index}>
                    {(index > 0 || factsLead) && <span aria-hidden="true">·</span>}
                    <span>{fact}</span>
                  </Fragment>
                ))}
                {factsTrail}
              </div>
            )}
          </div>
        </div>

        {(shownSecondary.length > 0 || shownPrimary || readiness) && (
          <div className="flex shrink-0 flex-col items-start gap-3 xl:items-end">
            {shownSecondary.length > 0 && (
              <div className="flex flex-wrap gap-2 xl:justify-end">
                {shownSecondary.map((action) => (
                  <button
                    key={action.id}
                    type="button"
                    onClick={action.onSelect}
                    disabled={action.disabled || action.busy}
                    aria-label={action.ariaLabel}
                    title={action.title}
                    data-testid={action.testId}
                    className="rounded-lg border border-input bg-background px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    {action.label}
                  </button>
                ))}
              </div>
            )}
            {shownPrimary && <HeaderPrimaryButton action={shownPrimary} />}
            {readiness}
          </div>
        )}
      </div>

      {hasContent(banners) && <div className="mt-4 space-y-3 border-t border-border pt-4">{banners}</div>}
    </section>
  );
}

function HeaderPrimaryButton({ action }: { action: InstanceAction }) {
  const stop = emphasisOf(action) === 'stop';
  const Icon = action.busy ? Loader2 : stop ? Square : Play;
  return (
    <button
      type="button"
      onClick={action.onSelect}
      disabled={action.disabled || action.busy}
      aria-label={action.ariaLabel}
      aria-busy={action.busy || undefined}
      title={action.title}
      data-testid={action.testId}
      className={cn(
        'inline-flex items-center gap-2 rounded-lg px-5 py-3 text-base font-semibold shadow-xs disabled:cursor-not-allowed disabled:opacity-50',
        stop
          ? 'bg-destructive text-destructive-foreground hover:bg-destructive/90'
          : 'bg-primary text-primary-foreground hover:bg-primary/90',
      )}
    >
      <Icon className={cn('h-5 w-5', action.busy ? 'animate-spin' : 'fill-current')} aria-hidden="true" />
      {action.label}
    </button>
  );
}

/** Minecraft's "Rename" beside an instance's name in its page header. */
export function RenameLink({ onSelect, disabled, label = 'Rename' }: { onSelect: () => void; disabled?: boolean; label?: string }) {
  return (
    <button
      type="button"
      onClick={onSelect}
      disabled={disabled}
      className="text-xs text-muted-foreground hover:text-foreground underline"
    >
      {label}
    </button>
  );
}
