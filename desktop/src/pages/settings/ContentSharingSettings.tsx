import { useCallback, useEffect, useState } from 'react';
import { Link2 } from 'lucide-react';
import { SettingsSection } from './SettingsSection';
import { Switch } from '../../components/ui/switch';
import { useConfirm } from '@/components/ui/confirm';
import { formatBytes } from '../../components/installed-content/contentTableState';
import { showToast } from '../../components/Toast';
import {
  formatError,
  getContentSharing,
  previewContentSharingConversion,
  setContentSharing,
  type ContentPlacementMode,
} from '../../lib/tauri';

type Load =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; mode: ContentPlacementMode };

function conversionSentence(
  share: boolean,
  files: number,
  instances: number,
  bytes: number,
): string {
  const fileText = `${files} file${files === 1 ? '' : 's'}`;
  const instanceText = `${instances} instance${instances === 1 ? '' : 's'}`;
  const space = bytes > 0 ? ` This ${share ? 'frees' : 'uses'} about ${formatBytes(bytes)}.` : '';
  return `${fileText} across ${instanceText} can be converted.${space}`;
}

/**
 * The "Share mod files between instances" choice, with the optional one-time conversion.
 *
 * Sharing is the default: it saves disk space, at the cost of mod files that cannot be edited in
 * place. Turning it off gives every instance its own editable copy. Converting existing instances
 * is always offered and never done on its own.
 */
export function ContentSharingSettings() {
  const { confirm } = useConfirm();
  const [load, setLoad] = useState<Load>({ status: 'loading' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    setLoad({ status: 'loading' });
    getContentSharing()
      .then((mode) => {
        if (mode !== 'link' && mode !== 'copy') {
          throw new Error('get_content_sharing did not return link or copy.');
        }
        setLoad({ status: 'loaded', mode });
      })
      .catch((reason: unknown) => setLoad({ status: 'failed', message: formatError(reason) }));
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  const change = async (share: boolean) => {
    setError(null);
    setBusy(true);
    try {
      const preview = await previewContentSharingConversion(share);
      const { filesConverted, bytes } = preview.summary;
      let convert = false;
      if (filesConverted > 0) {
        convert = await confirm({
          title: 'Also convert my existing instances?',
          body: `${conversionSentence(share, filesConverted, preview.instances, bytes)} This never deletes a file; anything you changed by hand is left as it is.`,
          confirmLabel: 'Convert',
          cancelLabel: 'Not now',
        });
      }
      const report = await setContentSharing(share, convert);
      setLoad({ status: 'loaded', mode: share ? 'link' : 'copy' });
      if (report) {
        const converted = report.summary.filesConverted;
        const skipped = report.summary.skipped.length;
        const failed = report.failures.length;
        if (failed > 0) {
          setError(
            `Converted ${converted} file${converted === 1 ? '' : 's'}, but ${failed} instance${failed === 1 ? '' : 's'} could not be converted. ${report.failures.map((failure) => failure.message).join(' ')}`,
          );
        } else if (skipped > 0) {
          showToast(
            `Converted ${converted} file${converted === 1 ? '' : 's'}. ${skipped} file${skipped === 1 ? ' was' : 's were'} left as ${skipped === 1 ? 'it is' : 'they are'} because ${skipped === 1 ? 'it does' : 'they do'} not match what Agora installed.`,
            'success',
          );
        } else {
          showToast(
            `Converted ${converted} file${converted === 1 ? '' : 's'}.`,
            'success',
          );
        }
      }
    } catch (reason) {
      setError(formatError(reason));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SettingsSection
      card="content-sharing"
      id="settings-content-sharing"
      icon={Link2}
      title="Share mod files between instances"
      description="On, one copy is shared by every instance: it saves space, but a mod file cannot be edited in place. Off, each instance keeps its own editable copy and uses more space."
    >
      {load.status === 'loading' && <p className="text-xs text-muted-foreground">Loading…</p>}
      {load.status === 'failed' && (
        <div className="space-y-2">
          <p role="alert" className="text-xs text-destructive">
            Agora could not read the sharing setting, so it will not guess. {load.message}
          </p>
          <button type="button" onClick={reload} className="text-xs text-primary hover:underline">
            Try again
          </button>
        </div>
      )}
      {load.status === 'loaded' && (
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0 text-sm">
            <p className="font-medium">
              {load.mode === 'link' ? 'Shared files (on)' : 'Separate copies (off)'}
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              {load.mode === 'link'
                ? 'New content is a hard link to one stored copy, so two instances never use twice the space.'
                : 'New content is copied into each instance, so every file can be changed there.'}
            </p>
          </div>
          <Switch
            checked={load.mode === 'link'}
            disabled={busy}
            onCheckedChange={(checked) => void change(checked)}
            aria-label="Share mod files between instances"
          />
        </div>
      )}
      {error && <p role="alert" className="text-xs text-destructive">{error}</p>}
    </SettingsSection>
  );
}
