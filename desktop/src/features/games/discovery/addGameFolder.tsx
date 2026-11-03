/**
 * The one "add a game folder" flow: pick a folder, let core identify and remember it, and hand the
 * accepted folder back to the screen that asked. A game's Installs page, Settings › Game support and More games all
 * use this, so there is one behaviour and one set of words for the same action.
 */

import { useState } from 'react';
import { FolderPlus } from 'lucide-react';
import { Button, type ButtonProps } from '@/components/ui/button';
import { pickDirectory } from '@/lib/tauri';
import { isGamesError } from '../shared/types';
import { addGameFolder, type AddedGameFolder } from './api';

/** The message to show for a failure: the core's own sentence when there is one. */
export function gameErrorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

interface AddGameFolderButtonProps extends Omit<ButtonProps, 'onClick' | 'children' | 'onError'> {
  /** The visible label. The default is the Installs page's. */
  label?: string;
  /** A label for a screen reader when several buttons share the visible label. */
  ariaLabel?: string;
  /** Called with the folder core accepted, after it was remembered. */
  onAdded: (added: AddedGameFolder) => void | Promise<void>;
  /** Called with a player-facing sentence when the pick or the add failed. */
  onError: (message: string) => void;
}

export function AddGameFolderButton({
  label = 'Add a game folder…',
  ariaLabel,
  onAdded,
  onError,
  disabled,
  ...buttonProps
}: AddGameFolderButtonProps) {
  const [busy, setBusy] = useState(false);

  const addFolder = async () => {
    let picked: string | null;
    try {
      picked = await pickDirectory('Choose the folder the game is installed in');
    } catch (caught) {
      onError(gameErrorMessage(caught));
      return;
    }
    if (!picked) return;
    setBusy(true);
    try {
      const added = await addGameFolder(picked);
      await onAdded(added);
    } catch (caught) {
      onError(gameErrorMessage(caught));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Button
      type="button"
      aria-label={ariaLabel}
      onClick={() => void addFolder()}
      disabled={disabled || busy}
      {...buttonProps}
    >
      <FolderPlus aria-hidden="true" />
      {label}
    </Button>
  );
}
