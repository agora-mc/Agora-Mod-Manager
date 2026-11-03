/**
 * "Import instances" on a game's Instances page: the Mod Organizer 2 import in a large dialog,
 * framed as Minecraft's "Import Instances" wizard is (a header bar, a scrolling body, a footer).
 *
 * The steps are `Mo2ImportFlow`'s; this only frames them, keeps the dialog open while files are
 * being copied, and offers the new instance once the import is done.
 */

import { useEffect, useRef, useState } from 'react';
import { ArrowRight, Check, Download } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog';
import { Mo2ImportFlow, type ImportFlowState, type ImportStep } from './Mo2ImportFlow';

const STEPS: { id: ImportStep; label: string }[] = [
  { id: 'setup', label: 'Choose a setup' },
  { id: 'profile', label: 'Choose a profile' },
  { id: 'preview', label: 'Preview' },
  { id: 'done', label: 'Imported' },
];

const INITIAL: ImportFlowState = { step: 'setup', running: false, report: null };

interface GameImportDialogProps {
  gameId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** An import finished: the Instances page reads its list again. */
  onImported: () => void;
  /** Open the instance the import made, closing the dialog. */
  onOpenInstance: (instanceId: string) => void;
}

export function GameImportDialog({ gameId, open, onOpenChange, onImported, onOpenInstance }: GameImportDialogProps) {
  const [state, setState] = useState<ImportFlowState>(INITIAL);
  const importedRef = useRef(onImported);
  importedRef.current = onImported;

  // A finished import is a new instance on the page behind the dialog.
  useEffect(() => {
    if (state.report !== null) importedRef.current();
  }, [state.report]);

  const close = (next: boolean) => {
    if (next) {
      onOpenChange(true);
      return;
    }
    // Copying cannot be stopped half way, so the dialog stays until it finishes.
    if (state.running) return;
    setState(INITIAL);
    onOpenChange(false);
  };

  const at = STEPS.findIndex((entry) => entry.id === state.step);
  const newInstance = state.report?.instanceId ?? null;

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogContent
        className="flex max-h-[85vh] max-w-4xl flex-col gap-0 p-0"
        data-testid="game-import-dialog"
        onEscapeKeyDown={state.running ? (event) => event.preventDefault() : undefined}
        onInteractOutside={state.running ? (event) => event.preventDefault() : undefined}
      >
        <div className="shrink-0 space-y-3 border-b border-border px-6 pb-4 pt-6">
          <div className="flex items-start gap-3 pr-8">
            <span className="settings-card-icon" aria-hidden="true">
              <Download className="h-4 w-4" />
            </span>
            <div className="min-w-0 space-y-1">
              <DialogTitle className="text-base font-semibold">Import from Mod Organizer 2</DialogTitle>
              <DialogDescription>
                Bring a Mod Organizer 2 profile in as a new instance. Agora only reads the setup: every mod is copied in
                as it is on disk, and the profile’s load order, plugins, INIs and saves are set on the new instance.
              </DialogDescription>
            </div>
          </div>
          <ol className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs" aria-label="Import steps">
            {STEPS.map((entry, index) => {
              const done = index < at;
              const current = index === at;
              return (
                <li key={entry.id} className="flex items-center gap-2" aria-current={current ? 'step' : undefined}>
                  {index > 0 && <span className="h-px w-4 bg-border" aria-hidden="true" />}
                  <span
                    className={[
                      'inline-flex h-5 w-5 items-center justify-center rounded-full border text-[11px] font-semibold',
                      current ? 'border-primary bg-primary text-primary-foreground' : '',
                      done ? 'border-primary/60 bg-primary/15 text-primary' : '',
                      !current && !done ? 'border-border text-muted-foreground' : '',
                    ].join(' ')}
                    aria-hidden="true"
                  >
                    {done ? <Check className="h-3 w-3" /> : index + 1}
                  </span>
                  <span className={current ? 'font-medium text-foreground' : 'text-muted-foreground'}>{entry.label}</span>
                </li>
              );
            })}
          </ol>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
          <Mo2ImportFlow gameId={gameId} onStateChange={setState} />
        </div>

        <div className="flex shrink-0 flex-wrap items-center justify-end gap-2 border-t border-border px-6 py-3">
          {state.running && (
            <p className="mr-auto text-xs text-muted-foreground">Keep Agora open until the import finishes.</p>
          )}
          <Button type="button" variant="outline" size="sm" onClick={() => close(false)} disabled={state.running}>
            {state.report ? 'Done' : 'Cancel'}
          </Button>
          {newInstance !== null && (
            <Button
              type="button"
              size="sm"
              onClick={() => {
                setState(INITIAL);
                onOpenChange(false);
                onOpenInstance(newInstance);
              }}
            >
              Open “{state.report?.instanceName}”
              <ArrowRight aria-hidden="true" />
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
