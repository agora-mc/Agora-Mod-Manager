/**
 * The dialog for a `manual_link` entry: Agora does not download the file itself, so it sends the
 * player to the page and takes the file they choose. The file is copied into the content store;
 * it is never moved, changed or deleted. The no-checksum and wrong-name questions are answered by
 * the same core install, one step at a time, so the CLI and the dialog cannot disagree.
 */

import { useEffect, useState } from 'react';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { formatError, openExternalUrl, pickOpenFile } from '@/lib/tauri';
import { installCatalogEntry, type CatalogEntry, type CatalogInstallReply } from './api';

/** The host a page is on, as the dialog names it. */
export function pageHost(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

type Step =
  | { kind: 'choose' }
  | { kind: 'checksum'; filePath: string; fileName: string }
  | { kind: 'wrong-name'; filePath: string; fileName: string }
  | { kind: 'installing'; filePath: string }
  | { kind: 'done'; reply: CatalogInstallReply }
  | { kind: 'error'; message: string; filePath: string | null };

interface ManualDownloadDialogProps {
  entry: CatalogEntry;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  gameId: string;
  instanceId: string;
  /** Whether "Notice mod downloads" is on, so the dialog can say Agora will spot the file. */
  watching: boolean;
  /** The page-level "Allow changed files" switch, for a curator-pin mismatch. */
  installAnyway: boolean;
  /** Called after an install so the Library can reload. */
  onInstalled: () => void;
}

export function ManualDownloadDialog({
  entry,
  open,
  onOpenChange,
  gameId,
  instanceId,
  watching,
  installAnyway,
  onInstalled,
}: ManualDownloadDialogProps) {
  const [step, setStep] = useState<Step>({ kind: 'choose' });
  useEffect(() => {
    if (open) setStep({ kind: 'choose' });
  }, [open]);

  const pageUrl = entry.pageUrl ?? '';
  const host = pageHost(pageUrl);
  const hint = entry.compatibility.find((compat) => compat.downloadHint)?.downloadHint ?? null;
  const pattern = entry.compatibility.find((compat) => compat.asset)?.asset ?? '';

  const run = async (
    filePath: string,
    options: { installAnyway: boolean; confirmNoChecksum: boolean },
  ) => {
    setStep({ kind: 'installing', filePath });
    try {
      const reply = await installCatalogEntry({
        gameId,
        instanceId,
        entryId: entry.id,
        installAnyway: options.installAnyway,
        allowMissingFrameworks: false,
        dryRun: false,
        filePath,
        confirmNoChecksum: options.confirmNoChecksum,
      });
      if (reply.state === 'needs_checksum') {
        setStep({ kind: 'checksum', filePath, fileName: reply.fileName });
      } else if (reply.state === 'wrong_file_name') {
        setStep({ kind: 'wrong-name', filePath, fileName: reply.fileName });
      } else {
        setStep({ kind: 'done', reply });
        onInstalled();
      }
    } catch (error) {
      setStep({ kind: 'error', message: formatError(error), filePath });
    }
  };

  const choose = async () => {
    const picked = await pickOpenFile('Choose the downloaded file', ['zip', '7z', 'rar']);
    if (!picked) return;
    await run(picked, { installAnyway: false, confirmNoChecksum: false });
  };

  const busy = step.kind === 'installing';

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent data-testid="manual-download-dialog">
        <DialogHeader>
          <DialogTitle>Download {entry.name} yourself</DialogTitle>
          <DialogDescription>
            {entry.name} is on {host}, which Agora doesn&apos;t download from. Open the page, download
            the file, then choose it here.
          </DialogDescription>
        </DialogHeader>

        {hint && <p className="-mt-2 text-sm text-muted-foreground">{hint}</p>}

        <p className="text-sm text-muted-foreground">
          {watching
            ? 'Agora will also spot it in your Downloads folder.'
            : 'Turn on Notice mod downloads in Settings and Agora will spot it for you.'}
        </p>

        {step.kind === 'checksum' && (
          <div className="rounded-lg border border-amber-500/60 bg-amber-500/10 p-3 text-sm" data-testid="manual-no-checksum">
            <p>
              {host} doesn&apos;t publish a checksum, so Agora couldn&apos;t check this file. Only
              install a file you downloaded from the page above.
            </p>
          </div>
        )}

        {step.kind === 'wrong-name' && (
          <div className="rounded-lg border border-amber-500/60 bg-amber-500/10 p-3 text-sm" data-testid="manual-wrong-name">
            <p>
              This doesn&apos;t look like {entry.name}&apos;s file. Agora expected a name like{' '}
              {pattern}.
            </p>
          </div>
        )}

        {step.kind === 'error' && (
          <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive" data-testid="manual-error">
            {step.message}
          </p>
        )}

        {step.kind === 'done' && (
          <p role="status" className="rounded-lg border border-border bg-muted/40 p-3 text-sm" data-testid="manual-installed">
            {step.reply.state === 'already_installed'
              ? `${entry.name} is already in this instance. Nothing changed.`
              : step.reply.state === 'needs_installer'
                ? `${entry.name} is in your library. Its installer has questions to answer.`
                : `Installed ${entry.name}.`}
          </p>
        )}

        <DialogFooter className="gap-2 sm:justify-between">
          <Button
            type="button"
            variant="outline"
            onClick={() => void openExternalUrl(pageUrl)}
            data-testid="manual-open-page"
          >
            Open page
          </Button>
          {step.kind === 'checksum' ? (
            <div className="flex gap-2">
              <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button
                type="button"
                onClick={() => void run(step.filePath, { installAnyway, confirmNoChecksum: true })}
                data-testid="manual-install"
              >
                Install
              </Button>
            </div>
          ) : step.kind === 'wrong-name' ? (
            <div className="flex gap-2">
              <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button
                type="button"
                onClick={() =>
                  void run(step.filePath, { installAnyway: true, confirmNoChecksum: true })
                }
                data-testid="manual-install-anyway"
              >
                Install anyway
              </Button>
            </div>
          ) : step.kind === 'done' ? (
            <Button type="button" onClick={() => onOpenChange(false)}>
              Close
            </Button>
          ) : (
            <div className="flex gap-2">
              <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
                Cancel
              </Button>
              <Button
                type="button"
                onClick={() => void choose()}
                disabled={busy || !pageUrl}
                data-testid="manual-choose-file"
              >
                {busy ? 'Checking…' : 'Choose downloaded file…'}
              </Button>
            </div>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
