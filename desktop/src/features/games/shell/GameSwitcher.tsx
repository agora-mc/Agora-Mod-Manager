/**
 * The game switcher at the top of the sidebar. Minecraft plus each non-Minecraft game found.
 * It is rendered only when there is another game to switch to, so a Minecraft-only user never
 * sees it. Every choice is a `<button>` so the controller can reach it.
 */

import { useEffect, useState, type ReactNode } from 'react';
import { ChevronDown, ChevronUp, Gamepad2 } from 'lucide-react';
import { loadGameIcon } from '../shared/icons';
import { MINECRAFT_GAME_ID } from '../shared/types';

export interface GameSwitcherOption {
  gameId: string;
  label: string;
}

interface GameSwitcherProps {
  options: GameSwitcherOption[];
  shownGame: string;
  onSwitch: (gameId: string) => void;
  collapsed: boolean;
}

/**
 * The game's exe icon, or `fallback` while it loads and whenever there is none.
 * Minecraft is skipped rather than asked: discovery never finds it, so the
 * command would answer `null` after a full scan for nothing.
 */
function GameSwitcherIcon({ gameId, fallback }: { gameId: string; fallback: ReactNode }) {
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
        // Decoration, not data: a failed lookup keeps today's letter/gamepad.
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
    return <img src={icon} alt="" className="h-4 w-4 shrink-0 rounded-sm object-contain" />;
  }
  return <>{fallback}</>;
}

export function GameSwitcher({ options, shownGame, onSwitch, collapsed }: GameSwitcherProps) {
  const [open, setOpen] = useState(false);
  const current = options.find((option) => option.gameId === shownGame) ?? options[0];
  if (!current) return null;

  const fallbackLetter = (option: GameSwitcherOption) => (
    <span aria-hidden="true">{option.gameId === MINECRAFT_GAME_ID ? 'M' : option.label.charAt(0)}</span>
  );
  const gamepad = <Gamepad2 className="h-4 w-4 shrink-0" aria-hidden="true" />;

  return (
    <div className="space-y-1" data-testid="game-switcher" data-tour="game-switcher">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        aria-label={`Game: ${current.label}. Choose a game`}
        title={collapsed ? current.label : undefined}
        className={[
          'flex w-full items-center gap-2 rounded-lg border border-border px-2.5 py-2 text-left text-sm font-medium hover:bg-accent',
          collapsed ? 'justify-center px-0' : '',
        ].join(' ')}
      >
        <GameSwitcherIcon gameId={current.gameId} fallback={gamepad} />
        {!collapsed && <span className="min-w-0 flex-1 truncate">{current.label}</span>}
        {!collapsed && (open ? <ChevronUp className="h-4 w-4 shrink-0" aria-hidden="true" /> : <ChevronDown className="h-4 w-4 shrink-0" aria-hidden="true" />)}
      </button>
      {open && (
        <div role="group" aria-label="Games" className="space-y-1">
          {options.map((option) => {
            const isCurrent = option.gameId === current.gameId;
            return (
              <button
                key={option.gameId}
                type="button"
                aria-pressed={isCurrent}
                aria-label={option.label}
                title={collapsed ? option.label : undefined}
                onClick={() => {
                  setOpen(false);
                  if (!isCurrent) onSwitch(option.gameId);
                }}
                className={[
                  'flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm',
                  collapsed ? 'justify-center px-0' : '',
                  isCurrent
                    ? 'bg-primary text-primary-foreground shadow-sm'
                    : 'text-muted-foreground hover:bg-accent hover:text-accent-foreground',
                ].join(' ')}
              >
                <GameSwitcherIcon gameId={option.gameId} fallback={collapsed ? fallbackLetter(option) : gamepad} />
                {!collapsed && <span className="min-w-0 flex-1 truncate">{option.label}</span>}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
