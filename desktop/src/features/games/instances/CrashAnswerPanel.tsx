/**
 * The crash answer entry point on an instance's Overview: the newest crashed session's answer as
 * a card, the instance's earlier crashes, and "Check a crash log" for text or a picked file.
 *
 * The card stays until the next session ends normally or the user hides it. The view itself is
 * presentational; this file loads the answers and decides what each action does.
 */

import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { formatError, getSetting, openExternalUrl, pickOpenFile, setSetting } from '@/lib/tauri';
import type { GameInstancePage } from '@/lib/useDestination';
import {
  CrashAnswerCard,
  CrashAnswerView,
  formatWhen,
  verdictWordFor,
} from '@/components/CrashAnswerView';
import {
  analyzeCrashFile,
  analyzeCrashText,
  getCrashAnswer,
  listCrashAnswers,
  PHOSTWOOD_ANALYZER_URL,
  readCrashArtifact,
  revealCrashArtifact,
  type CrashAnswerDetail,
  type CrashAnswerList,
  type CrashAnswerSummary,
  type SuppliedCrashAnswer,
} from './crashAnswerApi';

const DISMISS_KEY = 'crash_answer_dismissed';

function dismissKey(instanceId: string): string {
  return `${DISMISS_KEY}_${instanceId}`;
}

function summaryTopLine(summary: CrashAnswerSummary): string {
  if (summary.status === 'analyzed') return summary.headline ?? '';
  if (summary.status === 'nothing_to_read') return 'The game stopped without leaving a crash log.';
  return `Agora couldn't analyze this crash: ${summary.reason ?? 'no reason was given'}.`;
}

