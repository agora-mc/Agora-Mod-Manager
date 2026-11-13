/**
 * A game's own executable icon, or `fallback` while it loads and whenever there is none. Used by
 * the sidebar switcher and the game's entry in the Settings rail, so both show the same picture.
 *
 * Minecraft is skipped rather than asked: discovery never finds it, so the command would answer
 * `null` after a full scan for nothing.
 */

import { useEffect, useState, type ReactNode } from 'react';
import { loadGameIcon } from './icons';
import { MINECRAFT_GAME_ID } from './types';

export function GameIcon({
  gameId,
  fallback,
  className = 'h-4 w-4',
}: {
  gameId: string;
  fallback: ReactNode;
  className?: string;
}) {
  const [icon, setIcon] = useState<string | null>(null);
  const [pending, setPending] = useState(true);

  useEffect(() => {
    let cancelled = false;
    setIcon(null);
    setPending(true);
    if (gameId === MINECRAFT_GAME_ID) {
      setPending(false);
      return () => {
        cancelled = true;
      };
    }
    loadGameIcon(gameId)
      .then((dataUrl) => {
        if (cancelled) return;
        setIcon(dataUrl);
        setPending(false);
      })
      .catch(() => {
        // Decoration, not data: a failed lookup keeps the fallback.
        if (!cancelled) {
          setIcon(null);
          setPending(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [gameId]);

  if (!pending && icon) {
    return <img src={icon} alt="" className={`${className} shrink-0 rounded-sm object-contain`} />;
  }
  return <>{fallback}</>;
}
