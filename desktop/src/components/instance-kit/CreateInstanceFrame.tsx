/**
 * The frame of a "new instance" dialog: title, a sentence on what an instance is, the game's own
 * form body, progress and error lines, and Cancel / Create. Minecraft's create dialog
 * (`pages/Instances.tsx`) without its body, so each game brings only the fields it needs.
 */

import type { ReactNode } from 'react';
import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog';

interface CreateInstanceFrameProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: ReactNode;
  description?: ReactNode;
  /** The game's fields (`FormField`, `ChoiceCards`). */
  children: ReactNode;
  onSubmit: () => void;
  /** Create is pressable. Say why it is not with a `FormField` hint. */
  canSubmit: boolean;
  /** Creating: the buttons wait and the dialog cannot be closed. */
  busy?: boolean;
  submitLabel?: string;
  busyLabel?: string;
  cancelLabel?: string;
  /** A line under the form while creating ("Hashing 120 of 3,400 files"). */
  progress?: ReactNode;
  error?: ReactNode;
  /** Replaces the footer, for a dialog that cannot offer Create (no install found). */
  footer?: ReactNode;
  testId?: string;
}

export function CreateInstanceFrame({
  open,
  onOpenChange,
  title,
  description,
  children,
  onSubmit,
  canSubmit,
  busy = false,
  submitLabel = 'Create',
  busyLabel = 'Creating…',
  cancelLabel = 'Cancel',
  progress,
  error,
  footer,
  testId,
}: CreateInstanceFrameProps) {
  return (
    <Dialog open={open} onOpenChange={(next) => { if (!busy) onOpenChange(next); }}>
      <DialogContent className="max-h-[90vh] max-w-lg overflow-y-auto" data-testid={testId}>
        <DialogTitle>{title}</DialogTitle>
        {description && <DialogDescription>{description}</DialogDescription>}
        <form
          className="space-y-4"
          onSubmit={(event) => {
            event.preventDefault();
            if (canSubmit && !busy) onSubmit();
          }}
        >
          {children}
          {progress && <p className="text-sm text-muted-foreground" role="status">{progress}</p>}
          {error && <p className="text-sm text-destructive" role="alert">{error}</p>}
          {footer ?? (
            <div className="flex justify-end gap-2 pt-2">
              <button
                type="button"
                onClick={() => onOpenChange(false)}
                disabled={busy}
                className="rounded-lg border border-input bg-background px-4 py-2 text-sm font-medium hover:bg-accent disabled:opacity-50"
              >
                {cancelLabel}
              </button>
              <button
                type="submit"
                disabled={!canSubmit || busy}
                className="rounded-lg bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {busy ? busyLabel : submitLabel}
              </button>
            </div>
          )}
        </form>
      </DialogContent>
    </Dialog>
  );
}
