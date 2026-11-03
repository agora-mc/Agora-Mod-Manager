import { useCallback, useEffect, useRef, useState } from 'react';
import { Boxes, Download, Plus } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useConfirm } from '@/components/ui/confirm';
import { EmptyState, InstanceGrid, PageHero, PageHeroAction } from '@/components/instance-kit';
import { GAME_IMPORT_REQUEST_EVENT, peekGameImportRequest, takeGameImportRequest } from '@/lib/gameImportRequest';
import { GameImportDialog } from '../import';
import { CreateInstanceDialog } from './CreateInstanceDialog';
import { GameInstanceCard } from './GameInstanceCard';
import { DEFAULT_RETENTION_DAYS, GameRecentlyDeleted } from './GameRecentlyDeleted';
import { GameCloneDialog, useStopGameInstance } from './instanceActions';
import { daysText } from '@/components/instance-kit';
import {
  deleteGameInstance,
  gamesErrorCode,
  gamesErrorMessage,
  listGameInstances,
  onInstanceExited,
  type DeletedInstance,
  type InstanceCard,
} from './api';

interface GameInstancesPageProps {
  gameId: string;
  /** Open an instance's page. The shell owns the navigation; the slice only calls this. */
  onOpenInstance: (instanceId: string) => void;
}

type LoadState =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; instances: InstanceCard[] };

/**
 * The Instances tab of a non-Minecraft game: the same page as Minecraft's "My Instances" (hero,
 * card grid, empty state, Recently deleted), with this game's cards. Each card carries the
 * actions its instance allows: Launch or Stop, Edit, Clone and Delete.
 */
