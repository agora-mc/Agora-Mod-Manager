/**
 * The Library's "New downloads": mod files Agora recognised in the watched folder, for the game
 * whose Library is open. A mod is added through its game's own path (the FOMOD wizard for one with
 * an installer); a Modrinth modpack opens the "Import pack file" review and becomes a new
 * instance. Nothing here touches the downloaded file, and nothing is added until the player asks.
 */

import { useEffect, useRef, useState } from 'react';
import { Download, FileArchive, Package, Puzzle, Wand2, type LucideIcon } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { usePackInstallOptional } from '@/components/PackInstallProgress';
import { FomodWizard } from '@/features/games/fomod';
import { isGamesError } from '@/features/games/shared/types';
import {
  addDownload,
  downloadsForGame,
  formatDownloadSize,
  ignoreDownload,
  isModpackDownload,
  type NewDownload,
} from './api';
import { useNewDownloads } from './useNewDownloads';

function errorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

/** What the file is, in a player's words, and the picture for it. */
export function downloadKind(kind: string): { label: string; icon: LucideIcon } {
  switch (kind) {
    case 'modpack':
      return { label: 'Modpack', icon: Package };
    case 'FOMOD installer':
      return { label: 'Mod with an installer', icon: Wand2 };
    case 'resource pack':
      return { label: 'Resource pack', icon: FileArchive };
    case 'data pack':
      return { label: 'Data pack', icon: FileArchive };
    case 'shader pack':
      return { label: 'Shader pack', icon: FileArchive };
    default:
      return { label: 'Mod', icon: Puzzle };
  }
}

/** Why Agora thinks the file is a mod, as a short sentence ("It has fabric.mod.json."). */
export function recognisedBecause(reason: string): string {
  const plain = reason.replace(/`/g, '').trim();
  if (!plain) return '';
  const sentence = plain.startsWith('has ') ? `It ${plain}` : plain.charAt(0).toUpperCase() + plain.slice(1);
  return sentence.endsWith('.') ? sentence : `${sentence}.`;
}

export interface NewDownloadsSectionProps {
  /** The game whose Library is open: only its downloads are listed. */
  gameId: string;
  /** Called after an add so the Library's own list reloads. */
  onLibraryChanged?: () => void;
}

export function NewDownloadsSection({ gameId, onLibraryChanged }: NewDownloadsSectionProps) {
  const { rows: allRows, error, reload } = useNewDownloads();
  const packs = usePackInstallOptional();
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [installer, setInstaller] = useState<{ itemId: string; title: string } | null>(null);
  // Modpacks being imported, by the label the import task carries: once one finishes, its
  // download has been used and stops being offered.
  const importing = useRef<Record<string, string>>({});

  const rows = downloadsForGame(allRows, gameId);

  useEffect(() => {
    if (!packs) return;
    for (const task of packs.tasks) {
      if (task.kind !== 'pack-file' || task.status !== 'completed') continue;
      const sha256 = importing.current[task.label];
      if (!sha256) continue;
      delete importing.current[task.label];
      void ignoreDownload(sha256).then(reload).catch(() => undefined);
    }
  }, [packs, reload]);

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
      }
      // The item is stored either way (an installer's choices make a second item later).
      onLibraryChanged?.();
      await reload();
    } catch (failure) {
      setActionError(`Could not add ${row.fileName}: ${errorMessage(failure)}`);
    } finally {
      setBusy(null);
    }
  };

  const importPack = (row: NewDownload) => {
    if (!packs) return;
    setActionError(null);
    setNotice(null);
    importing.current[row.fileName] = row.sha256;
    packs.startPackFile(row.path, row.fileName);
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
    <section
      className="space-y-3 rounded-xl border border-primary/40 bg-primary/5 p-4"
      aria-labelledby="new-downloads-heading"
      data-testid="new-downloads"
    >
      <div className="flex items-start gap-3">
        <span className="settings-hero-badge" aria-hidden="true">
          <Download className="h-5 w-5" />
        </span>
        <div className="min-w-0">
          <h3 id="new-downloads-heading" className="flex items-center gap-2 text-lg font-semibold">
            New downloads
            {rows.length > 0 && <span className="settings-hero-chip text-xs">{rows.length}</span>}
          </h3>
          <p className="text-sm text-muted-foreground">
            Mods Agora spotted in your downloads. Nothing is added until you choose.
          </p>
        </div>
      </div>

      {error && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          New downloads could not be read: {error}
        </p>
      )}
      {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
      {actionError && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          {actionError}
        </p>
      )}

      {rows.length > 0 && (
        <ul className="space-y-2">
          {rows.map((row) => {
            const kind = downloadKind(row.kind);
            const KindIcon = kind.icon;
            const modpack = isModpackDownload(row);
            return (
              <li
                key={row.sha256}
                className="flex flex-col gap-3 rounded-lg border border-border bg-card p-3 sm:flex-row sm:items-center"
                data-testid="new-download"
              >
                <div className="flex min-w-0 flex-1 items-start gap-3">
                  <span
                    aria-hidden="true"
                    className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg border border-border bg-muted text-muted-foreground"
                  >
                    <KindIcon className="h-5 w-5" />
                  </span>
                  <div className="min-w-0">
                    <p className="truncate text-sm font-semibold" title={row.fileName}>{row.fileName}</p>
                    <p className="text-xs text-muted-foreground">
                      {kind.label}
                      <span aria-hidden="true"> · </span>
                      {formatDownloadSize(row.size)}
                      {row.nexusModId && (
                        <>
                          <span aria-hidden="true"> · </span>
                          Nexus mod {row.nexusModId}
                        </>
                      )}
                    </p>
                    <p className="text-xs text-muted-foreground">{recognisedBecause(row.reason)}</p>
                  </div>
                </div>
                <div className="flex shrink-0 flex-wrap items-center gap-2 sm:justify-end">
                  {row.alreadyInLibrary ? (
                    <span className="settings-hero-chip text-xs">Already in your library</span>
                  ) : modpack ? (
                    <Button size="sm" onClick={() => importPack(row)} disabled={busy !== null || !packs}>
                      Import as new instance…
                    </Button>
                  ) : (
                    <Button size="sm" onClick={() => void add(row)} disabled={busy !== null}>
                      {busy === row.sha256 ? 'Adding…' : 'Add to library'}
                    </Button>
                  )}
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => void ignore(row)}
                    disabled={busy !== null}
                    title="Stop showing this file"
                  >
                    Ignore
                  </Button>
                </div>
              </li>
            );
          })}
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
