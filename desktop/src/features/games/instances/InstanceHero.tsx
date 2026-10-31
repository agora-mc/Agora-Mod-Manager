/**
 * The header every page of a game instance shares, in the shape of the Minecraft instance page:
 * the instance's tile, name and facts on the left, Launch and how ready it is on the right, and
 * whatever a launch answers right under it, so a launch started from any page is seen.
 */

import { useEffect, useState } from 'react';
import { Loader2, Play } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { InstanceIcon, MetaChip } from '@/components/InstanceIcon';
import type { DeploymentMode, InstanceCheck, LaunchReply } from './api';
import { checkHeadline, deploymentLabel, elapsedText, fallbackTradeoffs } from './instanceText';
import { useInstanceSession } from './instanceSession';

type Readiness = 'blocked' | 'review' | 'ready' | 'unknown';

function readinessOf(check: InstanceCheck | null, checkError: string | null): Readiness {
  if (checkError !== null || check === null) return 'unknown';
  if (check.status === 'findings') return 'blocked';
  if (check.status === 'warnings') return 'review';
  return 'ready';
}

const READINESS_CLASS: Record<Readiness, string> = {
  blocked: 'text-destructive',
  review: 'text-amber-700 dark:text-amber-300',
  ready: 'text-green-700 dark:text-green-400',
  unknown: 'text-muted-foreground',
};

/** A backend time (SQLite UTC, no zone), or null when it does not parse. */
function parseStored(value: string): Date | null {
  const parsed = new Date(`${value.trim().replace(' ', 'T')}Z`);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

/** `Created Oct 1, 2026`, or null when the stored date does not parse. */
function createdText(value: string): string | null {
  const parsed = parseStored(value);
  if (parsed === null) return null;
  return `Created ${new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric', year: 'numeric' }).format(parsed)}`;
}

/** `Oct 29, 2026, 6:30 PM`; a value that does not parse is shown as it came. */
function playedWhen(value: string): string {
  const parsed = parseStored(value);
  if (parsed === null) return value;
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(parsed);
}

export function InstanceHero({ onShowChecks }: { onShowChecks: () => void }) {
  const session = useInstanceSession();
  const { page, busy, launchStartedAt, reply, actionError } = session;
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (busy !== 'launch') return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [busy]);

  if (page.status === 'failed') {
    return (
      <div role="alert" className="space-y-3 rounded-xl border border-destructive/50 bg-destructive/10 p-6">
        <h2 className="font-semibold">This instance could not be loaded</h2>
        <p className="text-sm">{page.message}</p>
        <Button type="button" variant="outline" onClick={session.reload}>Try again</Button>
      </div>
    );
  }
  if (page.status !== 'loaded') return null;

  const { detail, check, checkError, refreshing } = page;
  const readiness = readinessOf(check, checkError);
  const runtime = [detail.store, detail.version, detail.build].filter(Boolean).join(' ');
  const created = createdText(detail.createdAt);
  const elapsed = launchStartedAt === null ? 0 : (now - launchStartedAt) / 1000;

  return (
    <section
      className={`rounded-xl border bg-card p-6 ${detail.running ? 'instance-card--running border-border' : 'border-border'}`}
      data-testid="instance-hero"
    >
      <div className="flex flex-col gap-4 xl:flex-row xl:items-start xl:justify-between">
        <div className="flex min-w-0 items-start gap-4">
          <InstanceIcon name={detail.name} seed={detail.instanceId} size={64} />
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-3">
              <h2 className="wrap-break-word text-2xl font-bold">{detail.name}</h2>
              {detail.running && (
                <span className="rounded-full bg-primary px-2.5 py-0.5 text-xs font-semibold text-primary-foreground" role="status">
                  Running now
                </span>
              )}
            </div>
            <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
              <MetaChip>{detail.gameName}</MetaChip>
              {runtime !== '' && <MetaChip>{runtime}</MetaChip>}
              <MetaChip>{deploymentLabel(detail.deployment)}</MetaChip>
              <MetaChip>{detail.pinned ? 'Pinned base' : 'Runs from install'}</MetaChip>
            </div>
            <p className="mt-2 text-xs text-muted-foreground">
              {detail.running ? (
                <span className="inline-flex items-center gap-1.5 text-green-600 dark:text-green-400">
                  <span className="instance-status-dot" aria-hidden="true" />
                  Playing now
                </span>
              ) : detail.lastLaunchedAt ? (
                `Last played ${playedWhen(detail.lastLaunchedAt)}`
              ) : (
                'Never played'
              )}
              {created !== null && <span> · {created}</span>}
            </p>
          </div>
        </div>

        <div className="flex flex-col items-start gap-2 xl:items-end">
          <button
            type="button"
            onClick={() => void session.launch()}
            disabled={busy !== null || detail.running}
            className="inline-flex items-center gap-2 rounded-lg bg-primary px-5 py-3 text-base font-semibold text-primary-foreground shadow-xs hover:bg-primary/90 disabled:cursor-not-allowed disabled:opacity-50"
            aria-label={`Launch ${detail.name}`}
          >
            {busy === 'launch' ? (
              <Loader2 className="h-5 w-5 animate-spin" aria-hidden="true" />
            ) : (
              <Play className="h-5 w-5 fill-current" aria-hidden="true" />
            )}
            {busy === 'launch' ? 'Starting…' : detail.running ? 'Running' : 'Launch'}
          </button>
          <button
            type="button"
            onClick={onShowChecks}
            className={`max-w-sm text-left text-xs font-medium underline-offset-4 hover:underline xl:text-right ${READINESS_CLASS[readiness]}`}
            data-testid="instance-readiness"
          >
            {checkError !== null ? 'The checks could not run' : checkHeadline(check)}
            {refreshing && check !== null && (
              <span className="ml-1.5 font-normal text-muted-foreground">· checking again…</span>
            )}
          </button>
        </div>
      </div>

      {(busy === 'launch' || reply !== null || actionError !== null) && (
        <div className="mt-4 space-y-3 border-t border-border pt-4">
          {actionError !== null && (
            <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
              {actionError}
            </p>
          )}
          {busy === 'launch' && (
            <div role="status" className="space-y-1 text-sm">
              <p className="font-medium">Checking, preparing and starting the game…</p>
              <p className="text-muted-foreground">
                {elapsedText(elapsed)} so far. A large instance can take several minutes the first time, and Agora cannot show a percentage yet.
              </p>
            </div>
          )}
          {reply !== null && (
            <LaunchResult
              reply={reply}
              onChoose={(next, always) => void session.answerFallback(next, always)}
              onDismiss={session.dismissReply}
            />
          )}
        </div>
      )}
    </section>
  );
}

