/**
 * Importing several profiles (MASTER_SPEC §26.10). The player ticks the profiles
 * to bring in, chooses one instance with a loadout per profile (merged) or one
 * instance per profile (separate), sees Agora's suggested grouping and the bytes
 * the choice brings in against the free space, and then imports.
 */
import { useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { useConfirm } from '@/components/ui/confirm';
import { formatBytes } from '@/components/installed-content/contentTableState';
import { formatError } from '@/lib/tauri';
import { plural } from '@/features/library/format';
import type {
  ImportGrouping,
  MultiPlanDto,
  MultiRunDto,
  ProfileSuggestion,
} from './api';

interface MultiProfilesSectionProps {
  profiles: { name: string; detail?: string }[];
  onSuggest: () => Promise<ProfileSuggestion>;
  onPlan: (profiles: string[], mode: ImportGrouping) => Promise<MultiPlanDto>;
  onRun: (profiles: string[], mode: ImportGrouping) => Promise<MultiRunDto>;
}

export function MultiProfilesSection({
  profiles,
  onSuggest,
  onPlan,
  onRun,
}: MultiProfilesSectionProps) {
  const { confirm } = useConfirm();
  const [selected, setSelected] = useState<string[]>([]);
  const [mode, setMode] = useState<ImportGrouping>('merged');
  const [suggestion, setSuggestion] = useState<ProfileSuggestion | null>(null);
  const [suggestError, setSuggestError] = useState<string | null>(null);
  const [plan, setPlan] = useState<MultiPlanDto | null>(null);
  const [run, setRun] = useState<MultiRunDto | null>(null);
  const [busy, setBusy] = useState<'plan' | 'run' | null>(null);
  const [error, setError] = useState<string | null>(null);

  const toggle = (name: string) => {
    setPlan(null);
    setRun(null);
    setSelected((current) =>
      current.includes(name) ? current.filter((entry) => entry !== name) : [...current, name],
    );
  };

  const preview = async () => {
    setBusy('plan');
    setError(null);
    try {
      setPlan(await onPlan(selected, mode));
    } catch (cause) {
      setError(formatError(cause));
    } finally {
      setBusy(null);
    }
  };

  const runImport = async () => {
    if (!plan) return;
    const ok = await confirm({
      title:
        mode === 'merged'
          ? `Import ${selected.length} profiles into one instance?`
          : `Import ${selected.length} profiles as separate instances?`,
      body:
        mode === 'merged'
          ? `Agora copies the union (${formatBytes(plan.bytes)}) and creates a loadout per profile, applying the first.`
          : `Agora copies ${formatBytes(plan.bytes)} and creates one instance per profile.`,
      confirmLabel: 'Import profiles',
    });
    if (!ok) return;
    setBusy('run');
    setError(null);
    try {
      setRun(await onRun(selected, mode));
    } catch (cause) {
      setError(formatError(cause));
    } finally {
      setBusy(null);
    }
  };

  if (profiles.length < 2) return null;

  return (
    <div className="mt-3 space-y-3 rounded-xl border border-border bg-background/60 p-3" data-testid="multi-profiles">
      <div>
        <h4 className="text-sm font-semibold">Several profiles</h4>
        <p className="text-xs text-muted-foreground">
          Tick the profiles to bring in. Merged makes one instance holding all of them and a loadout
          per profile; separate makes one instance per profile with only that profile’s mods.
        </p>
      </div>
      <ul className="space-y-1">
        {profiles.map((profile) => (
          <li key={profile.name}>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                data-testid={`multi-profile-${profile.name}`}
                checked={selected.includes(profile.name)}
                onChange={() => toggle(profile.name)}
              />
              <span>{profile.name}</span>
              {profile.detail && <span className="text-xs text-muted-foreground">{profile.detail}</span>}
            </label>
          </li>
        ))}
      </ul>
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          size="sm"
          variant={mode === 'merged' ? 'default' : 'outline'}
          aria-pressed={mode === 'merged'}
          onClick={() => {
            setMode('merged');
            setPlan(null);
            setRun(null);
          }}
        >
          One instance, a loadout per profile
        </Button>
        <Button
          type="button"
          size="sm"
          variant={mode === 'separate' ? 'default' : 'outline'}
          aria-pressed={mode === 'separate'}
          onClick={() => {
            setMode('separate');
            setPlan(null);
            setRun(null);
          }}
        >
          One instance per profile
        </Button>
      </div>

      {suggestError && <p className="text-xs text-foreground">Agora could not group the profiles: {suggestError}</p>}
      {suggestion && (
        <div className="space-y-1 text-xs text-muted-foreground" data-testid="multi-suggestion">
          <p>{suggestion.note}</p>
          {suggestion.groups.map((group, index) => (
            <p key={group.profiles.join(',')}>
              <span className="font-medium text-foreground">Group {index + 1}:</span> {group.profiles.join(', ')}
            </p>
          ))}
          {suggestion.pairs.map((pair) => (
            <p key={`${pair.a}:${pair.b}`}>
              {pair.a} &amp; {pair.b}: {pair.shared} shared of {pair.union} ({Math.round(pair.ratio * 100)}%)
            </p>
          ))}
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={busy !== null}
          onClick={() => {
            setSuggestError(null);
            onSuggest()
              .then(setSuggestion)
              .catch((cause: unknown) => setSuggestError(formatError(cause)));
          }}
        >
          Compare profiles
        </Button>
        <Button type="button" size="sm" variant="outline" disabled={selected.length === 0 || busy !== null} onClick={() => void preview()}>
          {busy === 'plan' ? 'Planning…' : 'Preview the choice'}
        </Button>
        <Button type="button" size="sm" disabled={!plan || !plan.fits || busy !== null} onClick={() => void runImport()}>
          {busy === 'run' ? 'Importing…' : `Import ${selected.length} profile${selected.length === 1 ? '' : 's'}`}
        </Button>
      </div>
      {error && <p className="text-xs text-foreground" role="alert">{error}</p>}

      {plan && (
        <div className="space-y-1 text-xs text-muted-foreground" data-testid="multi-plan">
          <p>
            {plural(plan.profiles.length, 'profile')} · {formatBytes(plan.bytes)}
            {plan.freeBytes !== null ? ` · ${formatBytes(plan.freeBytes)} free` : ''} ·{' '}
            <span className={plan.fits ? 'text-foreground' : 'font-medium text-foreground'}>
              {plan.fits ? 'it fits' : 'it does not fit, so the import is refused'}
            </span>
          </p>
          {plan.plans.map((entry) => (
            <p key={entry.profile}>
              {entry.profile}: {plural(entry.enabled, 'enabled mod')}, {plural(entry.files, 'file')}, {formatBytes(entry.bytes)}
            </p>
          ))}
          {plan.warnings.map((warning) => (
            <p key={warning} role="status">{warning}</p>
          ))}
        </div>
      )}

      {run && (
        <div className="space-y-1 text-xs text-muted-foreground" data-testid="multi-run">
          <p className="text-sm font-medium text-foreground">
            {run.instances.length === 1 ? 'One instance imported.' : `${run.instances.length} instances imported.`}
          </p>
          {run.instances.map((instance) => (
            <p key={instance} className="font-mono">{instance}</p>
          ))}
          {run.loadouts.length > 0 && <p>Loadouts: {run.loadouts.join(', ')}</p>}
        </div>
      )}
    </div>
  );
}
