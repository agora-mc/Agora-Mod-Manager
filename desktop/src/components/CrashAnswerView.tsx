/**
 * The crash answer screen (`.spike/CRASH-ANSWER-SCREEN-DESIGN.md`): one stored answer, rendered
 * top to bottom. It is presentational: grouping, ordering and the words of findings come from the
 * answer, and every action is a callback the page supplies.
 *
 * Agora never says a mod is certainly the cause (§30.5), so no level here ever reads "certain".
 */

import { useState } from 'react';
import { AlertTriangle, ChevronDown, FileText } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type {
  AnswerStatus,
  CrashAnswer,
  CrashArtifact,
  CrashCandidate,
  CrashCause,
  CrashChange,
  CrashFinding,
  CrashLevel,
  CrashNextStep,
  CrashOwner,
  CrashTrustNote,
  CrashVerdict,
  LocalText,
} from '@/features/games/instances/crashAnswerApi';

export type CrashAnswerOrigin = 'session' | 'pasted' | 'file';

export interface CrashAnswerViewProps {
  status: AnswerStatus;
  answer?: CrashAnswer | null;
  reason?: string | null;
  verdict?: CrashVerdict | null;
  endedAt?: string | null;
  changes?: CrashChange[];
  retainedCrashes?: number;
  origin?: CrashAnswerOrigin;
  /** The answer's next step, the bisect/second-opinion buttons, and "Run it again". */
  onNext?: (kind: CrashNextStep['kind'], keys: string[]) => void;
  onRunAgain?: () => void;
  onCheckAnother: () => void;
  onAskPhostwood?: () => void;
  onShowFile?: (artifact: CrashArtifact) => void;
  onTestWithout?: (candidate: CrashCandidate) => void;
  onOpenModPage?: (candidate: CrashCandidate) => void;
  onShowSuspectFiles?: (candidate: CrashCandidate) => void;
}

/** The level words, exactly as the design gives them. */
const LEVEL_WORDS: Record<CrashLevel, string> = {
  not_sure: 'not sure',
  possible: 'possible',
  likely: 'likely',
  very_likely: 'very likely',
};

const SAVE_RISK_WORDS = {
  safe: 'Safe to test on your save',
  risky: 'Risky for your save',
  unknown: 'Back up your save first',
} as const;

/** The cause groups, in the design's order. */
const CAUSE_GROUPS: ReadonlyArray<{ cause: CrashCause; heading: string }> = [
  { cause: 'missing_requirement', heading: 'Something is missing' },
  { cause: 'version_mismatch', heading: 'Built for another version' },
  { cause: 'file', heading: 'A damaged file' },
  { cause: 'setting', heading: 'A setting' },
  { cause: 'save', heading: 'Your save' },
  { cause: 'memory', heading: 'Memory' },
  { cause: 'driver', heading: 'Graphics driver' },
  { cause: 'game_install', heading: "The game's own files" },
  { cause: 'other', heading: 'Other' },
];

const SECOND_OPINION_NOTE = 'Copies the crash log and opens a website.';

const SAVE_WARNING =
  'Removing a mod partway through a playthrough can damage your save. Back up your saves before you test, or test on a new game.';

/** The sentence shown when a log was read and Agora named nobody. */
const NAMED_NOBODY =
  "Agora couldn't name a cause from this crash. That's common: about a third of crashes leave no clue in the log.";

const NO_LOG = 'The game stopped without leaving a crash log.';

