import { useCallback, useEffect, useRef, useState } from 'react';
import {
  confirmCrashFix,
  createSnapshot,
  deleteSnapshot,
  disableModForTest,
  exportCrashReport,
  formatError,
  getDisablePlan,
  investigateCrash,
  pickOpenFile,
  openExternalUrl,
  resolveCrashSuspectFile,
  restoreSnapshot,
  type DisablePlan,
  type InstalledMod,
  type HealthBlocker,
} from '../lib/tauri';
import {
  analyzeCrashFile,
  analyzeCrashText,
  getCrashAnswer,
  listCrashAnswers,
  PHOSTWOOD_ANALYZER_URL,
  readCrashArtifact,
  revealCrashArtifact,
  type AnswerStatus,
  type CrashActionOffer,
  type CrashAnswer,
  type CrashAnswerSummary,
  type CrashCandidate,
  type CrashChange,
  type CrashVerdict,
  type SuppliedCrashAnswer,
} from '../features/games/instances/crashAnswerApi';
import { CrashAnswerView, ownerKey, ownerName, english } from './CrashAnswerView';
import { ModBisectPanel } from './ModBisectPanel';
import { DependencyPrompt } from './DependencyPrompt';
import { Button } from '@/components/ui/button';
import { InstallFlow } from './InstallFlow';
import type { InstallIntent } from '../lib/installFlow';
import { reinstallIntent, reinstallSourceFor, type ReinstallSource } from '../lib/missingFileReinstall';
import { tryEarnInteraction } from '../features/interactive/live/interactionAchievements';
import type { ProcessState } from '../lib/useProcessController';
import type { LaunchStartOutcome } from '../lib/useProcessController';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';

interface CrashInvestigatorProps {
  instanceId: string;
  manualLogText?: string | null;
  onClose: () => void;
  /** Called to re-launch the instance after disabling a suspected mod. */
  /** Returns true only when the launch controller actually started. */
  onLaunch: (onAwaitingHealth: () => void) => Promise<LaunchStartOutcome>;
  processState: ProcessState;
  /** Navigation for the answer's own actions. Agora never runs anything outside the closed set. */
  onOpenContent?: () => void;
  onOpenJvmSettings?: () => void;
  onOpenConsole?: () => void;
  onOpenBrowse?: () => void;
}

type Origin = 'session' | 'pasted' | 'file';

/** One answer ready to render, whether it came from a stored session or a log the user supplied. */
interface LoadedAnswer {
  origin: Origin;
  status: AnswerStatus;
  answer?: CrashAnswer;
  reason?: string;
  verdict?: CrashVerdict;
  endedAt?: string | null;
  changes?: CrashChange[];
  bucket?: string;
  sessionId?: string;
}

interface PendingTest {
  filename: string;
  ownerKey: string;
  armed: boolean;
}

function isTestable(candidate: CrashCandidate): boolean {
  if (candidate.owner.kind !== 'item') return false;
  return candidate.role !== 'framework'
    && candidate.role !== 'hook_framework'
    && candidate.role !== 'system'
    && candidate.role !== 'platform';
}

function fromStored(detail: Awaited<ReturnType<typeof getCrashAnswer>>): LoadedAnswer {
  return {
    origin: 'session',
    status: detail.status,
    answer: detail.answer,
    reason: detail.reason,
    verdict: detail.verdict,
    endedAt: detail.endedAt,
    changes: detail.changes,
    bucket: detail.bucket,
    sessionId: detail.sessionId,
  };
}

function fromSupplied(supplied: SuppliedCrashAnswer, origin: Origin): LoadedAnswer {
  return {
    origin,
    status: supplied.status,
    answer: supplied.answer,
    reason: supplied.reason,
    bucket: supplied.bucket,
  };
}

async function loadStoredAnswer(instanceId: string): Promise<LoadedAnswer> {
  const list = await listCrashAnswers(instanceId);
  const newest: CrashAnswerSummary | undefined = list.answers[0];
  if (newest === undefined) {
    return {
      origin: 'session',
      status: 'nothing_to_read',
      reason: 'No crash answer is stored for this instance. Run the game and let it crash, or check a crash log.',
    };
  }
  return fromStored(await getCrashAnswer(newest.sessionId));
}

/** The answer with suspects already ruled out by a failed experiment hidden from the screen. */
function withRuledOut(answer: CrashAnswer | undefined, ruledOut: ReadonlySet<string>): CrashAnswer | undefined {
  if (answer === undefined || ruledOut.size === 0) return answer;
  return {
    ...answer,
    candidates: answer.candidates.filter((candidate) => !ruledOut.has(ownerKey(candidate.owner))),
    findings: answer.findings.filter(
      (finding) => finding.candidate === undefined || !ruledOut.has(ownerKey(finding.candidate)),
    ),
  };
}

