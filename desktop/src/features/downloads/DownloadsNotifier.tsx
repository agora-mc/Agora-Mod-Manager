import { useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';

import { showToast } from '@/components/Toast';
import { getSetting } from '@/lib/tauri';
import { SETTINGS } from '@/lib/useTypedSettings';

/**
 * One quiet toast per batch of new downloads, unless the player turned it off. It renders nothing:
 * the rows themselves live in the Library's "New downloads" section.
 */
export function DownloadsNotifier() {
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void listen<number>('downloads-changed', (event) => {
      if (!Number.isFinite(event.payload) || event.payload <= 0) return;
      void getSetting(SETTINGS.downloadsToast.key)
        .then((raw) => {
          if (SETTINGS.downloadsToast.parse(raw)) {
            showToast('New mods are waiting in your Library.');
          }
        })
        .catch(() => undefined);
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
  }, []);
  return null;
}