function LaunchResult({
  reply,
  onChoose,
  onDismiss,
}: {
  reply: LaunchReply;
  onChoose: (next: DeploymentMode, always: boolean) => void;
  onDismiss: () => void;
}) {
  if (reply.status === 'launched') {
    return (
      <div role="status" className="space-y-2 rounded-lg border border-border p-3 text-sm">
        <p className="font-medium">The game is starting (process {reply.pid}). Agora will put your files back when it closes.</p>
        <p className="text-muted-foreground">
          {reply.deployment === null
            ? 'Running from the base, with nothing deployed on top of it.'
            : `Running from ${deploymentLabel(reply.deployment).toLowerCase()}.`}
        </p>
        {reply.notice !== null && <p>{reply.notice}</p>}
        {reply.findings.length > 0 && (
          <div className="space-y-1">
            <p className="font-medium">Launched past these:</p>
            <ul className="list-disc pl-5">
              {reply.findings.map((finding, index) => (
                <li key={index}>{finding.title}: {finding.detail}</li>
              ))}
            </ul>
          </div>
        )}
      </div>
    );
  }

  if (reply.status === 'refused') {
    return (
      <div role="alert" className="space-y-2 rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
        <p className="font-medium">The launch was stopped by these checks. Nothing was started.</p>
        <p>Fix them on the Overview, or turn on Advanced options there and launch anyway at your own risk.</p>
        <button type="button" className="text-xs underline" onClick={onDismiss}>Dismiss</button>
      </div>
    );
  }

  // The virtual file system would not start. The user decides; nothing was started.
  const next = reply.next;
  return (
    <div role="alertdialog" aria-label="The virtual file system could not start" className="space-y-3 rounded-lg border border-border p-4 text-sm" data-testid="vfs-question">
      <p className="font-semibold">The virtual file system could not start.</p>
      {reply.reason !== null && <p className="text-muted-foreground">{reply.reason}</p>}
      {next === null ? (
        <p>There is no simpler way to run this instance. Fix the reason above, then try again.</p>
      ) : (
        <>
          <p>Agora can run this instance from {deploymentLabel(next).toLowerCase()} instead. Nothing has been changed yet.</p>
          <ul className="list-disc space-y-1 pl-5 text-muted-foreground">
            {fallbackTradeoffs(next).map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ul>
          <div className="flex flex-wrap gap-2">
            <Button type="button" onClick={() => onChoose(next, false)}>
              Run once from {deploymentLabel(next).toLowerCase()}
            </Button>
            <Button type="button" variant="outline" onClick={() => onChoose(next, true)}>
              Always run from {deploymentLabel(next).toLowerCase()}
            </Button>
          </div>
        </>
      )}
      <button type="button" className="text-xs underline" onClick={onDismiss}>Cancel</button>
    </div>
  );
}
