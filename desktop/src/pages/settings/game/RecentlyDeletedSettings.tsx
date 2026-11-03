import { useEffect, useState } from 'react';
import { ArchiveRestore } from 'lucide-react';
import { formatError, getSetting, setSetting } from '../../../lib/tauri';
import { SettingsSection } from '../SettingsSection';
import { GameSettingRow, type Choice } from './NewInstancesSettings';

/** The setting core reads for the window (`game_instance::trash::RETENTION_SETTING`). */
export const RECENTLY_DELETED_DAYS_KEY = 'games_recently_deleted_days';
/** Core's default and its upper bound (`DEFAULT_RETENTION_DAYS`, `MAX_RETENTION_DAYS`). */
export const RECOMMENDED_DAYS = 30;
const MAX_DAYS = 366;

const PRESETS = [7, 14, 30, 60, 90];

/** Read the stored window the way core does: a number or a numeric string, clamped; else 30. */
export function parseRetentionDays(value: unknown): number {
  const number = typeof value === 'number'
    ? value
    : typeof value === 'string' && value.trim() !== '' ? Number(value.trim()) : Number.NaN;
  if (!Number.isFinite(number)) return RECOMMENDED_DAYS;
  return Math.min(MAX_DAYS, Math.max(1, Math.trunc(number)));
}

function dayChoices(current: number): Choice<string>[] {
  const days = PRESETS.includes(current) ? PRESETS : [...PRESETS, current].sort((a, b) => a - b);
  return days.map((count) => ({ value: String(count), label: `${count} day${count === 1 ? '' : 's'}` }));
}

/**
 * How long a deleted instance stays in Recently deleted before it moves to the Recycle Bin. Core
 * keeps one window for every game it manages (Minecraft's own list keeps 30 days), so the card
 * says so instead of pretending the choice is this game's alone.
 */
export function RecentlyDeletedSettings({ gameName }: { gameName: string }) {
  const [days, setDays] = useState<number | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    getSetting(RECENTLY_DELETED_DAYS_KEY)
      .then((value) => {
        if (!cancelled) setDays(parseRetentionDays(value));
      })
      .catch((error: unknown) => {
        if (!cancelled) setLoadError(formatError(error));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const change = async (value: string) => {
    if (days === null) return;
    const previous = days;
    const next = parseRetentionDays(value);
    setDays(next);
    setSaving(true);
    setSaveError(null);
    try {
      await setSetting(RECENTLY_DELETED_DAYS_KEY, next);
    } catch (error) {
      setDays(previous);
      setSaveError(`That setting was not saved. ${formatError(error)}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <SettingsSection
      card="game-recently-deleted"
      icon={ArchiveRestore}
      title="Recently deleted"
      description={`A deleted ${gameName} instance waits in Recently deleted, under the instance cards, where you can restore it.`}
    >
      {days === null && loadError === null && <p className="text-xs text-muted-foreground">Reading this setting…</p>}
      {loadError !== null && (
        <p role="alert" className="text-xs text-destructive">Agora could not read this setting. {loadError}</p>
      )}
      {days !== null && (
        <ul className="divide-y divide-border rounded-lg border border-border">
          <GameSettingRow
            label="Keep deleted instances for"
            description="After this, Agora moves them to your Recycle Bin. The same time applies to every game except Minecraft."
            value={String(days)}
            recommended={String(RECOMMENDED_DAYS)}
            choices={dayChoices(days)}
            disabled={saving}
            onChange={(value) => void change(value)}
          />
        </ul>
      )}
      {saveError !== null && <p role="alert" className="text-xs text-destructive">{saveError}</p>}
    </SettingsSection>
  );
}
