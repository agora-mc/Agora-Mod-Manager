/**
 * One Library item: its saved icon, name, version and description, where it came from, which
 * instances use it, and what it holds (plugin files, an installer). The same card in every game;
 * the game-specific facts are only shown when the item carries them.
 */

import { useEffect, useState } from 'react';
import { Boxes, FileCode2, MoreHorizontal, Wand2 } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { VerifyReport } from '@/features/games/content/api';
import { loadLibraryIcon, type LibraryItem } from './api';
import { formatSize, plural } from './format';

/** How many plugin names a card lists before it says "and N more". */
const PLUGINS_SHOWN = 4;

function LibraryIcon({ item }: { item: LibraryItem }) {
  const [iconSrc, setIconSrc] = useState<string | null>(null);

  // The listing carries only the hash; the bytes are read when this row renders.
  useEffect(() => {
    let cancelled = false;
    setIconSrc(null);
    if (item.iconSha256) {
      void loadLibraryIcon(item.iconSha256).then((src) => {
        if (!cancelled && src) setIconSrc(src);
      });
    }
    return () => {
      cancelled = true;
    };
  }, [item.iconSha256]);

  if (iconSrc) {
    return (
      <img
        src={iconSrc}
        alt=""
        aria-hidden="true"
        className="h-12 w-12 shrink-0 rounded-lg border border-border object-cover"
      />
    );
  }
  return (
    <div
      aria-hidden="true"
      className="flex h-12 w-12 shrink-0 items-center justify-center rounded-lg border border-border bg-muted text-lg font-semibold text-muted-foreground"
    >
      {item.name.trim().charAt(0).toLocaleUpperCase() || '?'}
    </div>
  );
}

const CHIP = 'inline-flex items-center gap-1 rounded-full border border-border px-2 py-0.5 text-xs text-muted-foreground';

/** The host of an item's page, or "its page" when none was recorded. */
function pageHost(url: string | null): string {
  if (!url) return 'its page';
  try {
    return new URL(url).host || 'its page';
  } catch {
    return 'its page';
  }
}

export interface LibraryItemCardProps {
  item: LibraryItem;
  /** The game whose Library shows the card. Other games the item is tied to are named. */
  gameId: string;
  gameLabel: (gameId: string) => string;
  /** No instance can take the item yet; the button explains why. */
  addDisabledReason: string | null;
  busy: boolean;
  report: VerifyReport | undefined;
  onAdd: () => void;
  onCheck: (full: boolean) => void;
  onRemove: () => void;
}

export function LibraryItemCard({
  item,
  gameId,
  gameLabel,
  addDisabledReason,
  busy,
  report,
  onAdd,
  onCheck,
  onRemove,
}: LibraryItemCardProps) {
  const otherGames = item.gameIds.filter((id) => id !== gameId);
  const shownPlugins = item.pluginFiles.slice(0, PLUGINS_SHOWN);
  const morePlugins = item.pluginFiles.length - shownPlugins.length;
  const inUse = item.instances.length > 0;

  return (
    <li
      className="rounded-xl border border-border bg-card p-4 transition-colors hover:border-primary/40"
      data-testid="library-item"
    >
      <div className="flex items-start gap-3">
        <LibraryIcon item={item} />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="text-sm font-semibold">{item.name}</h3>
            {item.version && <span className={CHIP}>{item.version}</span>}
            {item.catalogStrategy === 'manual_link' && (
              <span
                className={CHIP}
                title={`Agora can't check ${pageHost(item.pageUrl)} for new versions. Visit the page to see if there's an update.`}
                data-testid="updates-unknown"
              >
                Updates unknown
              </span>
            )}
            {item.hasInstaller && (
              <span className={CHIP} title="It asks a few questions when you add it to an instance.">
                <Wand2 className="h-3 w-3" aria-hidden="true" />
                Installer
              </span>
            )}
            {item.pluginFiles.length > 0 && (
              <span className={CHIP}>
                <FileCode2 className="h-3 w-3" aria-hidden="true" />
                {plural(item.pluginFiles.length, 'plugin')}
              </span>
            )}
            {otherGames.length > 0 && (
              <span className={CHIP}>Also used for {otherGames.map(gameLabel).join(', ')}</span>
            )}
          </div>
          {item.description ? (
            <p className="mt-1 text-sm">{item.description}</p>
          ) : (
            <p className="mt-1 text-sm italic text-muted-foreground">No description was saved.</p>
          )}
          {shownPlugins.length > 0 && (
            <ul className="mt-2 flex flex-wrap gap-1.5" aria-label="Plugin files">
              {shownPlugins.map((name) => (
                <li key={name} className="rounded-md bg-muted px-1.5 py-0.5 font-mono text-xs text-muted-foreground">
                  {name}
                </li>
              ))}
              {morePlugins > 0 && (
                <li className="px-1 py-0.5 text-xs text-muted-foreground">and {morePlugins} more</li>
              )}
            </ul>
          )}
          <p className="mt-2 text-xs text-muted-foreground">
            {item.sourceLabel}
            <span aria-hidden="true"> · </span>
            {plural(item.fileCount, 'file')}, {formatSize(item.totalSize)}
          </p>
          <p className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
            <Boxes className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
            {inUse
              ? `In ${item.instances.map((instance) => instance.name).join(', ')}.`
              : 'Not in an instance.'}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Button
            variant="outline"
            size="sm"
            onClick={onAdd}
            disabled={busy || addDisabledReason !== null}
            title={addDisabledReason ?? undefined}
          >
            Add to instance…
          </Button>
          <DropdownMenu modal={false}>
            <DropdownMenuTrigger
              className="rounded-md p-2 text-muted-foreground hover:bg-accent disabled:opacity-50"
              aria-label={`More for ${item.name}`}
              title="More"
              disabled={busy}
            >
              <MoreHorizontal className="h-4 w-4" aria-hidden="true" />
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-64">
              <DropdownMenuItem onSelect={() => onCheck(false)}>Check</DropdownMenuItem>
              <DropdownMenuItem onSelect={() => onCheck(true)}>Check every file</DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onSelect={onRemove}
                disabled={inUse}
                className="text-destructive focus:text-destructive"
              >
                {inUse ? 'Remove (an instance uses it)' : 'Remove from library…'}
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>
      {report && (
        <div className="mt-3 rounded-lg border border-border bg-background/40 p-3 text-sm" aria-live="polite">
          {report.problems.length === 0 ? (
            <p>Checked {plural(report.checked, 'file')}. Nothing has changed.</p>
          ) : (
            <div className="space-y-1">
              <p className="font-medium">
                {report.problems.length} problem{report.problems.length === 1 ? '' : 's'} found:
              </p>
              <ul className="list-disc space-y-1 pl-5">
                {report.problems.map((problem) => (
                  <li key={`${problem.kind}:${problem.path}`}>
                    <span className="font-mono text-xs">{problem.path || '(whole item)'}</span>: {problem.message}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      )}
    </li>
  );
}
