/**
 * Every state of the crash answer screen (`.spike/CRASH-ANSWER-SCREEN-DESIGN.md`): a named suspect
 * with each save-risk chip, findings in every cause group, nobody named, no log, could not analyze,
 * a pasted snippet, a collapsed could-not-check, the exact save warning and the primary button.
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { CrashAnswerCard, CrashAnswerView, nextStepLabel } from './CrashAnswerView';
import type {
  CrashAnswer,
  CrashCandidate,
  CrashCause,
  CrashFinding,
  CrashLevel,
  CrashSource,
  CrashVerdict,
} from '@/features/games/instances/crashAnswerApi';

const source: CrashSource = {
  plugin_id: 'acme.harbor',
  plugin_name: 'Acme Harbor crash rules',
  plugin_version: '1.0.0',
  rule_id: 'module_faulted',
};

const suspect: CrashCandidate = {
  owner: { kind: 'item', key: 'acme.example', name: 'Example Mod' },
  level: 'very_likely',
  score: 16,
  cause: 'mod',
  families: ['execution'],
  sources: [source],
};

function finding(overrides: Partial<CrashFinding> = {}): CrashFinding {
  return {
    scope: 'root_cause',
    level: 'likely',
    cause: 'other',
    headline: { en: 'Something went wrong.' },
    source,
    ...overrides,
  };
}

function answer(overrides: Partial<CrashAnswer> = {}): CrashAnswer {
  return {
    contract: 1,
    verdict: { kind: 'crashed', how: 'pid' },
    headline: { en: 'The game crashed, and Example Mod is very likely the cause.' },
    level: 'likely',
    cause: 'mod',
    candidates: [suspect],
    findings: [finding({ candidate: suspect.owner, headline: { en: 'Example Mod failed on the stack.' } })],
    inspections: [],
    could_not_check: [],
    trust_notes: [],
    next: { kind: 'confirm_experiment', keys: ['acme.example'] },
    repairs: [],
    artifacts: [],
    ...overrides,
  };
}

type ViewProps = Parameters<typeof CrashAnswerView>[0];

function renderView(props: Omit<ViewProps, 'onCheckAnother'> & { onCheckAnother?: ViewProps['onCheckAnother'] }) {
  return render(<CrashAnswerView onCheckAnother={() => undefined} {...props} />);
}

describe('a named suspect', () => {
  it('shows each save-risk chip in the design\u2019s words', () => {
    for (const [risk, words] of [
      ['safe', 'Safe to test on your save'],
      ['risky', 'Risky for your save'],
      ['unknown', 'Back up your save first'],
    ] as const) {
      const { unmount } = renderView({
        status: 'analyzed',
        answer: answer({ candidates: [{ ...suspect, save_risk: risk }] }),
      });
      expect(screen.getByTestId('save-risk-chip')).toHaveTextContent(words);
      unmount();
    }
  });

  it('shows the level words and never says certainly', () => {
    for (const [level, words] of [
      ['not_sure', 'not sure'],
      ['possible', 'possible'],
      ['likely', 'likely'],
      ['very_likely', 'very likely'],
    ] as const) {
      const { unmount } = renderView({
        status: 'analyzed',
        answer: answer({
          level,
          candidates: [{ ...suspect, level: level as CrashLevel }],
        }),
      });
      expect(screen.getByTestId('level-chip')).toHaveTextContent(words);
      expect(screen.queryByText(/certain/i)).toBeNull();
      unmount();
    }
  });
});

describe('other findings', () => {
  it('groups them under every cause heading, in the design\u2019s order', () => {
    const causes: Array<[CrashCause, string]> = [
      ['missing_requirement', 'Something is missing'],
      ['version_mismatch', 'Built for another version'],
      ['file', 'A damaged file'],
      ['setting', 'A setting'],
      ['save', 'Your save'],
      ['memory', 'Memory'],
      ['driver', 'Graphics driver'],
      ['game_install', "The game's own files"],
      ['other', 'Other'],
    ];
    renderView({
      status: 'analyzed',
      answer: answer({
        candidates: [],
        next: { kind: 'bisect', keys: [] },
        findings: causes.map(([cause], index) =>
          finding({
            cause,
            headline: { en: `finding number ${index}` },
          }),
        ),
      }),
    });

    const groups = screen.getAllByTestId('crash-cause-group');
    expect(groups.map((group) => group.getAttribute('data-cause'))).toEqual(causes.map(([cause]) => cause));
    for (const [, heading] of causes) {
      expect(screen.getByRole('heading', { name: heading, level: 4 })).toBeInTheDocument();
    }
  });
});

describe('empty states', () => {
  it('names nobody and offers the bisect and second-opinion buttons', () => {
    renderView({
      status: 'analyzed',
      answer: answer({ candidates: [], findings: [], next: { kind: 'bisect', keys: [] } }),
    });
    expect(screen.getByTestId('crash-named-nobody')).toHaveTextContent(
      "Agora couldn't name a cause from this crash. That's common: about a third of crashes leave no clue in the log.",
    );
    expect(screen.getByRole('button', { name: 'Find the mod by testing halves' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Get a second opinion' })).toBeInTheDocument();
  });

  it('says a session left no log, with the two next steps', () => {
    renderView({ status: 'nothing_to_read', origin: 'session', reason: 'No file was found.' });
    expect(screen.getByRole('heading', { name: 'The game stopped without leaving a crash log.' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Run it again' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Check a crash log' })).toBeInTheDocument();
  });

  it('says what stopped the analysis, with its reason', () => {
    renderView({ status: 'could_not_analyze', reason: 'the parser threw' });
    expect(
      screen.getByRole('heading', { name: "Agora couldn't analyze this crash: the parser threw." }),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Check a crash log' })).toBeInTheDocument();
  });

  it('notes that a pasted snippet is all Agora read', () => {
    renderView({ status: 'analyzed', origin: 'pasted', answer: answer() });
    expect(screen.getByTestId('crash-snippet-note')).toHaveTextContent(
      'This answer came from a log you pasted. Agora read only that text.',
    );
  });
});

describe('the save warning', () => {
  const words =
    'Removing a mod partway through a playthrough can damage your save. Back up your saves before you test, or test on a new game.';

  it('shows for a test-without step and for a bisect', () => {
    for (const kind of ['confirm_experiment', 'bisect'] as const) {
      const { unmount } = renderView({
        status: 'analyzed',
        answer: answer({ next: { kind, keys: ['acme.example'] } }),
      });
      expect(screen.getByTestId('crash-save-warning')).toHaveTextContent(words);
      unmount();
    }
  });

  it('shows for a repair that disables content, and not for a memory change', () => {
    const disable = renderView({
      status: 'analyzed',
      answer: answer({
        next: { kind: 'apply_repair', keys: ['acme.example'] },
        repairs: [{ request: { disable_content: { key: 'acme.example' } }, source }],
      }),
    });
    expect(screen.getByTestId('crash-save-warning')).toBeInTheDocument();
    disable.unmount();

    renderView({
      status: 'analyzed',
      answer: answer({
        next: { kind: 'apply_repair', keys: [] },
        repairs: [{ request: { set_jvm_memory: { memory_mb: 4096 } }, source }],
      }),
    });
    expect(screen.queryByTestId('crash-save-warning')).toBeNull();
  });

  it('does not show when nothing is removed or tested', () => {
    renderView({
      status: 'analyzed',
      answer: answer({ next: { kind: 'recapture', keys: [] } }),
    });
    expect(screen.queryByTestId('crash-save-warning')).toBeNull();
  });
});

describe('the primary button', () => {
  it('uses the design\u2019s words for each next step', () => {
    const cases: Array<[CrashAnswer['next']['kind'], string | null]> = [
      ['apply_repair', 'Fix it'],
      ['confirm_experiment', 'Test without Example Mod'],
      ['bisect', 'Find the mod by testing halves'],
      ['recapture', 'Run it again to get a better log'],
      ['restore_save', 'Restore an earlier save'],
      ['second_opinion', 'Get a second opinion'],
      ['nothing_needed', null],
    ];
    for (const [kind, label] of cases) {
      expect(nextStepLabel(answer({ next: { kind, keys: ['acme.example'] } }))).toBe(label);
    }
  });

  it('reports the next step when clicked', () => {
    let called: string | null = null;
    renderView({
      status: 'analyzed',
      answer: answer({ next: { kind: 'recapture', keys: [] } }),
      onNext: (kind) => { called = kind; },
    });
    fireEvent.click(screen.getByRole('button', { name: 'Run it again to get a better log' }));
    expect(called).toBe('recapture');
  });
});

describe('could not check', () => {
  it('is collapsed until opened, then names each reason', () => {
    renderView({
      status: 'analyzed',
      answer: answer({
        could_not_check: [
          { reason: 'not_covered', subject: 'core.memory', source },
          { reason: 'code_failed', subject: 'loose_log', detail: 'the parser timed out', source },
        ],
      }),
    });
    const toggle = screen.getByRole('button', { name: /2 checks could not run/ });
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByText(/core\.memory/)).toBeNull();

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute('aria-expanded', 'true');
    expect(screen.getByText(/core\.memory/)).toBeInTheDocument();
    expect(screen.getByText(/the parser timed out/)).toBeInTheDocument();
  });
});

describe('the overview card', () => {
  it('reads verdict, top line and the two actions', () => {
    render(
      <CrashAnswerCard
        verdict={{ kind: 'crashed', how: 'pid' } as CrashVerdict}
        headline="The game crashed, and Example Mod is very likely the cause."
        status="analyzed"
        onOpen={() => undefined}
        onHide={() => undefined}
      />,
    );
    expect(screen.getByTestId('crash-card-line')).toHaveTextContent(
      'Crashed last time \u00b7 The game crashed, and Example Mod is very likely the cause.',
    );
    expect(screen.getByRole('button', { name: 'See what happened' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Hide until the next crash' })).toBeInTheDocument();
  });
});
