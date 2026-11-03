/**
 * Settings › (game) › Bases (MASTER_SPEC §26.4): the bases built for this game, each with its
 * checks and its removal, the base records Agora cannot read (with their removal), and a way to
 * build a new one.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { useConfirm } from '@/components/ui/confirm';
import { formatBytes } from '../content/api';
import type { SectionFrame } from '../shared/sectionFrame';
import { isGamesError } from '../shared/types';
import { listBases, removeBase, type BaseSummary, type DamagedBaseSummary } from './api';
import { BaseCard } from './BaseCard';
import { BuildBaseDialog } from './BuildBaseDialog';
import { DamagedBaseCard } from './DamagedBaseCard';
import { DAMAGED_REMOVE_BODY, DAMAGED_TITLE } from './basesText';

type Load =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; bases: BaseSummary[]; damaged: DamagedBaseSummary[] };

/** A backend error shows its own message; anything else is a reply Agora could not read. */
export function errorMessage(error: unknown): string {
  return isGamesError(error) ? error.message : 'Agora could not read the list of bases. Try again.';
}

interface BasesSectionProps {
  gameId: string;
  /** The game's short name, for the line that totals the bases' own disk use. */
  gameName: string;
  /** The card the page puts the section in. */
  frame: SectionFrame;
}

export function BasesSection({ gameId, gameName, frame }: BasesSectionProps) {
  const [load, setLoad] = useState<Load>({ status: 'loading' });
  const [notice, setNotice] = useState<string | null>(null);
  const [buildOpen, setBuildOpen] = useState(false);
  const latest = useRef(0);
  const { confirm } = useConfirm();

  // Only the newest request may answer: a slow list must not overwrite a newer one.
  const refresh = useCallback(() => {
    const request = ++latest.current;
    listBases(gameId)
      .then((list) => {
        if (request === latest.current) setLoad({ status: 'loaded', bases: list.bases, damaged: list.damaged });
      })
      .catch((error: unknown) => {
        if (request === latest.current) setLoad({ status: 'error', message: errorMessage(error) });
      });
  }, [gameId]);

  useEffect(() => {
    setNotice(null);
    setLoad({ status: 'loading' });
    refresh();
  }, [refresh]);

  /** Asks first, then removes. The backend refuses, and names the instances, while any uses the base. */
  async function remove(baseId: string, confirmation: { title: string; body: string }, done: string) {
    const ok = await confirm({
      title: confirmation.title,
      body: confirmation.body,
      confirmLabel: 'Remove base',
      tone: 'danger',
    });
    if (!ok) return;
    try {
      await removeBase(baseId);
      setNotice(done);
      refresh();
    } catch (error: unknown) {
      // The backend names the instances that still use the base; show its message as it is.
      setNotice(isGamesError(error) ? error.message : 'Agora could not remove the base. Try again.');
    }
  }

  function removeBuilt(base: BaseSummary) {
    return remove(
      base.baseId,
      {
        title: `Remove the ${base.store} ${base.version} base?`,
        body: 'Its folder and record are deleted. The game in your store is not touched, and you can build the base again later.',
      },
      `Removed the ${base.store} ${base.version} base.`,
    );
  }

  function removeDamaged(base: DamagedBaseSummary) {
    return remove(
      base.baseId,
      { title: `Remove this ${DAMAGED_TITLE.toLowerCase()}?`, body: DAMAGED_REMOVE_BODY },
      `Removed the ${DAMAGED_TITLE.toLowerCase()}.`,
    );
  }

  const ownBytes = load.status === 'loaded' ? load.bases.reduce((sum, base) => sum + base.copiedBytes, 0) : 0;

  return frame({
    title: 'Bases',
    description: 'A base is a verified copy of one version of the game. Instances built from it keep working when your store updates the game. Bases no instance uses can be removed.',
    actions: (
      <Button type="button" size="sm" className="shrink-0" onClick={() => setBuildOpen(true)} disabled={load.status === 'loading'}>
        Build a base
      </Button>
    ),
    testId: 'bases-section',
    children: (
      <>
        {notice && (
          <p role="status" className="rounded-lg border border-border bg-muted/40 p-3 text-sm">
            {notice}
          </p>
        )}

        {load.status === 'loading' && (
          <p role="status" className="text-sm text-muted-foreground">Loading bases…</p>
        )}

        {load.status === 'error' && (
          <div role="alert" className="space-y-3 rounded-lg border border-destructive/50 p-4 text-sm">
            <p>{load.message}</p>
            <Button type="button" variant="outline" size="sm" onClick={() => {
              setLoad({ status: 'loading' });
              refresh();
            }}
            >
              Try again
            </Button>
          </div>
        )}

        {load.status === 'loaded' && load.damaged.length > 0 && (
          <div className="space-y-3">
            <p className="text-sm text-muted-foreground">
              {load.damaged.length === 1 ? 'One base record' : `${load.damaged.length} base records`} Agora cannot read.
              Each one is listed so you can remove it.
            </p>
            <ul className="space-y-3" aria-label="Damaged base records">
              {load.damaged.map((base) => (
                <li key={`${base.baseId}:${base.manifestPath}`}>
                  <DamagedBaseCard base={base} onRemove={() => removeDamaged(base)} />
                </li>
              ))}
            </ul>
          </div>
        )}

        {load.status === 'loaded' && load.bases.length === 0 && load.damaged.length === 0 && (
          <div className="space-y-2 rounded-lg border border-dashed border-border p-4 text-sm text-muted-foreground" data-testid="bases-empty">
            <p className="font-medium text-foreground">No pinned bases yet.</p>
            <p>
              Build one from an installed copy of the game. Instances pinned to it run from their own verified copy,
              so a store update cannot break them.
            </p>
          </div>
        )}

        {load.status === 'loaded' && load.bases.length > 0 && (
          <p className="text-sm" data-testid="bases-total">
            {load.bases.length} base{load.bases.length === 1 ? '' : 's'} for {gameName}, using{' '}
            <span className="font-medium">{formatBytes(ownBytes)}</span> of their own.
          </p>
        )}

        {load.status === 'loaded' && load.bases.length > 0 && (
          <ul className="space-y-3" aria-label="Pinned bases">
            {load.bases.map((base) => (
              <li key={base.baseId}>
                <BaseCard base={base} onRemove={() => removeBuilt(base)} />
              </li>
            ))}
          </ul>
        )}

        <BuildBaseDialog
          open={buildOpen}
          onOpenChange={setBuildOpen}
          gameId={gameId}
          onBuilt={(message) => {
            setNotice(message);
            refresh();
          }}
        />
      </>
    ),
  });
}