export function CrashAnswerOverview({
  instanceId,
  onNavigatePage,
  onRunAgain,
}: {
  instanceId: string;
  onNavigatePage?: (page: GameInstancePage) => void;
  onRunAgain?: () => void;
}) {
  const [list, setList] = useState<CrashAnswerList | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState<string | null>(null);
  const [earlierOpen, setEarlierOpen] = useState(false);
  const [openSession, setOpenSession] = useState<string | null>(null);
  const [detail, setDetail] = useState<CrashAnswerDetail | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [checkOpen, setCheckOpen] = useState(false);
  const [checkText, setCheckText] = useState('');
  const [checkBusy, setCheckBusy] = useState(false);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [supplied, setSupplied] = useState<SuppliedCrashAnswer | null>(null);
  const [suppliedOrigin, setSuppliedOrigin] = useState<'pasted' | 'file'>('pasted');
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setList(null);
    setListError(null);
    setDismissed(null);
    setNotice(null);
    Promise.all([
      listCrashAnswers(instanceId),
      getSetting(dismissKey(instanceId)).catch(() => null),
    ])
      .then(([answers, hidden]) => {
        if (!alive) return;
        setList(answers);
        setDismissed(typeof hidden === 'string' ? hidden : null);
      })
      .catch((error: unknown) => {
        if (alive) setListError(formatError(error));
      });
    return () => {
      alive = false;
    };
  }, [instanceId]);

  const latest = list?.answers[0];
  const newestNormal = list?.latestVerdict?.kind === 'normal';
  const showCard = latest !== undefined && !newestNormal && dismissed !== latest.sessionId;
  const earlier = list?.answers.slice(1) ?? [];

  const hide = () => {
    if (latest === undefined) return;
    setDismissed(latest.sessionId);
    void setSetting(dismissKey(instanceId), latest.sessionId).catch((error: unknown) => {
      setNotice(formatError(error));
    });
  };

  const openAnswer = (sessionId: string) => {
    setOpenSession(sessionId);
    setDetail(null);
    setDetailError(null);
    getCrashAnswer(sessionId)
      .then(setDetail)
      .catch((error: unknown) => setDetailError(formatError(error)));
  };

  const openCheck = () => {
    setCheckOpen(true);
    setCheckText('');
    setSupplied(null);
    setSuppliedOrigin('pasted');
    setCheckError(null);
  };

  const checkTextAnswer = async () => {
    setCheckBusy(true);
    setCheckError(null);
    try {
      setSuppliedOrigin('pasted');
      setSupplied(await analyzeCrashText(instanceId, checkText));
    } catch (error) {
      setCheckError(formatError(error));
    } finally {
      setCheckBusy(false);
    }
  };

  const checkFileAnswer = async () => {
    setCheckBusy(true);
    setCheckError(null);
    try {
      const path = await pickOpenFile('Choose a crash log', ['log', 'txt']);
      if (path === null) return;
      setSuppliedOrigin('file');
      setSupplied(await analyzeCrashFile(instanceId, path));
    } catch (error) {
      setCheckError(formatError(error));
    } finally {
      setCheckBusy(false);
    }
  };

  const askPhostwood = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setNotice('The crash log is on your clipboard. Paste it into the analyzer page.');
    } catch {
      setNotice('Agora could not copy the log. Paste it into the analyzer page yourself.');
    }
    try {
      await openExternalUrl(PHOSTWOOD_ANALYZER_URL);
    } catch (error) {
      setNotice(formatError(error));
    }
  };

  const next = (kind: string) => {
    setOpenSession(null);
    if (kind === 'recapture') {
      setNotice('Launch the game again from its page when you are ready.');
      onRunAgain?.();
      return;
    }
    if (kind === 'restore_save') {
      onNavigatePage?.('ini-saves');
      return;
    }
    if (kind === 'second_opinion') {
      openCheck();
      return;
    }
    if (kind === 'confirm_experiment' || kind === 'bisect') {
      setNotice('Open Content, disable the suspect there, and launch again.');
      onNavigatePage?.('content');
      return;
    }
    if (kind === 'apply_repair') {
      setNotice('Repairs for this game are not available yet. The findings above say what to change.');
    }
  };

  if (listError !== null) {
    return (
      <p role="alert" className="text-sm text-destructive">
        The crash answers could not be read. {listError}
      </p>
    );
  }
  if (list === null) return null;

  return (
    <div className="space-y-3" data-testid="crash-answer-panel">
      {notice !== null && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}

      {showCard && latest !== undefined && (
        <CrashAnswerCard
          verdict={latest.verdict}
          headline={latest.headline}
          reason={latest.reason}
          status={latest.status}
          onOpen={() => openAnswer(latest.sessionId)}
          onHide={hide}
        />
      )}

      {earlier.length > 0 && (
        <div className="space-y-2" data-testid="crash-earlier">
          <button
            type="button"
            aria-expanded={earlierOpen}
            onClick={() => setEarlierOpen((value) => !value)}
            className="text-sm font-medium text-muted-foreground underline-offset-4 hover:underline"
          >
            {earlierOpen ? 'Hide earlier crashes' : `Earlier crashes (${earlier.length})`}
          </button>
          {earlierOpen && (
            <ul className="divide-y divide-border rounded-lg border border-border">
              {earlier.map((summary) => (
                <li key={summary.sessionId}>
                  <button
                    type="button"
                    onClick={() => openAnswer(summary.sessionId)}
                    className="block w-full px-3 py-2 text-left text-sm hover:bg-accent"
                  >
                    <span className="font-medium">
                      {verdictWordFor(summary.verdict) ?? 'Ended'} · {formatWhen(summary.endedAt ?? summary.createdAt)}
                    </span>
                    <span className="block text-muted-foreground">{summaryTopLine(summary)}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <Button type="button" variant="outline" size="sm" onClick={openCheck}>
        Check a crash log
      </Button>

      <Dialog open={openSession !== null} onOpenChange={(open) => { if (!open) setOpenSession(null); }}>
        <DialogContent className="max-h-[85vh] max-w-3xl overflow-y-auto">
          <DialogHeader>
            <DialogTitle>Why it crashed</DialogTitle>
            <DialogDescription>What Agora found in the evidence it kept for this session.</DialogDescription>
          </DialogHeader>
          {detailError !== null && (
            <p role="alert" className="text-sm text-destructive">{detailError}</p>
          )}
          {detail === null && detailError === null && <p role="status" className="text-sm text-muted-foreground">Reading the answer…</p>}
          {detail !== null && (
            <CrashAnswerView
              status={detail.status}
              answer={detail.answer}
              reason={detail.reason}
              verdict={detail.verdict}
              endedAt={detail.endedAt}
              changes={detail.changes}
              retainedCrashes={list.retainedCrashes}
              origin="session"
              onNext={(kind) => next(kind)}
              onRunAgain={onRunAgain}
              onCheckAnother={() => { setOpenSession(null); openCheck(); }}
              onAskPhostwood={() => {
                const artifact = detail.answer?.artifacts[0];
                if (artifact === undefined) return;
                readCrashArtifact(detail.sessionId, artifact.id)
                  .then(askPhostwood)
                  .catch((error: unknown) => setNotice(formatError(error)));
              }}
              onShowFile={(artifact) => {
                void revealCrashArtifact(detail.sessionId, artifact.id).catch((error: unknown) => {
                  setNotice(formatError(error));
                });
              }}
            />
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setOpenSession(null)}>Close</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={checkOpen} onOpenChange={setCheckOpen}>
        <DialogContent className="max-h-[85vh] max-w-3xl overflow-y-auto">
          <DialogHeader>
            <DialogTitle>Check a crash log</DialogTitle>
            <DialogDescription>
              Paste the text of a crash log or choose the file. Agora reads it on this PC and sends it nowhere.
            </DialogDescription>
          </DialogHeader>
          {supplied === null ? (
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
                <Button type="button" disabled={checkBusy || checkText.trim() === ''} onClick={() => void checkTextAnswer()}>
                  {checkBusy ? 'Checking…' : 'Check this text'}
                </Button>
                <Button type="button" variant="outline" disabled={checkBusy} onClick={() => void checkFileAnswer()}>
                  Choose a file
                </Button>
              </div>
            </div>
          ) : (
            <CrashAnswerView
              status={supplied.status}
              answer={supplied.answer}
              reason={supplied.reason}
              origin={suppliedOrigin}
              onCheckAnother={() => setSupplied(null)}
              onRunAgain={onRunAgain}
              onAskPhostwood={suppliedOrigin === 'pasted' ? () => void askPhostwood(checkText) : undefined}
            />
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => setCheckOpen(false)}>Close</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
