/**
 * The Crash Doctor is the engine answer screen plus its test-it flow. A crash
 * that a missing dependency explains should say so first, with the same
 * Reinstall the Health Check offers, and a failed health read must not look
 * like "fine". The answer itself comes from the stored session answer or from
 * a log the user supplies.
 */
import { describe, expect, it, beforeEach, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { CrashInvestigator } from './CrashInvestigator';
import type { HealthReport, InstanceDetail, InstalledMod } from '@/lib/tauri';
import type { CrashAnswer } from '@/features/games/instances/crashAnswerApi';

const tauriMocks = vi.hoisted(() => ({
  checkInstanceHealth: vi.fn(),
  getInstanceDetail: vi.fn(),
  investigateCrash: vi.fn(),
  listRawModrinthVersions: vi.fn(),
  resolveCrashSuspectFile: vi.fn(),
  getDisablePlan: vi.fn(),
  createSnapshot: vi.fn(),
  disableModForTest: vi.fn(),
  restoreSnapshot: vi.fn(),
  deleteSnapshot: vi.fn(),
  reportStillCrashing: vi.fn(),
  confirmCrashFix: vi.fn(),
  exportCrashReport: vi.fn(),
  pickOpenFile: vi.fn(),
  openExternalUrl: vi.fn(),
}));

const crashMocks = vi.hoisted(() => ({
  listCrashAnswers: vi.fn(),
  getCrashAnswer: vi.fn(),
  analyzeCrashText: vi.fn(),
  analyzeCrashFile: vi.fn(),
  revealCrashArtifact: vi.fn(),
  readCrashArtifact: vi.fn(),
}));

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(async () => undefined) }));

vi.mock('@/lib/tauri', async () => {
  const actual = await vi.importActual<typeof import('@/lib/tauri')>('@/lib/tauri');
  return { ...actual, ...tauriMocks };
});

vi.mock('../lib/tauri', async () => {
  const actual = await vi.importActual<typeof import('../lib/tauri')>('../lib/tauri');
  return { ...actual, ...tauriMocks };
});

vi.mock('@/features/games/instances/crashAnswerApi', async () => {
  const actual = await vi.importActual<typeof import('@/features/games/instances/crashAnswerApi')>(
    '@/features/games/instances/crashAnswerApi',
  );
  return { ...actual, ...crashMocks };
});

vi.mock('../features/games/instances/crashAnswerApi', async () => {
  const actual = await vi.importActual<typeof import('../features/games/instances/crashAnswerApi')>(
    '../features/games/instances/crashAnswerApi',
  );
  return { ...actual, ...crashMocks };
});

// The review is covered by its own tests; here it only has to show what it
// was asked to install.
const installFlowMock = vi.hoisted(() => ({ lastIntent: null as unknown }));
vi.mock('./InstallFlow', () => ({
  InstallFlow: ({ intent, onClose }: { intent: unknown; onClose: () => void }) => {
    installFlowMock.lastIntent = intent;
    return (
      <div role="dialog" aria-label="install review">
        <button onClick={onClose}>Close review</button>
      </div>
    );
  },
}));

function mod(overrides: Partial<InstalledMod>): InstalledMod {
  return {
    filename: 'fabric-api.jar',
    registry_id: 'fabric-api',
    modrinth_id: null,
    source: 'curated',
    version: '0.100.0',
    sha256: 'a'.repeat(64),
    installed_at: '2026-01-01T00:00:00Z',
    enabled: true,
    content_type: 'mod',
    ...overrides,
  } as InstalledMod;
}

function report(overrides: Partial<HealthReport> = {}): HealthReport {
  return {
    score: 'red',
    scan_token: 't',
    warnings: [],
    recommendations: [],
    blockers: [],
    ...overrides,
  } as HealthReport;
}

const missingProviderBlocker = {
  kind: 'missing_required_dependency',
  mod_id: 'fabric-api',
  filename: 'sodium-0.6.0.jar',
  message: 'sodium-0.6.0 needs Fabric API, whose file is missing.',
  suggested_action: 'Reinstall Fabric API from the Health Check.',
  missing_file: 'fabric-api.jar',
};