/** Plain text for the shareable report. Findings name their source, as the screen does. */
export function renderAnswerText(answer: CrashAnswer): string {
  const lines: string[] = [];
  lines.push(english(answer.headline));
  lines.push(`Agora's confidence: ${answer.level.split('_').join(' ')}`);
  if (answer.candidates.length > 0) {
    lines.push('', 'Suspects:');
    answer.candidates.forEach((candidate, index) => {
      lines.push(`${index + 1}. ${ownerName(candidate.owner)} (${candidate.level.split('_').join(' ')})`);
      answer.findings
        .filter((finding) => finding.candidate !== undefined && ownerKey(finding.candidate) === ownerKey(candidate.owner))
        .forEach((finding) => lines.push(`   - ${english(finding.headline)} (from ${finding.source.plugin_name})`));
    });
  }
  const others = answer.findings.filter((finding) => finding.candidate === undefined);
  if (others.length > 0) {
    lines.push('', 'Other findings:');
    others.forEach((finding) => {
      lines.push(`- ${english(finding.headline)} (from ${finding.source.plugin_name})`);
    });
  }
  if (answer.could_not_check.length > 0) {
    lines.push('', 'Could not check:');
    answer.could_not_check.forEach((check) => {
      lines.push(`- ${check.subject}: ${check.reason.split('_').join(' ')}${check.detail === undefined ? '' : `: ${check.detail}`}`);
    });
  }
  return lines.join('\n');
}

/** Error banner. */
function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="rounded-xl border border-destructive/30 bg-destructive/10 p-4">
      <p className="text-sm font-semibold text-destructive">{message}</p>
    </div>
  );
}

/** Post-launch confirmation prompt. */
function FixConfirmation({
  filename,
  onFix,
  onStillCrashing,
  loading,
}: {
  filename: string;
  onFix: () => void;
  onStillCrashing: () => void;
  loading: boolean;
}) {
  return (
    <div className="rounded-xl border border-primary/30 bg-primary/10 p-4">
      <p className="text-sm font-semibold mb-3">
        Did the game start properly without &quot;{filename}&quot;?
      </p>
      <p className="text-xs text-muted-foreground mb-3">
        If the game opened and you could play, choose “Yes.” If it crashed again, choose “No.”
      </p>
      <div className="flex gap-2">
        <button
          disabled={loading}
          onClick={onFix}
          className={[
            'flex-1 rounded-lg px-3 py-2 text-sm font-medium transition-colors',
            'bg-green-600 text-white hover:bg-green-700',
            'disabled:opacity-50 disabled:cursor-not-allowed',
          ].join(' ')}
        >
          Yes, it worked
        </button>
        <button
          disabled={loading}
          onClick={onStillCrashing}
          className={[
            'flex-1 rounded-lg px-3 py-2 text-sm font-medium transition-colors',
            'bg-destructive text-destructive-foreground hover:bg-destructive/90',
            'disabled:opacity-50 disabled:cursor-not-allowed',
          ].join(' ')}
        >
          No, still crashing
        </button>
      </div>
    </div>
  );
}

function SuccessBanner({ message }: { message: string }) {
  return (
    <div className="rounded-xl border border-green-300 dark:border-green-700 bg-green-50 dark:bg-green-900/20 p-4">
      <p className="text-sm font-semibold text-green-800 dark:text-green-200">{message}</p>
    </div>
  );
}

