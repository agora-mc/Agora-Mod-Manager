import { useCallback, useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';

import { listNewDownloads, type NewDownload } from './api';

/** The downloads Agora has recognised. Reloads when the background watcher finds more. */
export function useNewDownloads(): {
  rows: NewDownload[];
  count: number;
  error: string | null;
  reload: () => Promise<void>;
} {
  const [rows, setRows] = useState<NewDownload[]>([]);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      setRows(await listNewDownloads());
      setError(null);
    } catch (failure) {
      // The shell keeps working when this command fails; the Library section says what happened.
      setError(failure instanceof Error ? failure.message : 'Could not read new downloads.');
    }
  }, []);

  useEffect(() => {
    void reload();
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void listen('downloads-changed', () => {
      void reload();
    })
      .then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [reload]);

  return { rows, count: rows.length, error, reload };
}
