import { useState } from 'react';
import { ArrowRight, CheckCircle2, Loader2, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { openGameSettings } from '@/lib/settingsSection';
import type { CheckFinding } from './api';
import {
  checkHeadline,
  DEPLOYMENT_OPTIONS,
  deploymentLabel,
  formatWhen,
  frameworksMatch,
  launchAnywayText,
  orderFindings,
} from './instanceText';
import { useInstanceSession } from './instanceSession';

interface InstanceOverviewTabProps {
  gameId: string;
  instanceId: string;
}

/** `checked 2 min ago`, for the last answer the page is showing while it checks again. */
function checkedAgo(at: number | null, now: number): string | null {
  if (at === null) return null;
  const seconds = Math.max(0, Math.round((now - at) / 1000));
  if (seconds < 60) return 'checked just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `checked ${minutes} min ago`;
  return `checked ${Math.round(minutes / 60)} h ago`;
}

/**
 * An instance's Overview: its launch checks on the left; how it runs and what it is built from on
 * the right. Launch itself is in the header every page shares. The last checks show at once and are
 * refreshed in the background.
 */
export function InstanceOverviewTab(_props: InstanceOverviewTabProps) {
  const session = useInstanceSession();
  const { page, busy } = session;
  const [advancedOpen, setAdvancedOpen] = useState(false);

  if (page.status === 'loading') {
    return <p role="status" className="text-sm text-muted-foreground">Loading this instance…</p>;
  }

  // The header above says when the instance could not be loaded, on every page.
  if (page.status === 'failed') return null;

  const { detail, check, checkError, refreshing, refreshError, checkedAt } = page;
  const matches = frameworksMatch(check);
  const choice = detail.deployment ?? 'auto';
  const choiceCopy = DEPLOYMENT_OPTIONS.find((option) => option.id === choice);
  const skippedText = launchAnywayText(check);
  const ago = refreshing ? checkedAgo(checkedAt, Date.now()) : null;

  return (
    <section className="grid items-start gap-6 lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]" data-testid="instance-overview">
      <section aria-labelledby="checks-heading" className="space-y-4 rounded-xl border border-border bg-card p-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="min-w-0">
            <h3 id="checks-heading" className="text-lg font-semibold">Launch checks</h3>
            {checkError === null && (
              <p className="text-sm text-muted-foreground" data-testid="checks-headline">
                {checkHeadline(check)}
              </p>
            )}
          </div>
          <Button type="button" variant="outline" size="sm" onClick={session.reload} disabled={refreshing}>
            {refreshing ? <Loader2 className="animate-spin" aria-hidden="true" /> : <RefreshCw aria-hidden="true" />}
            {refreshing ? 'Checking…' : 'Check again'}
          </Button>
        </div>
        {refreshing && check !== null && (
          <p role="status" className="text-xs text-muted-foreground">
            Showing the last answer{ago === null ? '' : ` (${ago})`} while Agora checks again.
          </p>
        )}
        {refreshError !== null && (
          <p role="alert" className="text-sm text-destructive">This instance could not be read again. Showing what Agora last saw. {refreshError}</p>
        )}
        {checkError !== null && (
          <p role="alert" className="text-sm text-destructive">The checks could not run. {checkError}</p>
        )}
        {check !== null && check.findings.length > 0 && (
          <ul className="space-y-3" aria-label="Launch check findings">
            {orderFindings(check.findings).map((finding, index) => (
              <FindingCard key={`${finding.kind}-${index}`} finding={finding} />
            ))}
          </ul>
        )}
        {check !== null && check.passed.length > 0 && (
          <ul className="divide-y divide-border rounded-lg border border-border" aria-label="Launch checks that passed">
            {check.passed.map((passed, index) => (
              <li key={`${passed.kind}-${index}`} className="flex items-start gap-3 px-3 py-2.5" data-testid="check-passed">
                <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-green-600 dark:text-green-400" aria-label="Passed" />
                <div className="min-w-0">
                  <p className="text-sm font-medium">{passed.title}</p>
                  <p className="text-xs text-muted-foreground">{passed.detail}</p>
                </div>
              </li>
            ))}
          </ul>
        )}

        <div className="border-t border-border pt-3">
          <button
            type="button"
            aria-expanded={advancedOpen}
            onClick={() => setAdvancedOpen((open) => !open)}
            className="text-sm font-medium text-muted-foreground underline-offset-4 hover:underline"
          >
            {advancedOpen ? 'Hide advanced options' : 'Advanced options'}
          </button>
          {advancedOpen && (
            <div className="mt-3 space-y-2 rounded-lg border border-destructive/40 p-3">
              <label className="flex items-start gap-2 text-sm">
                <input
                  type="checkbox"
                  className="mt-1"
                  checked={session.launchAnyway}
                  onChange={(event) => session.setLaunchAnyway(event.target.checked)}
                />
                <span>
                  <span className="font-medium">Launch anyway</span>:{' '}
                  {skippedText ?? 'start the game past the checks above, even when they would stop it.'}
                </span>
              </label>
              <p className="text-xs text-destructive">
                The game may crash or behave in the ways these checks exist to prevent. Use this only when you know why a check is wrong. Agora does not undo a session that goes badly.
              </p>
            </div>
          )}
        </div>
      </section>

      <div className="space-y-6">
        <section aria-labelledby="deployment-heading-overview" className="space-y-3 rounded-xl border border-border bg-card p-5">
          <h3 id="deployment-heading-overview" className="text-lg font-semibold">How this instance runs</h3>
          <div className="grid grid-cols-2 gap-1 rounded-lg bg-muted p-1" role="group" aria-label="Deployment">
            {DEPLOYMENT_OPTIONS.map((option) => {
              const chosen = option.id === choice;
              return (
                <button
                  key={option.id}
                  type="button"
                  aria-pressed={chosen}
                  disabled={busy !== null || detail.running}
                  onClick={() => void session.changeDeployment(option.id)}
                  className={[
                    'rounded-md px-2 py-1.5 text-center leading-tight text-sm font-medium transition-colors disabled:opacity-50',
                    chosen ? 'bg-background text-foreground shadow-sm' : 'text-muted-foreground hover:text-foreground',
                  ].join(' ')}
                >
                  {option.label}
                </button>
              );
            })}
          </div>
          <p className="text-sm text-muted-foreground" data-testid="deployment-explained">
            <span className="font-medium text-foreground">{deploymentLabel(detail.deployment)}.</span>{' '}
            {choiceCopy?.explain}
          </p>
          {detail.running && (
            <p className="text-xs text-muted-foreground">The way the instance runs can be changed once the game has closed.</p>
          )}
        </section>

        <section aria-labelledby="about-heading" className="space-y-3 rounded-xl border border-border bg-card p-5">
          <h3 id="about-heading" className="text-lg font-semibold">About this instance</h3>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
            <dt className="text-muted-foreground">Game</dt>
            <dd>{detail.gameName}</dd>
            <dt className="text-muted-foreground">Runtime</dt>
            <dd>{[detail.store, detail.version, detail.build].filter(Boolean).join(' ') || 'Not recorded'}</dd>
            <dt className="text-muted-foreground">Base</dt>
            <dd className="wrap-break-word">
              {detail.pinned
                ? `Pinned${detail.baseId ? ` (${detail.baseId})` : ''}`
                : `Not pinned: runs from the install folder${detail.unpinnedReason ? `. ${detail.unpinnedReason}` : ''}`}
            </dd>
            <dt className="text-muted-foreground">Frameworks</dt>
            <dd data-testid="frameworks-status">
              {matches === null && 'Not checked'}
              {matches === true && 'Match this game version'}
              {matches === false && 'Do not match this game version'}
            </dd>
            <dt className="text-muted-foreground">Saves</dt>
            <dd>{detail.saves === 'own' ? "This instance's own" : 'Shared with the game'}</dd>
            <dt className="text-muted-foreground">Last played</dt>
            <dd>{formatWhen(detail.lastLaunchedAt)}</dd>
          </dl>
        </section>
      </div>
    </section>
  );
}

function FindingCard({ finding }: { finding: CheckFinding }) {
  return (
    <li
      className={[
        'space-y-1 rounded-lg border p-4',
        finding.blocks ? 'border-destructive/50 bg-destructive/5' : 'border-amber-500/50 bg-amber-500/5',
      ].join(' ')}
      data-testid="check-finding"
      data-blocks={finding.blocks ? 'true' : 'false'}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span
          className={[
            'rounded-full px-2 py-0.5 text-xs font-semibold',
            finding.blocks ? 'bg-destructive text-destructive-foreground' : 'bg-amber-500/15 text-amber-700 dark:text-amber-300',
          ].join(' ')}
        >
          {finding.blocks ? 'Stops the launch' : 'To review'}
        </span>
        <span className="font-semibold">{finding.title}</span>
      </div>
      <p className="text-sm">{finding.detail}</p>
      <p className="text-sm text-muted-foreground">
        <span className="font-medium text-foreground">Why it matters: </span>
        {finding.why}
      </p>
      {finding.repair !== null && (
        <p className="text-sm">
          <span className="font-medium">What to do: </span>
          {finding.repair}
        </p>
      )}
      {finding.kind === 'base' && (
        <Button type="button" variant="outline" size="sm" className="mt-1" onClick={() => openGameSettings('bases')}>
          Open Bases in Settings
          <ArrowRight aria-hidden="true" />
        </Button>
      )}
    </li>
  );
}
