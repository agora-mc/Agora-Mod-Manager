/**
 * The Library's quiet "New downloads" section: archives Agora recognised in the watched folder.
 * Adding goes through the game's own flow (the FOMOD wizard for installers, the normal Minecraft
 * library item for jars and packs). Nothing here touches the downloaded file.
 */

import { useState } from 'react';

import { Button } from '@/components/ui/button';
import { FomodWizard } from '@/features/games/fomod';
import { isGamesError } from '@/features/games/shared/types';
import {
  addDownload,
  formatDownloadSize,
  ignoreDownload,
  type NewDownload,
} from './api';
import { useNewDownloads } from './useNewDownloads';

function errorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

function gameLabel(gameId: string): string {
  return gameId === 'minecraft' ? 'Minecraft' : gameId;
}

export interface NewDownloadsSectionProps {
  /** Called after an add so the Library's own list reloads. */
  onLibraryChanged?: () => void;
}

export function NewDownloadsSection({ onLibraryChanged }: NewDownloadsSectionProps) {
  const { rows, error, reload } = useNewDownloads();
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [installer, setInstaller] = useState<{ itemId: string; title: string } | null>(null);

  if (rows.length === 0 && !error && !actionError && !notice && !installer) return null;

  const add = async (row: NewDownload) => {
    setBusy(row.sha256);
    setActionError(null);
    setNotice(null);
    try {
      const result = await addDownload(row.sha256);
      if (result.status === 'needs_installer') {
        setInstaller({ itemId: result.itemId, title: result.title });
      } else {
        setNotice(`Added ${row.fileName} to your library.`);
        onLibraryChanged?.();
      }
      await reload();
    } catch (failure) {
      setActionError(errorMessage(failure));
    } finally {
      setBusy(null);
    }
  };

  const ignore = async (row: NewDownload) => {
    setBusy(row.sha256);
    setActionError(null);
    setNotice(null);
    try {
      await ignoreDownload(row.sha256);
      await reload();
    } catch (failure) {
      setActionError(errorMessage(failure));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="space-y-3" data-testid="new-downloads">
      <div>
        <h3 className="text-sm font-semibold">New downloads</h3>
        <p className="text-xs text-muted-foreground">
          Mods Agora noticed in your downloads folder. Nothing is imported until you add it.
        </p>
      </div>

      {error && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          {error}
        </p>
      )}
      {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
      {actionError && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          {actionError}
        </p>
      )}

      {rows.length > 0 && (
        <ul className="space-y-3">
          {rows.map((row) => (
            <li
              key={row.sha256}
              className="rounded-xl border border-border bg-card p-4"
              data-testid="new-download"
            >
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-baseline gap-2">
                    <h4 className="break-all text-sm font-semibold">{row.fileName}</h4>
                    <span className="text-xs text-muted-foreground">
                      {formatDownloadSize(row.size)}
                    </span>
                  </div>
                  <p className="mt-1 text-xs text-muted-foreground">
                    {gameLabel(row.gameId)} · {row.kind} · {row.reason}
                  </p>
                  {row.nexusModId && (
                    <p className="mt-1 text-xs text-muted-foreground">
                      Nexus mod {row.nexusModId}
                    </p>
                  )}
                  {row.alreadyInLibrary && (
                    <p className="mt-1 text-xs font-medium text-muted-foreground">
                      Already in your library.
                    </p>
                  )}
                </div>
                <div className="flex shrink-0 gap-2">
                  {!row.alreadyInLibrary && (
                    <Button
                      size="sm"
                      onClick={() => void add(row)}
                      disabled={busy !== null}
                    >
                      {busy === row.sha256 ? 'Adding…' : 'Add to library'}
                    </Button>
                  )}
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => void ignore(row)}
                    disabled={busy !== null}
                  >
                    Ignore
                  </Button>
                </div>
              </div>
            </li>
          ))}
        </ul>
      )}

      {installer && (
        <FomodWizard
          contentId={installer.itemId}
          onClose={() => setInstaller(null)}
          onInstalled={() => {
            setInstaller(null);
            setNotice(`Added ${installer.title} to your library.`);
            onLibraryChanged?.();
            void reload();
          }}
        />
      )}
    </section>
  );
}