export function CrashAnswerView({
  status,
  answer,
  reason,
  verdict,
  endedAt,
  changes,
  retainedCrashes,
  origin = 'session',
  onNext,
  onRunAgain,
  onCheckAnother,
  onAskPhostwood,
  onShowFile,
  onTestWithout,
  onOpenModPage,
  onShowSuspectFiles,
}: CrashAnswerViewProps) {
  if (status === 'nothing_to_read' && origin === 'session') {
    return (
      <section className="space-y-4" data-testid="crash-answer-view" data-status={status}>
        <h2 className="text-xl font-semibold">{NO_LOG}</h2>
        {reason !== null && reason !== undefined && (
          <p className="text-sm text-muted-foreground">{reason}</p>
        )}
        <div className="flex flex-wrap gap-2">
          <Button type="button" onClick={() => onRunAgain?.()}>Run it again</Button>
          <Button type="button" variant="outline" onClick={onCheckAnother}>Check a crash log</Button>
        </div>
      </section>
    );
  }

  if (status !== 'analyzed' || answer === null || answer === undefined) {
    return (
      <section className="space-y-4" data-testid="crash-answer-view" data-status={status}>
        <h2 className="text-xl font-semibold">
          Agora couldn&apos;t analyze this crash: {reason ?? 'no reason was given'}.
        </h2>
        <Button type="button" variant="outline" onClick={onCheckAnother}>Check a crash log</Button>
      </section>
    );
  }

  const suspects = answer.candidates;
  const suspectKeys = new Set(suspects.map((candidate) => ownerKey(candidate.owner)));
  const otherFindings = answer.findings.filter(
    (finding) => finding.candidate === undefined || !suspectKeys.has(ownerKey(finding.candidate)),
  );
  const grouped = groupFindings(otherFindings);
  const saveWarning = needsSaveWarning(answer);
  const verdictWord = verdictWordFor(verdict ?? answer.verdict);

  return (
    <section className="space-y-6" data-testid="crash-answer-view" data-status={status}>
      <header className="space-y-1">
        <h2 className="text-xl font-semibold wrap-anywhere" data-testid="crash-headline">
          {english(answer.headline)}
        </h2>
        <p className="text-sm text-muted-foreground" data-testid="crash-confidence">
          Agora&apos;s confidence: {LEVEL_WORDS[answer.level]}
          {verdictWord === null ? null : ` · ${verdictWord}`}
          {endedAt === null || endedAt === undefined ? null : ` ${formatWhen(endedAt)}`}
        </p>
        {origin === 'pasted' && (
          <p className="text-xs text-muted-foreground" data-testid="crash-snippet-note">
            This answer came from a log you pasted. Agora read only that text.
          </p>
        )}
      </header>

      <TrustNotes notes={answer.trust_notes} />

      {suspects.length > 0 && <NextStepButton answer={answer} onNext={onNext} />}

      {saveWarning && (
        <div
          role="note"
          className="flex items-start gap-2 rounded-lg border border-amber-500/50 bg-amber-500/10 p-3 text-sm"
          data-testid="crash-save-warning"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-400" aria-hidden="true" />
          <p>{SAVE_WARNING}</p>
        </div>
      )}

      {suspects.length > 0 ? (
        <section aria-labelledby="crash-suspects-heading" className="space-y-3">
          <h3 id="crash-suspects-heading" className="text-base font-semibold">Suspects</h3>
          <ol className="space-y-3">
            {suspects.map((candidate) => (
              <li key={ownerKey(candidate.owner)} className="rounded-lg border border-border bg-card p-4" data-testid="crash-suspect">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-semibold wrap-anywhere">{ownerName(candidate.owner)}</span>
                  <LevelChip level={candidate.level} />
                  {candidate.save_risk !== undefined && (
                    <span
                      className={[
                        'rounded-full px-2 py-0.5 text-xs font-semibold',
                        candidate.save_risk === 'risky'
                          ? 'bg-destructive/15 text-destructive'
                          : candidate.save_risk === 'safe'
                            ? 'bg-green-500/15 text-green-700 dark:text-green-400'
                            : 'bg-amber-500/15 text-amber-700 dark:text-amber-300',
                      ].join(' ')}
                      data-testid="save-risk-chip"
                    >
                      {SAVE_RISK_WORDS[candidate.save_risk]}
                    </span>
                  )}
                  <SuspectMenu
                    candidate={candidate}
                    onTestWithout={onTestWithout}
                    onOpenModPage={onOpenModPage}
                    onShowFiles={onShowSuspectFiles}
                  />
                </div>
                <ul className="mt-2 space-y-1">
                  {answer.findings
                    .filter((finding) => finding.candidate !== undefined && ownerKey(finding.candidate) === ownerKey(candidate.owner))
                    .map((finding, index) => (
                      <li key={`${ownerKey(candidate.owner)}-${index}`} className="text-sm text-muted-foreground">
                        {english(finding.headline)}{' '}
                        <span className="whitespace-nowrap">from {finding.source.plugin_name}</span>
                      </li>
                    ))}
                </ul>
              </li>
            ))}
          </ol>
        </section>
      ) : (
        <section className="space-y-3" data-testid="crash-named-nobody">
          <p className="text-sm">{NAMED_NOBODY}</p>
          <div className="flex flex-wrap gap-2">
            <Button type="button" onClick={() => onNext?.('bisect', [])}>Find the mod by testing halves</Button>
            <Button type="button" variant="outline" onClick={() => onNext?.('second_opinion', [])}>
              Get a second opinion
            </Button>
          </div>
        </section>
      )}

      {grouped.length > 0 && (
        <section aria-labelledby="crash-other-heading" className="space-y-3">
          <h3 id="crash-other-heading" className="text-base font-semibold">Other findings</h3>
          <div className="space-y-4">
            {grouped.map((group) => (
              <div key={group.cause} data-testid="crash-cause-group" data-cause={group.cause}>
                <h4 className="text-sm font-semibold">{group.heading}</h4>
                <ul className="mt-1 space-y-1">
                  {group.findings.map((finding, index) => (
                    <li key={`${group.cause}-${index}`} className="text-sm">
                      <LevelChip level={finding.level} />{' '}
                      <span>{english(finding.headline)}</span>{' '}
                      <span className="whitespace-nowrap text-muted-foreground">from {finding.source.plugin_name}</span>
                      {finding.why_not_sure !== undefined && (
                        <p className="mt-0.5 text-xs text-muted-foreground">{english(finding.why_not_sure)}</p>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        </section>
      )}

      <Changes changes={changes} />

      <CouldNotCheck answer={answer} />

      <SecondOpinion onCheckAnother={onCheckAnother} onAskPhostwood={onAskPhostwood} />

      <Files
        artifacts={answer.artifacts}
        retainedCrashes={retainedCrashes}
        onShowFile={onShowFile}
      />
    </section>
  );
}

/** An instance's most recent crash, as the overview card shows it. */
export function CrashAnswerCard({
  verdict,
  headline,
  reason,
  status,
  onOpen,
  onHide,
}: {
  verdict?: CrashVerdict | null;
  headline?: string | null;
  reason?: string | null;
  status: AnswerStatus;
  onOpen: () => void;
  onHide: () => void;
}) {
  const verdictWord = verdictWordFor(verdict ?? null);
  const topLine = status === 'analyzed'
    ? (headline ?? '')
    : status === 'nothing_to_read'
      ? NO_LOG
      : `Agora couldn't analyze this crash: ${reason ?? 'no reason was given'}.`;
  return (
    <section className="rounded-xl border border-amber-500/50 bg-amber-500/5 p-5" data-testid="crash-answer-card">
      <p className="text-sm font-semibold" data-testid="crash-card-line">
        {verdictWord === null ? '' : `${verdictWord} last time`} · {topLine}
      </p>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <Button type="button" onClick={onOpen}>See what happened</Button>
        <Button type="button" variant="ghost" onClick={onHide}>Hide until the next crash</Button>
      </div>
    </section>
  );
}

function NextStepButton({
  answer,
  onNext,
}: {
  answer: CrashAnswer;
  onNext?: CrashAnswerViewProps['onNext'];
}) {
  const label = nextStepLabel(answer);
  if (label === null) return null;
  return (
    <Button type="button" onClick={() => onNext?.(answer.next.kind, answer.next.keys)}>
      {label}
    </Button>
  );
}

function SuspectMenu({
  candidate,
  onTestWithout,
  onOpenModPage,
  onShowFiles,
}: {
  candidate: CrashCandidate;
  onTestWithout?: (candidate: CrashCandidate) => void;
  onOpenModPage?: (candidate: CrashCandidate) => void;
  onShowFiles?: (candidate: CrashCandidate) => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button type="button" variant="ghost" size="sm">
          Options <ChevronDown aria-hidden="true" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuItem disabled={onTestWithout === undefined} onSelect={() => onTestWithout?.(candidate)}>
          Test without it
        </DropdownMenuItem>
        <DropdownMenuItem disabled={onOpenModPage === undefined} onSelect={() => onOpenModPage?.(candidate)}>
          Open mod page
        </DropdownMenuItem>
        <DropdownMenuItem disabled={onShowFiles === undefined} onSelect={() => onShowFiles?.(candidate)}>
          Show files
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function LevelChip({ level }: { level: CrashLevel }) {
  return (
    <span
      className={[
        'rounded-full px-2 py-0.5 text-xs font-semibold',
        level === 'very_likely' || level === 'likely'
          ? 'bg-primary/15 text-primary'
          : level === 'possible'
            ? 'bg-amber-500/15 text-amber-700 dark:text-amber-300'
            : 'bg-muted text-muted-foreground',
      ].join(' ')}
      data-testid="level-chip"
    >
      {LEVEL_WORDS[level]}
    </span>
  );
}

function TrustNotes({ notes }: { notes: CrashTrustNote[] }) {
  if (notes.length === 0) return null;
  return (
    <ul className="space-y-1" aria-label="Notes about the evidence">
      {notes.map((note, index) => (
        <li key={index} className="text-xs text-muted-foreground">
          {english(note.text)} <span className="whitespace-nowrap">(from {note.source.plugin_name})</span>
        </li>
      ))}
    </ul>
  );
}

function Changes({ changes }: { changes?: CrashChange[] }) {
  if (changes === undefined || changes.length === 0) return null;
  return (
    <section aria-labelledby="crash-changes-heading" className="space-y-2" data-testid="crash-changes">
      <h3 id="crash-changes-heading" className="text-base font-semibold">What changed since it last worked</h3>
      <ul className="list-disc space-y-1 pl-5 text-sm">
        {changes.map((change, index) => (
          <li key={`${change.kind}-${change.key ?? ''}-${index}`}>{describeChange(change)}</li>
        ))}
      </ul>
    </section>
  );
}

function CouldNotCheck({ answer }: { answer: CrashAnswer }) {
  const [open, setOpen] = useState(false);
  const count = answer.could_not_check.length;
  if (count === 0) return null;
  return (
    <section className="rounded-lg border border-border" data-testid="crash-could-not-check">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="flex w-full items-center justify-between gap-2 px-3 py-2 text-left text-sm font-medium"
      >
        <span>{count === 1 ? '1 check could not run' : `${count} checks could not run`}</span>
        <ChevronDown
          className={open ? 'size-4 rotate-180 transition-transform' : 'size-4 transition-transform'}
          aria-hidden="true"
        />
      </button>
      {open && (
        <ul className="space-y-1 border-t border-border px-3 py-2 text-xs text-muted-foreground">
          {answer.could_not_check.map((check, index) => (
            <li key={index}>
              {check.subject}: {check.reason.split('_').join(' ')}
              {check.detail === undefined ? null : `: ${check.detail}`}
              {check.source === undefined ? null : (
                <span className="whitespace-nowrap"> (from {check.source.plugin_name})</span>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function SecondOpinion({
  onCheckAnother,
  onAskPhostwood,
}: {
  onCheckAnother: () => void;
  onAskPhostwood?: () => void;
}) {
  return (
    <section aria-labelledby="crash-second-opinion-heading" className="space-y-2" data-testid="crash-second-opinion">
      <h3 id="crash-second-opinion-heading" className="text-base font-semibold">Second opinion</h3>
      <div className="flex flex-wrap items-center gap-2">
        <Button type="button" variant="outline" onClick={onCheckAnother}>Check another crash log</Button>
        <Button
          type="button"
          variant="outline"
          disabled={onAskPhostwood === undefined}
          onClick={() => onAskPhostwood?.()}
        >
          Ask Phostwood&apos;s analyzer
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">{SECOND_OPINION_NOTE}</p>
    </section>
  );
}

function Files({
  artifacts,
  retainedCrashes,
  onShowFile,
}: {
  artifacts: CrashArtifact[];
  retainedCrashes?: number;
  onShowFile?: (artifact: CrashArtifact) => void;
}) {
  if (artifacts.length === 0) return null;
  return (
    <section aria-labelledby="crash-files-heading" className="space-y-2" data-testid="crash-files">
      <h3 id="crash-files-heading" className="text-base font-semibold">Files</h3>
      <ul className="space-y-1">
        {artifacts.map((artifact) => (
          <li key={artifact.id} className="flex flex-wrap items-center gap-2 text-sm">
            <FileText className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
            <span className="wrap-anywhere">{artifact.name}</span>
            <span className="text-xs text-muted-foreground">{formatSize(artifact.size)}</span>
            {onShowFile !== undefined && (
              <Button type="button" variant="ghost" size="sm" onClick={() => onShowFile(artifact)}>
                Show in folder
              </Button>
            )}
          </li>
        ))}
      </ul>
      {retainedCrashes !== undefined && (
        <p className="text-xs text-muted-foreground">
          Agora keeps the last {retainedCrashes} crashes&apos; files.
        </p>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

export function english(text: LocalText | undefined): string {
  return text?.en ?? '';
}

export function ownerName(owner: CrashOwner): string {
  switch (owner.kind) {
    case 'item':
      return owner.name;
    case 'base':
      return 'the game';
    case 'generated':
      return `${owner.tool}'s output`;
    case 'writable':
      return 'a file Agora did not install';
    case 'framework':
      return owner.name;
    case 'system':
      return 'the system';
    case 'not_in_instance':
      return 'something not installed';
    default:
      return 'something Agora could not identify';
  }
}

export function ownerKey(owner: CrashOwner): string {
  switch (owner.kind) {
    case 'item':
      return `item:${owner.key}`;
    case 'generated':
      return `generated:${owner.tool}`;
    case 'framework':
      return `framework:${owner.name}`;
    default:
      return owner.kind;
  }
}

export function verdictWordFor(verdict: CrashVerdict | null): string | null {
  if (verdict === null) return null;
  switch (verdict.kind) {
    case 'crashed':
      return 'Crashed';
    case 'failed_to_start':
      return "Didn't start";
    case 'froze':
      return 'Froze';
    default:
      return null;
  }
}

/** The primary button's words, or null when the answer asks for nothing (or offers nothing). */
export function nextStepLabel(answer: CrashAnswer): string | null {
  const first = answer.candidates[0];
  switch (answer.next.kind) {
    case 'apply_repair':
      return 'Fix it';
    case 'confirm_experiment':
      return first === undefined ? 'Try the suggested step' : `Test without ${ownerName(first.owner)}`;
    case 'bisect':
      return 'Find the mod by testing halves';
    case 'recapture':
      return 'Run it again to get a better log';
    case 'restore_save':
      return 'Restore an earlier save';
    case 'second_opinion':
      return 'Get a second opinion';
    default:
      return null;
  }
}

/** The design's save-warning rule: any step that can disable or remove, or a bisect. */
export function needsSaveWarning(answer: CrashAnswer): boolean {
  if (answer.next.kind === 'bisect' || answer.next.kind === 'confirm_experiment') return true;
  if (answer.next.kind !== 'apply_repair') return false;
  return answer.repairs.some((repair) => {
    if (typeof repair !== 'object' || repair === null) return false;
    const request = (repair as { request?: unknown }).request;
    if (typeof request !== 'object' || request === null) return false;
    return 'disable_content' in (request as Record<string, unknown>);
  });
}

export function describeChange(change: CrashChange): string {
  const key = change.key ?? '';
  switch (change.kind) {
    case 'added':
      return `added ${key}`;
    case 'removed':
      return `removed ${key}`;
    case 'updated':
      return change.from_version !== undefined && change.to_version !== undefined && change.from_version !== change.to_version
        ? `updated ${key} from ${change.from_version} to ${change.to_version}`
        : `updated ${key}`;
    case 'enabled':
      return `switched on ${key}`;
    case 'disabled':
      return `switched off ${key}`;
    case 'moved':
      return `moved ${key} in the load order`;
    case 'load_order':
      return 'changed the load order';
    case 'settings':
      return "changed the instance's settings";
    default:
      return change.kind.split('_').join(' ');
  }
}

export function formatWhen(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const hours = String(date.getHours()).padStart(2, '0');
  const minutes = String(date.getMinutes()).padStart(2, '0');
  return `${date.getDate()} ${months[date.getMonth()]}, ${hours}:${minutes}`;
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function groupFindings(findings: CrashFinding[]): Array<{ cause: CrashCause; heading: string; findings: CrashFinding[] }> {
  return CAUSE_GROUPS.flatMap(({ cause, heading }) => {
    // A finding about a mod that names no candidate still reads as "Other": the group list has no
    // "A mod" heading, because suspects are shown first.
    const bucket = findings.filter((finding) => (finding.cause === cause ? true : cause === 'other' && finding.cause === 'mod'));
    return bucket.length === 0 ? [] : [{ cause, heading, findings: bucket }];
  });
}
