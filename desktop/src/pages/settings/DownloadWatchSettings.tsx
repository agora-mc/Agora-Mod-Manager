/**
 * "Notice mod downloads": off by default. While it is on, Agora watches one folder and lists the
 * archives it recognises in the Library. It never imports anything on its own.
 */

import { useEffect, useState } from 'react';
import { Download, FolderOpen } from 'lucide-react';

import { Switch } from '../../components/ui/switch';
import { Button } from '../../components/ui/button';
import { defaultDownloadsFolder, scanNewDownloads } from '@/features/downloads/api';
import { formatError, pickDirectory } from '../../lib/tauri';
import { SETTINGS, useTypedSettings } from '../../lib/useTypedSettings';
import { SettingsSection } from './SettingsSection';

export function DownloadWatchSettings() {
  const ts = useTypedSettings();
  const [notice, setNotice] = useState(false);
  const [folder, setFolder] = useState<string | null>(null);
  const [defaultFolder, setDefaultFolder] = useState<string | null>(null);
  const [toast, setToast] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (ts.loading) return;
    setNotice(ts.values.noticeModDownloads === true);
    setFolder((ts.values.modDownloadsFolder as string | null) ?? null);
    setToast(ts.values.downloadsToast !== false);
  }, [ts.loading, ts.values]);

  useEffect(() => {
    let cancelled = false;
    void defaultDownloadsFolder()
      .then((value) => {
        if (!cancelled) setDefaultFolder(value);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  const refreshBaseline = async () => {
    // The watcher takes the folder's current contents as its baseline, so anything already there
    // is not offered later. This is best-effort: the timer does the same on its next pass.
    await scanNewDownloads().catch(() => undefined);
  };

  const toggleNotice = async (value: boolean) => {
    setNotice(value);
    setBusy(true);
    setError(null);
    try {
      await ts.update(SETTINGS.noticeModDownloads, value);
      if (value) await refreshBaseline();
    } catch (failure) {
      setNotice(!value);
      setError(formatError(failure));
    } finally {
      setBusy(false);
    }
  };

  const chooseFolder = async () => {
    const picked = await pickDirectory('Choose the folder to watch for mod downloads');
    if (!picked) return;
    setBusy(true);
    setError(null);
    try {
      await ts.update(SETTINGS.modDownloadsFolder, picked);
      setFolder(picked);
      await refreshBaseline();
    } catch (failure) {
      setError(formatError(failure));
    } finally {
      setBusy(false);
    }
  };

  const useDefaultFolder = async () => {
    setBusy(true);
    setError(null);
    try {
      await ts.update(SETTINGS.modDownloadsFolder, null);
      setFolder(null);
      await refreshBaseline();
    } catch (failure) {
      setError(formatError(failure));
    } finally {
      setBusy(false);
    }
  };

  const toggleToast = async (value: boolean) => {
    setToast(value);
    setError(null);
    try {
      await ts.update(SETTINGS.downloadsToast, value);
    } catch (failure) {
      setToast(!value);
      setError(formatError(failure));
    }
  };

  return (
    <SettingsSection
      card="download-watch"
      id="settings-download-watch"
      icon={Download}
      title="Notice mod downloads"
      description="While Agora is running, it can notice mod archives you download in your browser and list them in your Library. It recognises them by what is inside, never by the file name, and never imports one on its own."
    >
      <div className="space-y-4">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0 text-sm">
            <p className="font-medium">
              {notice ? 'Watching for new mods (on)' : 'Not watching (off)'}
            </p>
            <p className="mt-1 text-xs text-muted-foreground">
              Only common mod archives (.zip, .7z, .rar, .jar, .mrpack) for a game you have turned
              on are offered. Anything else is ignored.
            </p>
          </div>
          <Switch
            checked={notice}
            disabled={busy}
            onCheckedChange={(checked) => void toggleNotice(checked)}
            aria-label="Notice mod downloads"
          />
        </div>

        <div className="space-y-2">
          <p className="text-sm font-medium">Folder</p>
          <p className="break-all rounded-lg border border-border bg-background px-3 py-2 text-xs text-muted-foreground">
            {folder ?? defaultFolder ?? 'Your Downloads folder'}
          </p>
          <div className="flex flex-wrap gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={() => void chooseFolder()}
              disabled={busy}
            >
              <FolderOpen className="mr-2 h-4 w-4" aria-hidden="true" />
              Change folder…
            </Button>
            {folder && (
              <Button
                variant="outline"
                size="sm"
                onClick={() => void useDefaultFolder()}
                disabled={busy}
              >
                Use Downloads
              </Button>
            )}
          </div>
        </div>

        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0 text-sm">
            <p className="font-medium">Show a notification for new downloads</p>
            <p className="mt-1 text-xs text-muted-foreground">
              One quiet message per batch. The downloads still appear in your Library either way.
            </p>
          </div>
          <Switch
            checked={toast}
            disabled={busy}
            onCheckedChange={(checked) => void toggleToast(checked)}
            aria-label="Show a notification for new downloads"
          />
        </div>

        {error && (
          <p role="alert" className="text-xs text-destructive">
            {error}
          </p>
        )}
      </div>
    </SettingsSection>
  );
}
