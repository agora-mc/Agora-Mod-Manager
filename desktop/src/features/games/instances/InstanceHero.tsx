/**
 * The header every page of a game instance shares: a thin adapter that fills the kit's
 * `InstancePageHeader` (the same header as Minecraft's instance page) with this instance: its
 * tile, name and facts on the left, Launch and how ready it is on the right, and whatever a launch
 * answers right under it, so a launch started from any page (or from the instance's card) is seen.
 */

import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  InstancePageHeader,
  InstanceTileButton,
  ReadinessLine,
  SetImageButton,
  type InstanceAction,
  type ReadinessView,
} from '@/components/instance-kit';
import { gamesErrorMessage, type DeploymentMode, type InstanceCheck, type LaunchReply } from './api';
import { checkHeadline, deploymentLabel, elapsedText, fallbackTradeoffs } from './instanceText';
import { createdOn, playedWhen } from './instanceDates';
import { GAME_INSTANCE_CAPABILITIES, GameInstanceTile, InstanceChips } from './instanceIdentity';
import { chooseGameInstanceIcon, useGameInstanceIcon } from './instanceIcons';
import { useInstanceSession } from './instanceSession';

/** The checks' answer as the kit's readiness line. */
export function readinessView(check: InstanceCheck | null, checkError: string | null, refreshing: boolean): ReadinessView {
  if (checkError !== null) return { level: 'unknown', headline: 'The checks could not run' };
  const level = check === null
    ? 'unknown'
    : check.status === 'findings'
      ? 'blocked'
      : check.status === 'warnings'
        ? 'check'
        : 'ready';
  return { level, headline: checkHeadline(check), refreshing: refreshing && check !== null };
}

export function InstanceHero({ onShowChecks }: { onShowChecks: () => void }) {
  const session = useInstanceSession();
  const { page, busy, launchStartedAt, reply, actionError } = session;
  const [now, setNow] = useState(() => Date.now());
  const customIcon = useGameInstanceIcon(session.instanceId);
  const [imageBusy, setImageBusy] = useState(false);
  const [imageError, setImageError] = useState<string | null>(null);

  const chooseImage = async () => {
    setImageBusy(true);
    setImageError(null);
    try {
      await chooseGameInstanceIcon(session.instanceId);
    } catch (failure) {
      setImageError(`The image was not set. ${gamesErrorMessage(failure)}`);
    } finally {
      setImageBusy(false);
    }
  };

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
  const created = createdOn(detail.createdAt);
  const elapsed = launchStartedAt === null ? 0 : (now - launchStartedAt) / 1000;

  // While the game runs there is nothing to press until games can be stopped (`kill`): the
  // header says it is running instead of showing a dead button.
  const launch: InstanceAction | undefined = detail.running
    ? undefined
    : {
        id: 'launch',
        label: busy === 'launch' ? 'Starting…' : 'Launch',
        ariaLabel: `Launch ${detail.name}`,
        busy: busy === 'launch',
        disabled: busy !== null,
        onSelect: () => void session.launch(),
      };

  const showBanners = busy === 'launch' || reply !== null || actionError !== null || imageError !== null;

  return (
    <InstancePageHeader
      testId="instance-hero"
      running={detail.running}
      tile={
        <InstanceTileButton hasImage={customIcon !== null} onSelect={() => void chooseImage()} disabled={imageBusy}>
          <GameInstanceTile instanceId={detail.instanceId} name={detail.name} size={64} />
        </InstanceTileButton>
      }
      name={detail.name}
      status={detail.running && (
        <span className="rounded-full bg-primary px-2.5 py-0.5 text-xs font-semibold text-primary-foreground" role="status">
          Running now
        </span>
      )}
      chips={
        <InstanceChips
          store={detail.store}
          version={detail.version}
          build={detail.build}
          deployment={detail.deployment}
          pinned={detail.pinned}
        />
      }
      facts={[
        detail.lastLaunchedAt ? `Last played ${playedWhen(detail.lastLaunchedAt)}` : 'Never played',
        created === null ? null : `Created ${created}`,
      ]}
      factsTrail={<SetImageButton onSelect={() => void chooseImage()} disabled={imageBusy} />}
      capabilities={GAME_INSTANCE_CAPABILITIES}
      primaryAction={launch}
      readiness={!detail.running && (
        <ReadinessLine readiness={readinessView(check, checkError, refreshing)} onSelect={onShowChecks} />
      )}
      banners={showBanners && (
        <>
          {imageError !== null && (
            <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm text-destructive">
              {imageError}
            </p>
          )}
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
        </>
      )}
    />
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