const source = {
  plugin_id: 'agora.minecraft',
  plugin_name: 'Minecraft crash rules',
  plugin_version: '0.1.0',
  rule_id: 'invented_rule',
  rule_version: 1,
};

function answer(overrides: Partial<CrashAnswer> = {}): CrashAnswer {
  return {
    contract: 1,
    verdict: { kind: 'crashed', how: 'pid' },
    headline: { en: 'The game crashed, and Example Mod is likely the cause.' },
    level: 'likely',
    cause: 'mod',
    candidates: [
      {
        owner: { kind: 'item', key: 'mods/example.jar:abc', name: 'Example Mod' },
        level: 'likely',
        score: 9,
        cause: 'mod',
        families: ['execution'],
        sources: [source],
      },
    ],
    findings: [],
    inspections: [],
    could_not_check: [],
    trust_notes: [],
    next: { kind: 'confirm_experiment', keys: ['mods/example.jar:abc'] },
    repairs: [],
    actions: [],
    artifacts: [],
    ...overrides,
  };
}

function detail(overrides: { answer?: CrashAnswer; status?: string; reason?: string; bucket?: string } = {}) {
  return {
    sessionId: 's1',
    instanceId: 'inst-1',
    verdict: { kind: 'crashed', how: 'pid' },
    endedAt: '2026-11-12T21:14:00Z',
    createdAt: '2026-11-12T21:15:00Z',
    status: overrides.status ?? 'analyzed',
    answer: overrides.answer ?? answer(),
    bucket: overrides.bucket,
    changes: [],
    reason: overrides.reason,
  };
}

function renderDoctor(overrides: { manualLogText?: string; onOpenContent?: () => void } = {}) {
  return render(
    <CrashInvestigator
      instanceId="inst-1"
      manualLogText={overrides.manualLogText}
      onClose={() => undefined}
      onLaunch={async () => 'started' as never}
      processState={{ status: 'idle' } as never}
      onOpenContent={overrides.onOpenContent}
    />,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  installFlowMock.lastIntent = null;
  tauriMocks.investigateCrash.mockResolvedValue({
    suspects: [],
    suggested_action: { kind: 'NoSuspects' },
    ruled_out: [],
  });
  tauriMocks.getInstanceDetail.mockResolvedValue({ manifest: { mods: [mod({})] } } as unknown as InstanceDetail);
  tauriMocks.resolveCrashSuspectFile.mockResolvedValue('example.jar');
  tauriMocks.getDisablePlan.mockResolvedValue({ dependents: [] });
  tauriMocks.createSnapshot.mockResolvedValue({ id: 'snap-1' });
  tauriMocks.disableModForTest.mockResolvedValue(undefined);
  tauriMocks.restoreSnapshot.mockResolvedValue(undefined);
  tauriMocks.deleteSnapshot.mockResolvedValue(undefined);
  crashMocks.listCrashAnswers.mockResolvedValue({
    answers: [{
      sessionId: 's1',
      instanceId: 'inst-1',
      verdict: { kind: 'crashed', how: 'pid' },
      endedAt: '2026-11-12T21:14:00Z',
      createdAt: '2026-11-12T21:15:00Z',
      status: 'analyzed',
      headline: 'The game crashed, and Example Mod is likely the cause.',
    }],
    latestVerdict: { kind: 'crashed', how: 'pid' },
    retainedCrashes: 40,
  });
  crashMocks.getCrashAnswer.mockResolvedValue(detail());
  crashMocks.analyzeCrashText.mockResolvedValue({ status: 'nothing_to_read', reason: 'Nothing was supplied.' });
});

