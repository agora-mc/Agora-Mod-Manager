/**
 * The overview's crash card: when it shows, when a normal session or a dismissal hides it, the
 * earlier-crashes list, and checking a pasted log through the same view.
 */
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { CrashAnswerOverview } from './CrashAnswerPanel';
import type { CrashAnswer, CrashAnswerDetail, CrashAnswerList, CrashAnswerSummary } from './crashAnswerApi';

const mocks = vi.hoisted(() => ({
  listCrashAnswers: vi.fn(),
  getCrashAnswer: vi.fn(),
  analyzeCrashText: vi.fn(),
  analyzeCrashFile: vi.fn(),
  revealCrashArtifact: vi.fn(),
  readCrashArtifact: vi.fn(),
  getSetting: vi.fn(),
  setSetting: vi.fn(),
  pickOpenFile: vi.fn(),
  openExternalUrl: vi.fn(),
}));

vi.mock('./crashAnswerApi', () => ({
  listCrashAnswers: mocks.listCrashAnswers,
  getCrashAnswer: mocks.getCrashAnswer,
  analyzeCrashText: mocks.analyzeCrashText,
  analyzeCrashFile: mocks.analyzeCrashFile,
  revealCrashArtifact: mocks.revealCrashArtifact,
  readCrashArtifact: mocks.readCrashArtifact,
  PHOSTWOOD_ANALYZER_URL: 'https://example.test/analyzer',
}));

vi.mock('@/lib/tauri', () => ({
  getSetting: mocks.getSetting,
  setSetting: mocks.setSetting,
  pickOpenFile: mocks.pickOpenFile,
  openExternalUrl: mocks.openExternalUrl,
  formatError: (error: unknown) => String(error),
}));

const source = {
  plugin_id: 'acme.harbor',
  plugin_name: 'Acme Harbor crash rules',
  plugin_version: '1.0.0',
};

const answer: CrashAnswer = {
  contract: 1,
  verdict: { kind: 'crashed', how: 'pid' },
  headline: { en: 'The game crashed, and Example Mod is very likely the cause.' },
  level: 'likely',
  cause: 'mod',
  candidates: [{
    owner: { kind: 'item', key: 'acme.example', name: 'Example Mod' },
    level: 'very_likely',
    score: 16,
    cause: 'mod',
    families: ['execution'],
    sources: [source],
    save_risk: 'unknown',
  }],
  findings: [],
  inspections: [],
  could_not_check: [],
  trust_notes: [],
  next: { kind: 'confirm_experiment', keys: ['acme.example'] },
  repairs: [],
  artifacts: [],
};

function summary(sessionId: string, headline: string, verdict: CrashAnswerSummary['verdict'] = { kind: 'crashed', how: 'pid' }): CrashAnswerSummary {
  return {
    sessionId,
    instanceId: 'inst',
    verdict,
    endedAt: '2026-11-12T21:14:00Z',
    createdAt: '2026-11-12T21:15:00Z',
    status: 'analyzed' as const,
    headline,
    level: 'likely' as const,
    cause: 'mod' as const,
  };
}

function list(overrides: Partial<CrashAnswerList> = {}): CrashAnswerList {
  return {
    answers: [summary('s2', 'The game crashed, and Example Mod is very likely the cause.')],
    latestVerdict: { kind: 'crashed', how: 'pid' },
    retainedCrashes: 40,
    ...overrides,
  };
}

function detail(sessionId: string): CrashAnswerDetail {
  return {
    sessionId,
    instanceId: 'inst',
    verdict: { kind: 'crashed', how: 'pid' },
    endedAt: '2026-11-12T21:14:00Z',
    createdAt: '2026-11-12T21:15:00Z',
    status: 'analyzed',
    answer,
    changes: [],
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  mocks.getSetting.mockResolvedValue(null);
  mocks.setSetting.mockResolvedValue(undefined);
  mocks.getCrashAnswer.mockImplementation(async (sessionId: string) => detail(sessionId));
});

