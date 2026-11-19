/**
 * Deploy the game folder now, or take it down, without launching: what `agora games instance
 * deploy` and `undeploy` do. Useful before running an outside tool against the deployed folder,
 * or to free the disk space a copied deployment uses. A launch deploys and a closed session
 * keeps the game's writes either way, so this is never needed to play.
 */

import { useState } from 'react';
import { Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useConfirm } from '@/components/ui/confirm';
import { deployGameInstance, gamesErrorMessage, undeployGameInstance } from './api';

export function DeploymentActions({ instanceId, running }: { instanceId: string; running: boolean }) {
  const { confirm } = useConfirm();
  const [busy, setBusy] = useState<'deploy' | 'undeploy' | null>(null);
  const [note, setNote] = useState<{ text: string; error: boolean } | null>(null);

  const deploy = async () => {
    setBusy('deploy');
    setNote(null);
    try {
      const done = await deployGameInstance(instanceId);
      setNote({
        text: done.outcome.status === 'up_to_date'
          ? `The ${done.mode} deployment was already up to date.`
          : `Deployed as ${done.mode}.`,
        error: false,
      });
    } catch (failure) {
      setNote({ text: gamesErrorMessage(failure), error: true });
    } finally {
      setBusy(null);
    }
  };

  const undeploy = async () => {
    const ok = await confirm({
      title: 'Take the deployed game folder down?',
      body: 'What the game wrote there is kept in the instance first. The next launch deploys it again.',
      confirmLabel: 'Take it down',
    });
    if (!ok) return;
    setBusy('undeploy');
    setNote(null);
    try {
      const kept = await undeployGameInstance(instanceId);
      const count = kept.copied_to_writable.length;
      setNote({
        text: count === 0
          ? 'Taken down. The game had written nothing new.'
          : `Taken down. Kept ${count} file${count === 1 ? '' : 's'} the game wrote.`,
        error: false,
      });
    } catch (failure) {
      setNote({ text: gamesErrorMessage(failure), error: true });
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-2 border-t border-border pt-3" data-testid="deployment-actions">
      <p className="text-xs text-muted-foreground">
        A launch deploys the game folder for you. Deploy it now to point another tool at it, or take it down to free the space.
      </p>
      <div className="flex flex-wrap gap-2">
        <Button size="sm" variant="outline" disabled={running || busy !== null} onClick={() => void deploy()}>
          {busy === 'deploy' && <Loader2 className="size-4 animate-spin" aria-hidden="true" />}
          Deploy now
        </Button>
        <Button size="sm" variant="outline" disabled={running || busy !== null} onClick={() => void undeploy()}>
          {busy === 'undeploy' && <Loader2 className="size-4 animate-spin" aria-hidden="true" />}
          Take it down
        </Button>
      </div>
      {note && (
        <p role={note.error ? 'alert' : 'status'} className={note.error ? 'text-xs text-destructive' : 'text-xs text-muted-foreground'}>
          {note.text}
        </p>
      )}
    </div>
  );
}