describe('Crash Doctor: mod problems first', () => {
  it('lists the health blockers before anything else, with the Health Check fix', async () => {
    tauriMocks.checkInstanceHealth.mockResolvedValue(report({ blockers: [missingProviderBlocker] }));
    renderDoctor();

    expect(await screen.findByText(/Before digging into the crash: 1 mod problem to fix/)).toBeInTheDocument();
    expect(screen.getByText('sodium-0.6.0 needs Fabric API, whose file is missing.')).toBeInTheDocument();
    const reinstall = await screen.findByRole('button', { name: 'Reinstall' });
    fireEvent.click(reinstall);
    await screen.findByRole('dialog', { name: 'install review' });
    expect(installFlowMock.lastIntent).toMatchObject({
      action: { type: 'install', sourceType: 'curated', itemId: 'fabric-api', candidateVersion: '0.100.0' },
      targetInstance: 'inst-1',
    });
  });

  it('says so when the health read fails, rather than showing no problems', async () => {
    tauriMocks.checkInstanceHealth.mockRejectedValue(new Error('registry locked'));
    renderDoctor();

    expect(await screen.findByRole('alert')).toHaveTextContent(/couldn.t check this instance/);
    expect(screen.queryByText(/Before digging into the crash/)).not.toBeInTheDocument();
  });

  it('shows no problem section when the instance is healthy', async () => {
    tauriMocks.checkInstanceHealth.mockResolvedValue(report({ score: 'green' }));
    renderDoctor();

    expect(await screen.findByTestId('crash-headline')).toHaveTextContent('Example Mod is likely the cause');
    expect(screen.queryByText(/Before digging into the crash/)).not.toBeInTheDocument();
  });
});

describe('Crash Doctor: the engine answer', () => {
  it('shows the stored answer for the newest crashed session', async () => {
    tauriMocks.checkInstanceHealth.mockResolvedValue(report({ score: 'green' }));
    renderDoctor();

    expect(await screen.findByTestId('crash-headline')).toHaveTextContent('Example Mod is likely the cause');
    expect(crashMocks.listCrashAnswers).toHaveBeenCalledWith('inst-1');
    expect(crashMocks.getCrashAnswer).toHaveBeenCalledWith('s1');
  });

  it('runs a pasted log through analyze_crash_text and shows its answer', async () => {
    tauriMocks.checkInstanceHealth.mockResolvedValue(report({ score: 'green' }));
    crashMocks.listCrashAnswers.mockResolvedValue({ answers: [], latestVerdict: null, retainedCrashes: 40 });
    crashMocks.analyzeCrashText.mockResolvedValue({
      status: 'analyzed',
      answer: answer({ headline: { en: 'The pasted log names Example Mod.' } }),
      bucket: 'pasted-bucket',
    });
    renderDoctor({ manualLogText: 'invented pasted crash text' });

    expect(await screen.findByTestId('crash-headline')).toHaveTextContent('The pasted log names Example Mod.');
    expect(crashMocks.analyzeCrashText).toHaveBeenCalledWith('inst-1', 'invented pasted crash text');
  });

  it('an action button calls the existing handler', async () => {
    const onOpenContent = vi.fn();
    tauriMocks.checkInstanceHealth.mockResolvedValue(report({ score: 'green' }));
    crashMocks.getCrashAnswer.mockResolvedValue(detail({
      answer: answer({
        actions: [{
          action: { kind: 'review_mods', label: { en: 'Review installed mods' } },
          source,
        }],
      }),
    }));
    renderDoctor({ onOpenContent });

    fireEvent.click(await screen.findByRole('button', { name: 'Review installed mods' }));
    expect(onOpenContent).toHaveBeenCalledTimes(1);
  });

  it('Test without it disables the top installed item', async () => {
    tauriMocks.checkInstanceHealth.mockResolvedValue(report({ score: 'green' }));
    crashMocks.getCrashAnswer.mockResolvedValue(detail({
      answer: answer({
        candidates: [
          {
            owner: { kind: 'item', key: 'mods/example.jar:abc', name: 'Example Mod' },
            level: 'likely',
            score: 9,
            cause: 'mod',
            families: ['execution'],
            sources: [source],
          },
          {
            owner: { kind: 'framework', name: 'Invented Engine' },
            level: 'possible',
            score: 4,
            cause: 'mod',
            role: 'framework',
            families: ['execution'],
            sources: [source],
          },
        ],
      }),
    }));
    renderDoctor();

    fireEvent.click(await screen.findByRole('button', { name: /Test without Example Mod/ }));
    await waitFor(() => expect(tauriMocks.disableModForTest).toHaveBeenCalledWith('inst-1', 'example.jar'));
    expect(tauriMocks.resolveCrashSuspectFile).toHaveBeenCalledWith('inst-1', 'item:mods/example.jar:abc');

  });
});
