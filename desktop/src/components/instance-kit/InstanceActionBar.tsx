/**
 * The row of actions at the foot of an instance card, and the button style shared with the
 * instance page header. Taken from Minecraft's card: one filled primary action, outlined
 * secondary ones, and Delete in quiet red at the end of the row.
 */

import {
  AlertTriangle,
  Copy,
  FolderInput,
  FolderOpen,
  LifeBuoy,
  Loader2,
  Pencil,
  Play,
  Square,
  Trash2,
  type LucideIcon,
} from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  emphasisOf,
  supportedActions,
  type InstanceAction,
  type InstanceActionEmphasis,
  type InstanceActionId,
  type InstanceCapabilities,
} from './types';

const ACTION_ICON: Record<InstanceActionId, LucideIcon> = {
  launch: Play,
  stop: Square,
  review: AlertTriangle,
  edit: Pencil,
  clone: Copy,
  group: FolderInput,
  troubleshoot: LifeBuoy,
  openFolder: FolderOpen,
  delete: Trash2,
};

/** Filled icons read as "go" and "stop"; the rest are outlines. */
const FILLED_ICON: ReadonlySet<InstanceActionId> = new Set(['launch', 'stop']);

const BASE = 'inline-flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-50';

const EMPHASIS_CLASS: Record<InstanceActionEmphasis, string> = {
  primary: 'bg-primary text-primary-foreground hover:bg-primary/90',
  stop: 'bg-destructive text-destructive-foreground hover:bg-destructive/90',
  secondary: 'border border-border hover:bg-accent',
  danger: 'ml-auto border border-destructive/30 text-destructive hover:bg-destructive/10',
};

/** One action as a card-sized button. Exported for the odd action that sits outside a bar. */
export function InstanceActionButton({ action }: { action: InstanceAction }) {
  const Icon = action.busy ? Loader2 : ACTION_ICON[action.id];
  const filled = !action.busy && FILLED_ICON.has(action.id);
  return (
    <button
      type="button"
      onClick={action.onSelect}
      disabled={action.disabled || action.busy}
      aria-label={action.ariaLabel}
      aria-busy={action.busy || undefined}
      title={action.title}
      data-testid={action.testId}
      data-action={action.id}
      className={cn(BASE, EMPHASIS_CLASS[emphasisOf(action)])}
    >
      <Icon className={cn('h-3.5 w-3.5', filled && 'fill-current', action.busy && 'animate-spin')} aria-hidden="true" />
      {action.label}
    </button>
  );
}

interface InstanceActionBarProps {
  actions: readonly InstanceAction[];
  /** When given, actions the game does not support are left out (never shown disabled). */
  capabilities?: InstanceCapabilities;
  className?: string;
}

export function InstanceActionBar({ actions, capabilities, className }: InstanceActionBarProps) {
  const shown = supportedActions(actions, capabilities);
  if (shown.length === 0) return null;
  return (
    <div className={cn('mt-4 flex flex-wrap gap-2', className)} data-testid="instance-actions">
      {shown.map((action) => (
        <InstanceActionButton key={action.id} action={action} />
      ))}
    </div>
  );
}
