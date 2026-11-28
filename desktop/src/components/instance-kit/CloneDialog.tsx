/**
 * Clone an instance: a name for the copy, what is copied, and any choices the game offers (as
 * children). The markup and classes are Minecraft's clone dialog (`pages/Instances.tsx`), so every
 * game's Clone is the same dialog. Presentational: `onSubmit` makes the copy.
 */

import { useState, type ReactNode } from 'react';
import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog';

interface CloneDialogProps {
  /** The instance being copied. */
  sourceName: string;
  /** What the copy takes, in a sentence or two. */
  description: ReactNode;
  /** The name field starts with this. Defaults to "Copy of <name>". */
  initialName?: string;
  onClose: () => void;
  /** Make the copy. A rejection keeps the dialog open and shows `errorText(error)`. */
  onSubmit: (name: string) => Promise<void>;
  errorText: (error: unknown) => string;
  /** Choices under the name (a switch for saves, say). */
  children?: ReactNode;
  /** The title. Minecraft's says "Clone Instance". */
  title?: string;
}

export function CloneDialog({
  sourceName,
  description,
  initialName,
  onClose,
  onSubmit,
  errorText,
  children,
  title = 'Clone Instance',
}: CloneDialogProps) {
  const [name, setName] = useState(initialName ?? `Copy of ${sourceName}`);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async () => {
    if (!name.trim()) {
      setError('Enter a name for the clone.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await onSubmit(name.trim());
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(open) => { if (!open) onClose(); }}>
      <DialogContent className="max-w-md">
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>{description}</DialogDescription>
        <div className="space-y-4">
          <label className="block">
            <span className="text-sm font-medium">New instance name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              className="mt-1 w-full rounded-lg border border-input bg-background px-3 py-2 text-sm"
            />
          </label>
          {children}
          {error && <p className="text-xs text-destructive">{error}</p>}
        </div>
        <div className="mt-6 flex justify-end gap-2">
          <button
            onClick={onClose}
            disabled={busy}
            className="rounded-lg border border-input bg-background px-4 py-2 text-sm font-medium hover:bg-accent"
          >
            Cancel
          </button>
          <button
            onClick={submit}
            disabled={busy}
            className="rounded-lg bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            {busy ? 'Cloning…' : 'Clone'}
          </button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
