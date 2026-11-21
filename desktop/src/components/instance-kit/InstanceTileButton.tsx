/**
 * An instance's tile as a button that sets its image, with the image icon showing over it on
 * hover and focus. Minecraft's instance header (`pages/InstanceEditor.tsx`) markup and classes.
 */

import type { ReactNode } from 'react';
import { ImagePlus } from 'lucide-react';

interface InstanceTileButtonProps {
  /** The tile (`InstanceIcon`). */
  children: ReactNode;
  /** The instance already has its own image: the button changes it rather than sets it. */
  hasImage: boolean;
  onSelect: () => void;
  disabled?: boolean;
}

export function InstanceTileButton({ children, hasImage, onSelect, disabled = false }: InstanceTileButtonProps) {
  return (
    <button
      type="button"
      onClick={onSelect}
      disabled={disabled}
      className="instance-icon-button shrink-0"
      title={hasImage ? 'Change instance image' : 'Set a custom instance image'}
      aria-label={hasImage ? 'Change instance image' : 'Set instance image'}
    >
      {children}
      <span className="instance-icon-button__overlay">
        <ImagePlus className="h-5 w-5" aria-hidden="true" />
      </span>
    </button>
  );
}

/** The small "Set image" button at the end of the header's facts row. */
export function SetImageButton({ onSelect, disabled = false }: { onSelect: () => void; disabled?: boolean }) {
  return (
    <button
      type="button"
      onClick={onSelect}
      disabled={disabled}
      className="inline-flex items-center gap-1 rounded-lg border border-input bg-background px-2.5 py-1 text-xs font-medium hover:bg-accent disabled:cursor-not-allowed disabled:opacity-50"
      title="Set a custom instance image"
    >
      <ImagePlus className="h-3.5 w-3.5" aria-hidden="true" />
      Set image
    </button>
  );
}
