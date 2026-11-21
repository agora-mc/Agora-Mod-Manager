/**
 * The instance actions a card and an instance's header share: Stop (asks first), Rename (asks
 * for the name) and Clone (a dialog). Each says what happened in a sentence, or nothing when the
 * player changed their mind.
 */

import { useState } from 'react';
import { CloneDialog } from '@/components/instance-kit';
import { useConfirm } from '@/components/ui/confirm';
import { Switch } from '@/components/ui/switch';
import { formatPlayTime } from '@/lib/utils';
import {
  cloneGameInstance,
  gamesErrorCode,
  gamesErrorMessage,
  renameGameInstance,
  stopGameInstance,
  type ClonedInstance,
} from './api';

export type ActionOutcome =
  | { status: 'done'; message: string }
  | { status: 'cancelled' }
  | { status: 'failed'; message: string };

/** "7.3 h played", or null under a minute (as Minecraft's cards say it). */
export function playedText(playTimeMs: number): string | null {
  const time = formatPlayTime(playTimeMs);
  return time === null ? null : `${time} played`;
}

/**
 * Stop: asks first, because the game closes without saving. `onStopping` runs once the player
 * agrees, so a button shows "Stopping…" only while the stop is really under way.
 */
export function useStopGameInstance(): (instanceId: string, name: string, onStopping?: () => void) => Promise<ActionOutcome> {
  const { confirm } = useConfirm();
  return async (instanceId, name, onStopping) => {
    const approved = await confirm({
      title: `Stop ${name}?`,
      body: 'The game closes right away. Anything you have not saved in the game is lost.',
      confirmLabel: 'Stop the game',
      tone: 'danger',
    });
    if (!approved) return { status: 'cancelled' };
    onStopping?.();
    try {
      await stopGameInstance(instanceId);
      return { status: 'done', message: `${name} was stopped.` };
    } catch (failure) {
      const code = gamesErrorCode(failure);
      if (code === 'instance_not_running') return { status: 'done', message: `${name} had already closed.` };
      if (code === 'instance_not_agora_process') {
        return { status: 'failed', message: `Agora did not start this game, so it cannot stop it. Close it from the game.` };
      }
      return { status: 'failed', message: `${name} was not stopped. ${gamesErrorMessage(failure)}` };
    }
  };
}

/** Rename: asks for the new name in Minecraft's prompt. */
export function useRenameGameInstance(): (instanceId: string, name: string) => Promise<ActionOutcome> {
  const { prompt } = useConfirm();
  return async (instanceId, name) => {
    const next = await prompt({ title: 'Rename instance', initialValue: name, confirmLabel: 'Rename' });
    if (next === null || next.trim() === '' || next.trim() === name) return { status: 'cancelled' };
    try {
      const renamed = await renameGameInstance(instanceId, next.trim());
      return { status: 'done', message: `Renamed to "${renamed.name}".` };
    } catch (failure) {
      return { status: 'failed', message: `The name was not changed. ${gamesErrorMessage(failure)}` };
    }
  };
}

/** Minecraft's Clone dialog, with this game's one choice: whether the saves come along. */
export function GameCloneDialog({
  instanceId,
  name,
  onClose,
  onCloned,
}: {
  instanceId: string;
  name: string;
  onClose: () => void;
  onCloned: (clone: ClonedInstance) => void;
}) {
  const [copySaves, setCopySaves] = useState(false);
  return (
    <CloneDialog
      sourceName={name}
      description={`Makes a copy of "${name}" with its mods, load order and game settings. The original is not changed.`}
      onClose={onClose}
      errorText={gamesErrorMessage}
      onSubmit={async (cloneName) => {
        onCloned(await cloneGameInstance(instanceId, cloneName, copySaves));
      }}
    >
      <label className="flex items-start justify-between gap-4 rounded-lg border border-border p-3">
        <span className="min-w-0">
          <span className="block text-sm font-medium">Also copy saves</span>
          <span className="mt-0.5 block text-xs text-muted-foreground">
            Off: the copy starts with no saves of its own.
          </span>
        </span>
        <Switch checked={copySaves} onCheckedChange={setCopySaves} aria-label="Also copy saves" />
      </label>
    </CloneDialog>
  );
}