export function GameInstancesPage({ gameId, onOpenInstance }: GameInstancesPageProps) {
  const { confirm } = useConfirm();
  const [state, setState] = useState<LoadState>({ status: 'loading' });
  const [createOpen, setCreateOpen] = useState(false);
  // An old link to the retired Import page lands here with the import dialog open.
  const [importOpen, setImportOpen] = useState(() => peekGameImportRequest(gameId));
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [stoppingId, setStoppingId] = useState<string | null>(null);
  const [cloneTarget, setCloneTarget] = useState<InstanceCard | null>(null);
  const [retentionDays, setRetentionDays] = useState(DEFAULT_RETENTION_DAYS);
  const stopInstance = useStopGameInstance();
  // Only the newest request may answer: a slow list must not overwrite a newer one.
  const latest = useRef(0);

  const load = useCallback(() => {
    const request = ++latest.current;
    setState({ status: 'loading' });
    listGameInstances(gameId)
      .then((list) => {
        if (request === latest.current) setState({ status: 'loaded', instances: list.instances });
      })
      .catch((error: unknown) => {
        if (request === latest.current) setState({ status: 'failed', message: gamesErrorMessage(error) });
      });
  }, [gameId]);

  // Read the list again without the loading skeleton: after a launch, or when a game closes.
  const refresh = useCallback(() => {
    const request = ++latest.current;
    listGameInstances(gameId)
      .then((list) => {
        if (request === latest.current) setState({ status: 'loaded', instances: list.instances });
      })
      .catch(() => undefined);
  }, [gameId]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    const take = () => {
      if (takeGameImportRequest(gameId)) setImportOpen(true);
    };
    take();
    window.addEventListener(GAME_IMPORT_REQUEST_EVENT, take);
    return () => window.removeEventListener(GAME_IMPORT_REQUEST_EVENT, take);
  }, [gameId]);

  // A game that closes turns its card back from Running.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    onInstanceExited(() => refresh())
      .then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [refresh]);

  const remove = async (card: InstanceCard) => {
    const restorable = card.capabilities.trash;
    const approved = await confirm({
      title: `Delete ${card.name}?`,
      body: restorable
        ? `It moves to Recently deleted, below the cards, where you can restore it for ${daysText(retentionDays)}. The game install and the pinned base are kept.`
        : 'This removes the instance and its folder for good. The game install and the pinned base are kept.',
      confirmLabel: 'Delete instance',
      tone: 'danger',
    });
    if (!approved) return;
    setBusyId(card.instanceId);
    setActionError(null);
    setNotice(null);
    try {
      let deleted: DeletedInstance;
      try {
        deleted = await deleteGameInstance(card.instanceId);
      } catch (error) {
        if (gamesErrorCode(error) !== 'instance_base_unreadable') throw error;
        const forced = await confirm({
          title: `Delete ${card.name} anyway?`,
          body: `Agora cannot read this instance's base record, so it cannot check the game folder it deployed. The instance and its own folder will be deleted. A deployed game folder Agora cannot prove is this instance's will be left where it is, and Agora will say where.`,
          confirmLabel: 'Delete instance anyway',
          tone: 'danger',
        });
        if (!forced) return;
        deleted = await deleteGameInstance(card.instanceId, true);
      }
      const left = deleted.leftInPlace === null
        ? ''
        : ` A deployed game folder Agora could not check was left at ${deleted.leftInPlace}.`;
      const done = deleted.trashEntryId === null
        ? `${card.name} was deleted.`
        : `${card.name} was moved to Recently deleted.`;
      setNotice(
        deleted.orphanedBase === null
          ? `${done}${left}`
          : `${done} The base ${deleted.orphanedBase} is no longer used by any instance. Remove it in Settings, on this game's Bases page, if you want the disk space back.${left}`,
      );
      load();
    } catch (error) {
      setActionError(`${card.name} was not deleted. ${gamesErrorMessage(error)}`);
    } finally {
      setBusyId(null);
    }
  };

  const stop = async (card: InstanceCard) => {
    setActionError(null);
    setNotice(null);
    try {
      const outcome = await stopInstance(card.instanceId, card.name, () => setStoppingId(card.instanceId));
      if (outcome.status === 'done') setNotice(outcome.message);
      if (outcome.status === 'failed') setActionError(outcome.message);
      if (outcome.status !== 'cancelled') refresh();
    } finally {
      setStoppingId(null);
    }
  };

  const instances = state.status === 'loaded' ? state.instances : [];
  const running = instances.find((card) => card.running) ?? null;
  const importAction = (
    <PageHeroAction icon={Download} onClick={() => setImportOpen(true)}>
      Import instances
    </PageHeroAction>
  );

  return (
    <section className="space-y-6" data-testid="game-instances-tab">
      <PageHero
        icon={Boxes}
        title="Instances"
        description="Each instance is its own setup of this game: its mods, its load order and its saves."
        chips={state.status === 'loaded' && instances.length > 0
          ? [
              `${instances.length} instance${instances.length === 1 ? '' : 's'}`,
              running === null ? null : `● ${running.name} running`,
            ]
          : []}
        actions={
          <>
            {importAction}
            <PageHeroAction primary icon={Plus} onClick={() => setCreateOpen(true)} disabled={state.status !== 'loaded'}>
              New instance
            </PageHeroAction>
          </>
        }
      />

      {notice !== null && (
        <p role="status" className="rounded-lg border border-border bg-card p-3 text-sm">
          {notice}
        </p>
      )}
      {actionError !== null && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
          {actionError}
        </p>
      )}

      {state.status === 'failed' ? (
        <div role="alert" className="space-y-3 rounded-xl border border-destructive/50 bg-destructive/10 p-6">
          <h3 className="font-semibold">The instances could not be loaded</h3>
          <p className="text-sm">{state.message}</p>
          <Button type="button" variant="outline" onClick={load}>Try again</Button>
        </div>
      ) : (
        <InstanceGrid
          loading={state.status === 'loading'}
          label="Instances"
          groups={[{
            id: '',
            label: 'Instances',
            count: instances.length,
            cards: instances.map((card) => (
              <GameInstanceCard
                key={card.instanceId}
                card={card}
                onOpen={() => onOpenInstance(card.instanceId)}
                onDelete={() => void remove(card)}
                onClone={() => setCloneTarget(card)}
                onStop={() => void stop(card)}
                stopping={stoppingId === card.instanceId}
                deleting={busyId === card.instanceId}
                locked={busyId !== null}
                onLaunchSettled={refresh}
              />
            )),
          }]}
          empty={
            <EmptyState
              testId="instances-empty"
              icon={Boxes}
              title="No instances yet"
              body="An instance starts from an install of this game that Agora found on this machine. Create one, then add mods to it."
              actions={
                <>
                  <PageHeroAction primary icon={Plus} onClick={() => setCreateOpen(true)}>
                    Create the first instance
                  </PageHeroAction>
                  <PageHeroAction icon={Download} onClick={() => setImportOpen(true)}>
                    Import from Mod Organizer 2
                  </PageHeroAction>
                </>
              }
            />
          }
        />
      )}

      {state.status !== 'failed' && (
        <GameRecentlyDeleted
          gameId={gameId}
          refreshKey={state}
          onRestored={refresh}
          onRetentionDays={setRetentionDays}
        />
      )}

      {cloneTarget !== null && (
        <GameCloneDialog
          instanceId={cloneTarget.instanceId}
          name={cloneTarget.name}
          onClose={() => setCloneTarget(null)}
          onCloned={(clone) => {
            setCloneTarget(null);
            setActionError(null);
            setNotice(`Made ${clone.name}, a copy of ${cloneTarget.name}.`);
            refresh();
          }}
        />
      )}

      <GameImportDialog
        gameId={gameId}
        open={importOpen}
        onOpenChange={setImportOpen}
        onImported={refresh}
        onOpenInstance={onOpenInstance}
      />

      <CreateInstanceDialog
        open={createOpen}
        gameId={gameId}
        onOpenChange={setCreateOpen}
        onCreated={(instanceId) => {
          setCreateOpen(false);
          onOpenInstance(instanceId);
        }}
      />
    </section>
  );
}
