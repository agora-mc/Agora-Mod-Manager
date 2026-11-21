/**
 * The Linux launch panel: Proton is not tested on real hardware yet, and Steam starts the game
 * through the wrapper in its launch options. The page shows the exact text to paste and a copy
 * button. Read only: Agora never edits Steam's files. On other platforms the command says the
 * panel is unsupported and nothing is drawn.
 */

import { useEffect, useState } from 'react';
import { Check, Copy, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  gamesErrorMessage,
  getSteamLaunchInfo,
  isDefaultGameInstance,
  setDefaultGameInstance,
  type SteamLaunchInfo,
} from './api';

export function SteamLaunchPanel({ instanceId }: { instanceId: string }) {
  const [info, setInfo] = useState<SteamLaunchInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  // Whether Steam starts this instance when it launches the game with nothing pending.
  const [isDefault, setIsDefault] = useState<boolean | null>(null);
  const [defaultError, setDefaultError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setInfo(null);
    setError(null);
    setCopied(false);
    setIsDefault(null);
    setDefaultError(null);
    getSteamLaunchInfo(instanceId)
      .then((value) => {
        if (live) setInfo(value);
        if (live && value.supported) {
          isDefaultGameInstance(instanceId)
            .then((answer) => {
              if (live) setIsDefault(answer);
            })
            .catch((failure: unknown) => {
              if (live) setDefaultError(gamesErrorMessage(failure));
            });
        }
      })
      .catch((failure: unknown) => {
        if (live) setError(gamesErrorMessage(failure));
      });
    return () => {
      live = false;
    };
  }, [instanceId]);

  if (error !== null) {
    return (
      <p role="alert" className="text-sm text-destructive">
        The Steam launch options could not be read. {error}
      </p>
    );
  }
  if (info === null) {
    return (
      <p role="status" className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" aria-hidden="true" />
        Reading Steam's launch options…
      </p>
    );
  }
  if (!info.supported) return null;

  const makeDefault = async () => {
    setDefaultError(null);
    try {
      await setDefaultGameInstance(instanceId);
      setIsDefault(true);
    } catch (failure) {
      setDefaultError(gamesErrorMessage(failure));
    }
  };

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(info.expected);
      setCopied(true);
    } catch {
      setError('This window could not write to the clipboard. Select the text and copy it.');
    }
  };

  return (
    <section
      aria-labelledby="steam-launch-heading"
      className="space-y-3 rounded-xl border border-border bg-card p-5"
      data-testid="steam-launch-panel"
    >
      <h3 id="steam-launch-heading" className="text-lg font-semibold">Steam and Proton</h3>
      <p role="note" className="text-sm">{info.note}</p>
      <p className="text-sm text-muted-foreground">
        Steam starts the game through this wrapper. Put it in the game's Properties → Launch
        Options:
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <code className="rounded bg-muted px-2 py-1 text-sm" data-testid="steam-launch-text">
          {info.expected}
        </code>
        <Button type="button" variant="outline" size="sm" onClick={() => void copy()}>
          {copied ? <Check aria-hidden="true" /> : <Copy aria-hidden="true" />}
          {copied ? 'Copied' : 'Copy'}
        </Button>
      </div>
      {info.wrapperPresent ? (
        <p className="text-sm text-green-700 dark:text-green-400">
          Steam's launch options already run the wrapper
          {info.configured === null ? '' : `: ${info.configured}`}.
        </p>
      ) : (
        <p className="text-sm">
          Steam's launch options do not run Agora yet{info.configured === null ? '' : `: ${info.configured}`}.
        </p>
      )}
      {info.files.length > 0 && (
        <p className="text-xs text-muted-foreground">
          Agora read the launch options from {info.files.join(', ')}.
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2" data-testid="steam-default">
        {isDefault === true ? (
          <p className="text-sm text-green-700 dark:text-green-400">
            Steam starts this instance when you launch the game from Steam.
          </p>
        ) : (
          <>
            <p className="text-sm text-muted-foreground">
              When you launch the game from Steam, it starts this game&apos;s default instance.
            </p>
            {isDefault === false && (
              <Button type="button" variant="outline" size="sm" onClick={() => void makeDefault()}>
                Make this the one Steam starts
              </Button>
            )}
          </>
        )}
        {defaultError !== null && (
          <p role="alert" className="text-xs text-destructive">{defaultError}</p>
        )}
      </div>
      {info.problems.length > 0 && (
        <ul role="alert" className="space-y-1 text-xs text-destructive">
          {info.problems.map((problem) => (
            <li key={problem}>Could not read {problem}</li>
          ))}
        </ul>
      )}
    </section>
  );
}
