import { useEffect, useState } from 'react';
import { ArchiveRestore } from 'lucide-react';
import { formatError, getSetting, setSetting } from '../../lib/tauri';
import {
  RECENTLY_DELETED_DAYS_KEY,
  RECOMMENDED_RETENTION_DAYS,
  parseRetentionDays,
} from '../../lib/recentlyDeletedDays';
import { SettingsSection } from './SettingsSection';
import { GameSettingList, GameSettingRow, type Choice } from './game/NewInstancesSettings';

const PRESETS = [7, 14, 30, 60, 90];

function dayChoices(current: number): Choice<string>[] {
  const days = PRESETS.includes(current) ? PRESETS : [...PRESETS, current].sort((a, b) => a - b);
  return days.map((count) => ({ value: String(count), label: `${count} day${count === 1 ? '' : 's'}` }));
}

/**
 * How long a deleted instance stays in Recently deleted before it moves to the Recycle Bin. One
 * window for every game (Minecraft included), so it lives in General > Storage rather than on a
 * game's own page.
 */
export function RecentlyDeletedSettings() {
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
      card="recently-deleted"
      id="settings-recently-deleted"
      icon={ArchiveRestore}
      title="Recently deleted"
      description="When you delete an instance of any game, Agora keeps it in Recently deleted, below your instances, so you can restore it."
    >
      {days === null && loadError === null && <p className="text-xs text-muted-foreground">Reading this setting…</p>}
      {loadError !== null && (
        <p role="alert" className="text-xs text-destructive">Agora could not read this setting. {loadError}</p>
      )}
      {days !== null && (
        <GameSettingList>
          <GameSettingRow
            label="Keep deleted instances for"
            description="After this, Agora moves them to your Recycle Bin. The same time applies to every game, and to instances already there."
            value={String(days)}
            recommended={String(RECOMMENDED_RETENTION_DAYS)}
            choices={dayChoices(days)}
            disabled={saving}
            onChange={(value) => void change(value)}
          />
        </GameSettingList>
      )}
      {saveError !== null && <p role="alert" className="text-xs text-destructive">{saveError}</p>}
    </SettingsSection>
  );
}