export function CrashInvestigator({
  instanceId,
  manualLogText,
  onClose,
  onLaunch,
  processState,
  onOpenContent,
  onOpenJvmSettings,
  onOpenConsole,
  onOpenBrowse,
}: CrashInvestigatorProps) {
  const [loaded, setLoaded] = useState<LoadedAnswer | null>(null);
  const [loading, setLoading] = useState(true);
  const [flowError, setFlowError] = useState<string | null>(null);
  const [ruledOut, setRuledOut] = useState<ReadonlySet<string>>(new Set());
  const [primeSuspects, setPrimeSuspects] = useState<string[]>([]);
  const [showBisect, setShowBisect] = useState(false);
  const [lastText, setLastText] = useState<string | null>(manualLogText ?? null);
  const [lastFilePath, setLastFilePath] = useState<string | null>(null);
  const [postLaunch, setPostLaunch] = useState<PendingTest | null>(null);
  const [pendingTest, setPendingTest] = useState<PendingTest | null>(null);
  const [experimentNotice, setExperimentNotice] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [recoverySnapshotId, setRecoverySnapshotId] = useState<string | null>(null);
  const recoverySnapshotIdRef = useRef<string | null>(null);
  const [disabledByTest, setDisabledByTest] = useState<string[]>([]);
  const [disablePlanTarget, setDisablePlanTarget] = useState<{
    originalFilename: string;
    ownerKey: string;
    plan: DisablePlan;
  } | null>(null);
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState<string | null>(null);
  const [exported, setExported] = useState(false);
  const [checkOpen, setCheckOpen] = useState(false);
  const [checkText, setCheckText] = useState('');
  const [checkBusy, setCheckBusy] = useState(false);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const cancelledRef = useRef(false);
  const closeInProgressRef = useRef(false);
  const [healthBlockers, setHealthBlockers] = useState<HealthBlocker[] | 'loading' | 'failed'>('loading');
  const [manifestMods, setManifestMods] = useState<InstalledMod[] | 'loading' | 'failed'>('loading');
  const [reinstall, setReinstall] = useState<InstallIntent | null>(null);
  const [reinstallBusy, setReinstallBusy] = useState<string | null>(null);
  const [reinstallError, setReinstallError] = useState<string | null>(null);

  const refreshHealthBlockers = useCallback(async () => {
    try {
      const report = await (await import('../lib/tauri')).checkInstanceHealth(instanceId);
      setHealthBlockers(report.blockers);
    } catch {
      setHealthBlockers('failed');
    }
  }, [instanceId]);

  useEffect(() => {
    void refreshHealthBlockers();
  }, [refreshHealthBlockers]);

  useEffect(() => {
    if (!Array.isArray(healthBlockers) || !healthBlockers.some((blocker) => blocker.missing_file)) return;
    let cancelled = false;
    (async () => {
      try {
        const { getInstanceDetail } = await import('../lib/tauri');
        const detail = await getInstanceDetail(instanceId);
        if (!cancelled) setManifestMods(detail?.manifest?.mods ?? []);
      } catch {
        if (!cancelled) setManifestMods('failed');
      }
    })();
    return () => { cancelled = true; };
  }, [instanceId, healthBlockers]);

  const startHealthReinstall = async (entry: InstalledMod, source: ReinstallSource) => {
    setReinstallBusy(entry.filename);
    setReinstallError(null);
    try {
      if (source.kind === 'curated') {
        setReinstall(reinstallIntent(instanceId, 'curated', source.itemId, source.version));
        return;
      }
      const { listRawModrinthVersions } = await import('../lib/tauri');
      const versions = await listRawModrinthVersions(instanceId, source.projectId, 'mod');
      const match = versions.find((candidate) => candidate.version === source.version);
      if (!match) {
        setReinstallError(`Modrinth no longer lists version ${source.version} of ${entry.filename}. Find it in Browse instead.`);
        return;
      }
      setReinstall(reinstallIntent(instanceId, 'modrinth', source.projectId, match.version_id));
    } catch (cause) {
      setReinstallError(formatError(cause));
    } finally {
      setReinstallBusy(null);
    }
  };
  const pendingLaunchRef = useRef(false);
  const stackedHealthDialogRef = useRef(false);

  const loadInitial = useCallback(async () => {
    setLoading(true);
    setFlowError(null);
    try {
      let next: LoadedAnswer;
      if (manualLogText !== null && manualLogText !== undefined) {
        next = fromSupplied(await analyzeCrashText(instanceId, manualLogText), 'pasted');
        setLastText(manualLogText);
      } else {
        next = await loadStoredAnswer(instanceId);
      }
      if (cancelledRef.current) return;
      setLoaded(next);
      setRuledOut(new Set());
      if (next.origin === 'session') {
        void investigateCrash(instanceId)
          .then((result) => { if (!cancelledRef.current) setPrimeSuspects(result.suspects.map((suspect) => suspect.filename)); })
          .catch(() => { /* the bisect can run without a priming list */ });
      }
    } catch (cause) {
      if (!cancelledRef.current) {
        setLoaded({ origin: 'session', status: 'could_not_analyze', reason: formatError(cause) });
      }
    } finally {
      if (!cancelledRef.current) setLoading(false);
    }
  }, [instanceId, manualLogText]);

  useEffect(() => {
    cancelledRef.current = false;
    void loadInitial();
    return () => { cancelledRef.current = true; };
  }, [loadInitial]);

  const reloadAfterCrash = useCallback(async (): Promise<LoadedAnswer | null> => {
    try {
      if (loaded?.origin === 'pasted' && lastText !== null) {
        return fromSupplied(await analyzeCrashText(instanceId, lastText), 'pasted');
      }
      if (loaded?.origin === 'file' && lastFilePath !== null) {
        return fromSupplied(await analyzeCrashFile(instanceId, lastFilePath), 'file');
      }
      return await loadStoredAnswer(instanceId);
    } catch (cause) {
      setFlowError(formatError(cause));
      return null;
    }
  }, [instanceId, lastFilePath, lastText, loaded?.origin]);

  const checkTextAnswer = async (origin: Origin, text: string, filePath: string | null) => {
    setCheckBusy(true);
    setCheckError(null);
    try {
      const next = origin === 'file' && filePath !== null
        ? fromSupplied(await analyzeCrashFile(instanceId, filePath), 'file')
        : fromSupplied(await analyzeCrashText(instanceId, text), 'pasted');
      setLoaded(next);
      setRuledOut(new Set());
      setLastText(origin === 'file' ? null : text);
      setLastFilePath(filePath);
      setCheckOpen(false);
      setNotice(null);
    } catch (cause) {
      setCheckError(formatError(cause));
    } finally {
      setCheckBusy(false);
    }
  };

  const chooseCrashFile = async () => {
    const path = await pickOpenFile('Choose a crash log', ['log', 'txt']);
    if (path === null) return;
    await checkTextAnswer('file', '', path);
  };

  const ensureRecoverySnapshot = useCallback(async () => {
    if (recoverySnapshotIdRef.current) return recoverySnapshotIdRef.current;
    const snapshot = await createSnapshot(
      instanceId,
      `crash-doctor-${globalThis.crypto?.randomUUID?.() ?? Date.now()}`,
    );
    recoverySnapshotIdRef.current = snapshot.id;
    setRecoverySnapshotId(snapshot.id);
    return snapshot.id;
  }, [instanceId]);

  const clearRecoverySnapshotState = useCallback(() => {
    recoverySnapshotIdRef.current = null;
    setRecoverySnapshotId(null);
    setDisabledByTest([]);
    setPostLaunch(null);
  }, []);

  const discardRecoverySnapshot = useCallback(async () => {
    const snapshotId = recoverySnapshotIdRef.current;
    clearRecoverySnapshotState();
    if (!snapshotId) return;
    try {
      await deleteSnapshot(instanceId, snapshotId);
    } catch (cause) {
      const message = formatError(cause).toLowerCase();
      if (!message.includes('not found') && !message.includes('not_found')) {
        console.warn('Crash Doctor could not remove its completed recovery point', cause);
      }
    }
  }, [clearRecoverySnapshotState, instanceId]);

  const restoreInvestigationSnapshot = useCallback(async () => {
    const snapshotId = recoverySnapshotIdRef.current;
    if (!snapshotId) {
      throw new Error('We don’t have a backup to put back — nothing to undo.');
    }
    await restoreSnapshot(instanceId, snapshotId);
    setDisabledByTest([]);
    setPostLaunch(null);
  }, [instanceId]);

  const handleClose = useCallback(async () => {
    if (closeInProgressRef.current) return;
    closeInProgressRef.current = true;
    if (success) {
      onClose();
      closeInProgressRef.current = false;
      return;
    }
    if (!recoverySnapshotId && disabledByTest.length === 0) {
      onClose();
      closeInProgressRef.current = false;
      return;
    }
    setLoading(true);
    setFlowError(null);
    try {
      await restoreInvestigationSnapshot();
      await discardRecoverySnapshot();
      onClose();
    } catch (cause) {
      const raw = formatError(cause);
      const lower = raw.toLowerCase();
      const isMissing = lower.includes('not found') || lower.includes('not_found') || lower.includes('unavailable') || lower.includes('no backup');
      if (isMissing) {
        clearRecoverySnapshotState();
        if (!cancelledRef.current) {
          setFlowError(null);
          setLoading(false);
        }
        onClose();
        closeInProgressRef.current = false;
        return;
      }
      setFlowError(`We couldn't put your mods back the way they were: ${raw}. You can try again, or close and keep things as they are — nothing will be lost.`);
    } finally {
      if (!cancelledRef.current) setLoading(false);
      closeInProgressRef.current = false;
    }
  }, [clearRecoverySnapshotState, disabledByTest.length, discardRecoverySnapshot, onClose, recoverySnapshotId, restoreInvestigationSnapshot, success]);

  const handleDialogOpenChange = useCallback((open: boolean) => {
    if (open) return;
    if (
      pendingLaunchRef.current
      || stackedHealthDialogRef.current
      || (pendingTest && processState.healthReport)
      || reinstall !== null
      || checkOpen
    ) return;
    void handleClose();
  }, [checkOpen, handleClose, pendingTest, processState.healthReport, reinstall]);

  const beginTest = useCallback(async (candidate: CrashCandidate) => {
    if (!isTestable(candidate)) {
      setFlowError('That is part of the game or a framework, so turning it off would not tell us anything.');
      return;
    }
    const key = ownerKey(candidate.owner);
    setLoading(true);
    setFlowError(null);
    try {
      const filename = await resolveCrashSuspectFile(instanceId, key);
      tryEarnInteraction('suspect');
      const plan = await getDisablePlan(instanceId, filename);
      if (plan.dependents.length > 0) {
        setDisablePlanTarget({ originalFilename: filename, ownerKey: key, plan });
        setLoading(false);
        return;
      }
      await ensureRecoverySnapshot();
      await disableModForTest(instanceId, filename);
      setDisabledByTest([filename]);
      pendingLaunchRef.current = true;
      const launchOutcome = await onLaunch(() => {
        stackedHealthDialogRef.current = true;
        setPendingTest({ filename, ownerKey: key, armed: true });
      });
      if (!cancelledRef.current && launchOutcome !== 'failed') {
        setPendingTest({ filename, ownerKey: key, armed: true });
      } else if (!cancelledRef.current) {
        pendingLaunchRef.current = false;
        setPendingTest(null);
        await restoreInvestigationSnapshot();
        setFlowError('The game didn’t start for the test. Check the message shown, fix it, then try again.');
      }
    } catch (cause) {
      if (!cancelledRef.current) setFlowError(formatError(cause));
    } finally {
      if (!cancelledRef.current) setLoading(false);
    }
  }, [ensureRecoverySnapshot, instanceId, onLaunch, restoreInvestigationSnapshot]);

  const handleDisableConfirm = useCallback(async (selectedKeys: string[]) => {
    if (!disablePlanTarget) return;
    const { originalFilename, ownerKey: key, plan } = disablePlanTarget;
    setLoading(true);
    setFlowError(null);
    try {
      await ensureRecoverySnapshot();
      const selectedSet = new Set(selectedKeys);
      const filenames = [
        ...plan.dependents
          .filter((dependent) => selectedSet.has(dependent.mod_id))
          .map((dependent) => dependent.filename),
        originalFilename,
      ];
      for (const filename of filenames) {
        await disableModForTest(instanceId, filename);
      }
      setDisabledByTest(filenames);
      pendingLaunchRef.current = true;
      const launchOutcome = await onLaunch(() => {
        stackedHealthDialogRef.current = true;
        setPendingTest({ filename: originalFilename, ownerKey: key, armed: true });
      });
      if (!cancelledRef.current && launchOutcome !== 'failed') {
        setPendingTest({ filename: originalFilename, ownerKey: key, armed: true });
      } else if (!cancelledRef.current) {
        pendingLaunchRef.current = false;
        setPendingTest(null);
        await restoreInvestigationSnapshot();
        setFlowError('The game didn’t start for the test. Check the message shown, fix it, then try again.');
      }
    } catch (cause) {
      try {
        await restoreInvestigationSnapshot();
      } catch (restoreError) {
        if (!cancelledRef.current) {
          setFlowError(`We tried to turn off “${originalFilename}” but something went wrong and we couldn’t put your mods back: ${formatError(restoreError)}. You can close and keep things as they are.`);
        }
        return;
      }
      if (!cancelledRef.current) setFlowError(formatError(cause));
    } finally {
      if (!cancelledRef.current) {
        setDisablePlanTarget(null);
        setLoading(false);
      }
    }
  }, [disablePlanTarget, ensureRecoverySnapshot, instanceId, onLaunch, restoreInvestigationSnapshot]);

  const finishFailedLaunch = useCallback(async () => {
    try {
      await restoreInvestigationSnapshot();
      if (!cancelledRef.current) {
        pendingLaunchRef.current = false;
        globalThis.setTimeout(() => { stackedHealthDialogRef.current = false; }, 250);
        setPendingTest(null);
        setFlowError(processState.error ?? 'The game didn’t start for the test. We put your mods back how they were.');
      }
    } catch (cause) {
      if (!cancelledRef.current) setFlowError(`The test didn’t start and we couldn’t put your mods back: ${formatError(cause)}. You can close and keep things as they are.`);
    }
  }, [processState.error, restoreInvestigationSnapshot]);

  useEffect(() => {
    if (!pendingTest?.armed) return;
    let cancelled = false;
    const finishTest = async () => {
      const failedBeforeStart = (
        processState.phase === 'failed'
        && processState.instanceId === instanceId
        && !processState.healthReport
      ) || (
        processState.phase === 'idle'
        && processState.instanceId === null
      );
      if (failedBeforeStart) {
        await finishFailedLaunch();
        return;
      }

      if (processState.instanceId !== instanceId || processState.phase !== 'exited') return;
      if (processState.outcome === 'success') {
        if (!cancelled) {
          pendingLaunchRef.current = false;
          stackedHealthDialogRef.current = false;
          setPostLaunch(pendingTest);
          setPendingTest(null);
        }
        return;
      }

      if (processState.outcome === 'crash') {
        setLoading(true);
        try {
          await restoreInvestigationSnapshot();
          const next = await reloadAfterCrash();
          const nextRuledOut = new Set(ruledOut);
          nextRuledOut.add(pendingTest.ownerKey);
          // The backend records the ruled-out candidate too, so the next experiment never repeats
          // the one we just disproved.
          if (loaded?.origin === 'session') {
            await import('../lib/tauri').then(({ reportStillCrashing }) => reportStillCrashing(instanceId, pendingTest.ownerKey));
          }
          if (!cancelled && next !== null) {
            setLoaded(next);
            setRuledOut(nextRuledOut);
            setExperimentNotice(`It still crashed without “${pendingTest.filename}”, so that’s probably not the cause. We turned it back on.`);
          }
        } catch (cause) {
          if (!cancelled) setFlowError(`We couldn’t check the test result: ${formatError(cause)}. Your mods were put back.`);
        } finally {
          if (!cancelled) {
            pendingLaunchRef.current = false;
            stackedHealthDialogRef.current = false;
            setPendingTest(null);
            setLoading(false);
          }
        }
        return;
      }

      try {
        await restoreInvestigationSnapshot();
        if (!cancelled) {
          pendingLaunchRef.current = false;
          stackedHealthDialogRef.current = false;
          setExperimentNotice('We couldn’t tell if that helped — the game didn’t finish starting. We put your mods back.');
          setPendingTest(null);
        }
      } catch (cause) {
        if (!cancelled) setFlowError(`We couldn’t put your mods back after the test: ${formatError(cause)}. You can close and keep things as they are.`);
      }
    };
    void finishTest();
    return () => { cancelled = true; };
  }, [instanceId, pendingTest, processState.error, processState.healthReport, processState.instanceId, processState.outcome, processState.phase, reloadAfterCrash, restoreInvestigationSnapshot, ruledOut, loaded?.origin, finishFailedLaunch]);

  const handleFixConfirmed = useCallback(async () => {
    if (!postLaunch) return;
    setLoading(true);
    setFlowError(null);
    try {
      if (loaded?.bucket !== undefined) {
        await confirmCrashFix(loaded.bucket, postLaunch.ownerKey);
      }
      if (!cancelledRef.current) {
        await discardRecoverySnapshot();
        setSuccess(`Got it — keeping “${postLaunch.filename}” turned off fixed the crash. You can turn it back on later from your mods list.`);
        setTimeout(() => {
          if (!cancelledRef.current) onClose();
        }, 2000);
      }
    } catch (cause) {
      if (!cancelledRef.current) setFlowError(formatError(cause));
    } finally {
      if (!cancelledRef.current) setLoading(false);
    }
  }, [discardRecoverySnapshot, loaded?.bucket, onClose, postLaunch]);

  const handleStillCrashing = useCallback(async () => {
    if (!postLaunch) return;
    setLoading(true);
    setFlowError(null);
    try {
      await restoreInvestigationSnapshot();
      const next = await reloadAfterCrash();
      const nextRuledOut = new Set(ruledOut);
      nextRuledOut.add(postLaunch.ownerKey);
      if (loaded?.origin === 'session') {
        await import('../lib/tauri').then(({ reportStillCrashing }) => reportStillCrashing(instanceId, postLaunch.ownerKey));
      }
      if (!cancelledRef.current && next !== null) {
        setLoaded(next);
        setRuledOut(nextRuledOut);
        setPostLaunch(null);
        setExperimentNotice(`It still crashed without “${postLaunch.filename}”, so that’s probably not the cause. We turned it back on.`);
      }
    } catch (cause) {
      if (!cancelledRef.current) setFlowError(formatError(cause));
    } finally {
      if (!cancelledRef.current) setLoading(false);
    }
  }, [instanceId, loaded?.origin, postLaunch, reloadAfterCrash, restoreInvestigationSnapshot, ruledOut]);

  const handleExportReport = useCallback(async () => {
    if (exporting || cancelledRef.current) return;
    setExporting(true);
    setExportError(null);
    setExported(false);
    try {
      const report = await exportCrashReport({
        instance_id: instanceId ?? null,
        crash_log: loaded?.origin === 'pasted' ? lastText : null,
        answer: loaded?.answer === undefined ? null : renderAnswerText(loaded.answer),
        suspects: null,
      });
      await navigator.clipboard.writeText(report);
      if (!cancelledRef.current) setExported(true);
    } catch (cause) {
      if (!cancelledRef.current) setExportError(formatError(cause));
    } finally {
      if (!cancelledRef.current) setExporting(false);
    }
  }, [exporting, instanceId, lastText, loaded?.answer, loaded?.origin]);

  const askPhostwood = useCallback(async () => {
    try {
      let text = lastText;
      if (text === null && loaded?.sessionId !== undefined) {
        const artifact = loaded.answer?.artifacts[0];
        if (artifact !== undefined) {
          text = await readCrashArtifact(loaded.sessionId, artifact.id);
        }
      }
      if (text === null || text === undefined) {
        setNotice('Agora does not have the crash log text to copy. Open the log file and copy it yourself.');
      } else {
        await navigator.clipboard.writeText(text);
        setNotice('The crash log is on your clipboard. Paste it into the analyzer page.');
      }
    } catch {
      setNotice('Agora could not copy the log. Paste it into the analyzer page yourself.');
    }
    try {
      await openExternalUrl(PHOSTWOOD_ANALYZER_URL);
    } catch (cause) {
      setNotice(formatError(cause));
    }
  }, [lastText, loaded?.answer?.artifacts, loaded?.sessionId]);

  const handleShowFile = useCallback((artifact: { id: string }) => {
    if (loaded?.sessionId === undefined) return;
    void revealCrashArtifact(loaded.sessionId, artifact.id).catch((cause: unknown) => {
      setNotice(formatError(cause));
    });
  }, [loaded?.sessionId]);

  const handleAction = useCallback((offer: CrashActionOffer) => {
    switch (offer.action.kind) {
      case 'review_mods':
        if (onOpenContent === undefined) {
          setNotice('Open the instance’s content list to review its mods.');
          return;
        }
        onClose();
        onOpenContent();
        return;
      case 'open_jvm_settings':
        if (onOpenJvmSettings === undefined) {
          setNotice('Open the instance’s Java and memory settings.');
          return;
        }
        onClose();
        onOpenJvmSettings();
        return;
      case 'open_console':
        if (onOpenConsole === undefined) {
          setNotice('Open the instance’s console to read the launch output.');
          return;
        }
        onClose();
        onOpenConsole();
        return;
      case 'open_crash_log': {
        const artifact = loaded?.answer?.artifacts[0];
        if (artifact === undefined || loaded?.sessionId === undefined) {
          setNotice('Agora did not keep a copy of this crash log. You can check another log from this window.');
          return;
        }
        handleShowFile(artifact);
        return;
      }
      case 'install_content':
        if (onOpenBrowse === undefined) {
          setNotice('Find the missing item in Browse and install it there.');
          return;
        }
        onClose();
        onOpenBrowse();
        return;
      default:
        setNotice('Agora does not know how to do that yet.');
    }
  }, [handleShowFile, loaded?.answer?.artifacts, loaded?.sessionId, onClose, onOpenBrowse, onOpenConsole, onOpenContent, onOpenJvmSettings]);

  const displayAnswer = withRuledOut(loaded?.answer, ruledOut);

  const handleNext = useCallback((kind: string) => {
    if (kind === 'confirm_experiment') {
      const top = displayAnswer?.candidates[0];
      if (top !== undefined) void beginTest(top);
      return;
    }
    if (kind === 'bisect') {
      setShowBisect(true);
      return;
    }
    if (kind === 'second_opinion') {
      setCheckOpen(true);
      return;
    }
    if (kind === 'recapture') {
      setNotice('Launch the game again from its page when you are ready.');
      return;
    }
    if (kind === 'restore_save') {
      setNotice('Open the instance’s saves to restore an earlier one.');
      return;
    }
    if (kind === 'apply_repair') {
      setNotice('Repairs for this game are not available yet. The findings above say what to change.');
    }
  }, [beginTest, displayAnswer]);

  if (loading && loaded === null) {
    return (
      <Dialog open onOpenChange={handleDialogOpenChange}>
        <DialogContent className="max-w-5xl">
          <DialogTitle>Crash Doctor</DialogTitle>
          <DialogDescription>
            Checking your recent crash reports and game logs to figure out what went wrong.
          </DialogDescription>
          <div className="flex flex-col items-center gap-3 py-8">
            <div role="status" aria-label="Looking at your crash" className="h-8 w-8 animate-spin rounded-full border-2 border-primary border-t-transparent" />
            <p className="text-sm text-muted-foreground">Looking at your crash…</p>
          </div>
        </DialogContent>
      </Dialog>
    );
  }

  if (loaded === null) return null;

  return (
    <>
      <Dialog open={reinstall === null && !checkOpen} onOpenChange={handleDialogOpenChange}>
        <DialogContent className="max-h-[90vh] max-w-5xl overflow-hidden flex flex-col">
          <div className="flex items-start justify-between gap-4 border-b border-border pb-4 pr-6 shrink-0">
            <div className="flex-1 min-w-0">
              <DialogTitle>Crash Doctor</DialogTitle>
              <DialogDescription>
                What Agora found, and the one change worth testing next. Anything we change can be undone.
              </DialogDescription>
            </div>
            <div className="flex items-center gap-2">
              <button
                onClick={handleExportReport}
                disabled={exporting}
                className="rounded-lg bg-primary px-3 py-1.5 text-xs font-medium text-primary-foreground transition-colors hover:bg-primary/90 disabled:opacity-50"
              >
                {exporting ? 'Preparing…' : exported ? 'Copied!' : 'Copy report for help'}
              </button>
            </div>
          </div>

          <div className="flex-1 overflow-y-auto space-y-4 pr-1">
            {flowError !== null && <ErrorBanner message={flowError} />}
            {notice !== null && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}

            {healthBlockers === 'failed' && (
              <div role="alert" className="rounded-lg border border-amber-500 bg-amber-500/10 p-3 text-xs text-amber-700 dark:text-amber-300">
                We couldn’t check this instance’s mods, so a missing or broken mod may not be listed here. Open the Health Check for the full picture.
              </div>
            )}
            {Array.isArray(healthBlockers) && healthBlockers.length > 0 && (
              <section aria-label="Mod problems to fix first" className="rounded-lg border border-destructive bg-destructive/10 p-3">
                <p className="text-sm font-semibold text-destructive">
                  Before digging into the crash: {healthBlockers.length} {healthBlockers.length === 1 ? 'mod problem' : 'mod problems'} to fix
                </p>
                <ul className="mt-2 space-y-2">
                  {healthBlockers.map((blocker, index) => (
                    <li key={`${blocker.kind}:${blocker.mod_id ?? blocker.filename ?? index}`} className="text-xs">
                      <p className="text-destructive">{blocker.message}</p>
                      {blocker.suggested_action && <p className="mt-0.5 text-muted-foreground">{blocker.suggested_action}</p>}
                      {blocker.missing_file && (
                        <div className="mt-1.5 flex flex-wrap items-center gap-2">
                          {manifestMods === 'loading' ? (
                            <span className="text-muted-foreground">Checking where {blocker.missing_file} came from…</span>
                          ) : (() => {
                            const entry = Array.isArray(manifestMods)
                              ? manifestMods.find((mod) => mod.filename === blocker.missing_file) ?? null
                              : null;
                            const source = entry ? reinstallSourceFor(entry) : null;
                            return entry && source ? (
                              <Button
                                size="sm"
                                variant="outline"
                                disabled={reinstallBusy !== null}
                                title="Review a reinstall of this version"
                                onClick={() => void startHealthReinstall(entry, source)}
                              >
                                {reinstallBusy === entry.filename ? 'Preparing…' : 'Reinstall'}
                              </Button>
                            ) : (
                              <span className="text-muted-foreground">{blocker.missing_file} is missing from mods/. Find it in Browse.</span>
                            );
                          })()}
                        </div>
                      )}
                    </li>
                  ))}
                </ul>
                {reinstallError && <p role="alert" className="mt-2 text-xs text-destructive">{reinstallError}</p>}
              </section>
            )}

            {recoverySnapshotId && (
              <div className="flex items-center justify-between gap-3 rounded-lg border border-border bg-muted p-3 text-xs text-muted-foreground">
                <span>We saved a backup before changing anything. You can undo the test.</span>
                <button
                  onClick={() => {
                    void restoreInvestigationSnapshot()
                      .then(() => discardRecoverySnapshot())
                      .then(onClose)
                      .catch((cause) => {
                        const msg = formatError(cause);
                        const lower = msg.toLowerCase();
                        if (lower.includes('not found') || lower.includes('not_found') || lower.includes('unavailable') || lower.includes('no backup')) {
                          clearRecoverySnapshotState();
                          onClose();
                        } else {
                          setFlowError(`We couldn't put your mods back: ${msg}.`);
                        }
                      });
                  }}
                  disabled={loading}
                  className="shrink-0 rounded-md border border-border px-2 py-1 font-medium hover:bg-accent disabled:opacity-50"
                >
                  Undo and close
                </button>
              </div>
            )}

            <CrashAnswerView
              status={loaded.status}
              answer={displayAnswer}
              reason={loaded.reason}
              verdict={loaded.verdict}
              endedAt={loaded.endedAt}
              changes={loaded.changes}
              origin={loaded.origin === 'session' ? 'session' : loaded.origin}
              onNext={handleNext}
              onRunAgain={() => setNotice('Launch the game again from its page when you are ready.')}
              onCheckAnother={() => { setCheckOpen(true); setCheckError(null); }}
              onAskPhostwood={loaded.origin === 'session' || lastText !== null ? () => void askPhostwood() : undefined}
              onShowFile={loaded.sessionId === undefined ? undefined : handleShowFile}
              onTestWithout={(candidate) => { void beginTest(candidate); }}
              onAction={handleAction}
            />

            {pendingTest && (
              <div className="rounded-xl border border-primary/30 bg-primary/10 p-4">
                <div className="flex items-center gap-2 mb-1">
                  <div className="h-3 w-3 animate-pulse rounded-full bg-primary" />
                  <p className="text-sm font-medium">Game is running without “{pendingTest.filename}”</p>
                </div>
                <p className="text-xs text-muted-foreground">
                  Play for a bit, then close the game. When you’re back here, we’ll check if it helped.
                </p>
              </div>
            )}

            {experimentNotice && (
              <div className="rounded-xl border border-border bg-muted p-4 text-sm text-muted-foreground">
                {experimentNotice}
              </div>
            )}

            {postLaunch && (
              <FixConfirmation
                filename={postLaunch.filename}
                onFix={handleFixConfirmed}
                onStillCrashing={handleStillCrashing}
                loading={loading}
              />
            )}

            {success && <SuccessBanner message={success} />}

            {showBisect && (
              <ModBisectPanel
                instanceId={instanceId}
                primeSuspects={primeSuspects}
                onLaunch={() => onLaunch(() => { stackedHealthDialogRef.current = true; })}
              />
            )}

            <div className="rounded-xl border border-border bg-card p-4">
              <p className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
                Still stuck?
              </p>
              <p className="mt-1 text-sm text-muted-foreground">
                Copy a report of everything above — the crash log and Agora’s answer — then paste it
                to an AI assistant or share it in the Agora Discord. File paths and usernames are removed.
              </p>
              <button
                onClick={handleExportReport}
                disabled={exporting}
                className="mt-3 w-full rounded-lg border border-border bg-background px-3 py-2 text-sm font-medium transition-colors hover:bg-accent hover:text-accent-foreground disabled:opacity-50"
              >
                {exporting ? 'Preparing report…' : exported ? 'Copied to clipboard' : 'Copy crash report'}
              </button>
              {exportError && <p className="mt-2 text-xs text-destructive">{exportError}</p>}
            </div>
          </div>
        </DialogContent>
      </Dialog>

      {reinstall && (
        <InstallFlow
          open
          intent={reinstall}
          instanceName={instanceId}
          onClose={() => {
            setReinstall(null);
            void refreshHealthBlockers();
          }}
        />
      )}

      {disablePlanTarget && (
        <DependencyPrompt
          title={`“${disablePlanTarget.originalFilename}” needs other mods`}
          actionLabel="Turn off selected and try again"
          candidates={disablePlanTarget.plan.dependents.map((dependent) => ({
            key: dependent.mod_id,
            label: dependent.mod_id,
            requirement: dependent.requirement,
            source: dependent.source,
          }))}
          onConfirm={handleDisableConfirm}
          onCancel={() => setDisablePlanTarget(null)}
        />
      )}

      <Dialog open={checkOpen} onOpenChange={setCheckOpen}>
        <DialogContent className="max-h-[85vh] max-w-3xl overflow-y-auto">
          <DialogHeader>
            <DialogTitle>Check a crash log</DialogTitle>
            <DialogDescription>
              Paste the text of a crash log or choose the file. Agora reads it on this PC and sends it nowhere.
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <label className="block space-y-1 text-sm">
              <span className="font-medium">Paste a crash log</span>
              <textarea
                value={checkText}
                onChange={(event) => setCheckText(event.target.value)}
                rows={10}
                className="w-full rounded-md border border-input bg-background p-2 font-mono text-xs"
                placeholder="Paste the crash log here"
              />
            </label>
            {checkError !== null && <p role="alert" className="text-sm text-destructive">{checkError}</p>}
            <div className="flex flex-wrap gap-2">
              <Button type="button" disabled={checkBusy || checkText.trim() === ''} onClick={() => void checkTextAnswer('pasted', checkText, null)}>
                {checkBusy ? 'Checking…' : 'Check this text'}
              </Button>
              <Button type="button" variant="outline" disabled={checkBusy} onClick={() => void chooseCrashFile()}>
                Choose a file
              </Button>
            </div>
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setCheckOpen(false)}>Cancel</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