describe('the card', () => {
  it('shows the newest answer and hides it with a setting', async () => {
    mocks.listCrashAnswers.mockResolvedValue(list());
    render(<CrashAnswerOverview instanceId="inst" />);
    expect(await screen.findByTestId('crash-answer-card')).toHaveTextContent(
      'Crashed last time · The game crashed, and Example Mod is very likely the cause.',
    );

    fireEvent.click(screen.getByRole('button', { name: 'Hide until the next crash' }));
    expect(screen.queryByTestId('crash-answer-card')).toBeNull();
    await waitFor(() => expect(mocks.setSetting).toHaveBeenCalledWith('crash_answer_dismissed_inst', 's2'));
  });

  it('stays hidden after the newest session ended normally', async () => {
    mocks.listCrashAnswers.mockResolvedValue(list({ latestVerdict: { kind: 'normal' } }));
    render(<CrashAnswerOverview instanceId="inst" />);
    await waitFor(() => expect(mocks.listCrashAnswers).toHaveBeenCalled());
    expect(screen.queryByTestId('crash-answer-card')).toBeNull();
  });

  it('stays hidden when the stored dismissal is the newest answer', async () => {
    mocks.listCrashAnswers.mockResolvedValue(list());
    mocks.getSetting.mockResolvedValue('s2');
    render(<CrashAnswerOverview instanceId="inst" />);
    await waitFor(() => expect(mocks.listCrashAnswers).toHaveBeenCalled());
    expect(screen.queryByTestId('crash-answer-card')).toBeNull();
  });
});

describe('earlier crashes', () => {
  it('lists the older sessions and opens the same view', async () => {
    mocks.listCrashAnswers.mockResolvedValue(list({
      answers: [summary('s2', 'new'), summary('s1', 'old')],
    }));
    render(<CrashAnswerOverview instanceId="inst" />);
    fireEvent.click(await screen.findByRole('button', { name: /Earlier crashes \(1\)/ }));
    fireEvent.click(screen.getByRole('button', { name: /Crashed · .*old/ }));

    await waitFor(() => expect(mocks.getCrashAnswer).toHaveBeenCalledWith('s1'));
    expect(await screen.findByTestId('crash-answer-view')).toBeInTheDocument();
  });
});

describe('check a crash log', () => {
  it('analyzes pasted text and shows the answer with the snippet note', async () => {
    mocks.listCrashAnswers.mockResolvedValue(list({ answers: [], latestVerdict: null }));
    mocks.analyzeCrashText.mockResolvedValue({ status: 'analyzed', answer });
    render(<CrashAnswerOverview instanceId="inst" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Check a crash log' }));

    fireEvent.change(screen.getByPlaceholderText('Paste the crash log here'), {
      target: { value: '== FAULTS ==\nmodule ExampleMod.dll failed\n' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Check this text' }));

    await waitFor(() => expect(mocks.analyzeCrashText).toHaveBeenCalledWith(
      'inst',
      '== FAULTS ==\nmodule ExampleMod.dll failed\n',
    ));
    expect(await screen.findByTestId('crash-snippet-note')).toBeInTheDocument();
  });

  it('shows an explicit nothing-to-read answer instead of silence', async () => {
    mocks.listCrashAnswers.mockResolvedValue(list({ answers: [], latestVerdict: null }));
    mocks.analyzeCrashText.mockResolvedValue({
      status: 'nothing_to_read',
      reason: 'no installed crash pack recognised this file',
    });
    render(<CrashAnswerOverview instanceId="inst" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Check a crash log' }));
    fireEvent.change(screen.getByPlaceholderText('Paste the crash log here'), { target: { value: 'not a crash log' } });
    fireEvent.click(screen.getByRole('button', { name: 'Check this text' }));

    expect(await screen.findByText(/Agora couldn't analyze this crash: no installed crash pack recognised this file/)).toBeInTheDocument();
  });
});

describe('second opinion', () => {
  it('copies the kept log and opens the analyzer page', async () => {
    const writeText = vi.fn(async () => undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    mocks.listCrashAnswers.mockResolvedValue(list());
    mocks.readCrashArtifact.mockResolvedValue('a crash log');
    mocks.getCrashAnswer.mockResolvedValue({
      ...detail('s2'),
      answer: {
        ...answer,
        artifacts: [{
          id: 'log-1',
          name: 'crash.log',
          size: 11,
          privacy: 'personal',
          retained: { kind: 'whole' },
        }],
      },
    });
    render(<CrashAnswerOverview instanceId="inst" />);
    fireEvent.click(await screen.findByRole('button', { name: 'See what happened' }));

    const ask = await screen.findByRole('button', { name: "Ask Phostwood's analyzer" });
    await waitFor(() => expect(ask).toBeEnabled());
    fireEvent.click(ask);

    await waitFor(() => expect(mocks.readCrashArtifact).toHaveBeenCalledWith('s2', 'log-1'));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith('a crash log'));
    await waitFor(() => expect(mocks.openExternalUrl).toHaveBeenCalledWith('https://example.test/analyzer'));
  });
});
