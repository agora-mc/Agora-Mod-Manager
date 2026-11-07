import { useEffect, useId, useState, type ReactNode } from 'react';
import { Boxes } from 'lucide-react';
import { formatError } from '../../../lib/tauri';
import {
  RECOMMENDED_GAME_SETTINGS,
  loadGameSettings,
  saveGameSetting,
  type GameSettings,
} from '../../../features/games/settings/gameSettings';
import { DEPLOYMENT_OPTIONS, baseModeCopy } from '../../../features/games/instances/instanceText';
import { SettingsSection } from '../SettingsSection';

type Load<T> =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; value: T };

export interface Choice<Value extends string> {
  value: Value;
  label: string;
}

/** The option names come from the instances pages' own copy, so the two never disagree. */
const BASE_TYPE_CHOICES: readonly Choice<GameSettings['baseType']>[] = [
  { value: 'linked', label: baseModeCopy('linked').label },
  { value: 'copied', label: baseModeCopy('copied').label },
];

const DEPLOYMENT_CHOICES: readonly Choice<GameSettings['deployment']>[] = DEPLOYMENT_OPTIONS.map((option) => ({
  value: option.id,
  label: option.label,
}));

const IMPORT_SAVES_CHOICES: readonly Choice<GameSettings['importCopySaves']>[] = [
  { value: 'off', label: 'Leave them in place' },
  { value: 'on', label: 'Copy them into the instance' },
];

/** " (recommended)" on the option Agora advises; the user's saved choice is the selected one. */
function optionLabel<Value extends string>(choice: Choice<Value>, recommended: Value): string {
  return choice.value === recommended ? `${choice.label} (recommended)` : choice.label;
}

/**
 * One per-game choice. Like Minecraft's settings, the name and explanation sit above the dropdown;
 * only a list wide enough to give the explanation real room puts the dropdown beside it. The list
 * is a size container, so this follows the card's width (a compact window, the Settings rail, the
 * text size), never squeezing the explanation to a word a line.
 * Put rows in `GameSettingList`. When the saved choice is not the recommended one, a quiet line says what
 * Agora recommends, so the advice is visible without opening the list.
 */
export function GameSettingRow<Value extends string>({
  label,
  description,
  value,
  recommended,
  choices,
  disabled,
  onChange,
}: {
  label: string;
  description: string;
  value: Value;
  recommended: Value;
  choices: readonly Choice<Value>[];
  disabled: boolean;
  onChange: (value: Value) => void;
}) {
  const id = useId();
  const descriptionId = useId();
  const recommendedChoice = choices.find((choice) => choice.value === recommended);
  return (
    <li className="grid gap-x-6 gap-y-2 px-3 py-3 @4xl:grid-cols-[minmax(0,3fr)_minmax(0,2fr)] @4xl:items-center">
      <div className="min-w-0">
        <label htmlFor={id} className="block text-sm font-medium">{label}</label>
        <p id={descriptionId} className="mt-0.5 text-xs text-muted-foreground">{description}</p>
      </div>
      <div className="min-w-0 max-w-[calc(20rem*var(--font-scale))] space-y-1 @4xl:max-w-none">
        <select
          id={id}
          aria-describedby={descriptionId}
          className="block w-full rounded-lg border border-input bg-background px-3 py-2 text-sm focus:outline-hidden focus:ring-2 focus:ring-ring disabled:opacity-60"
          value={value}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value as Value)}
        >
          {choices.map((choice) => (
            <option key={choice.value} value={choice.value}>
              {optionLabel(choice, recommended)}
            </option>
          ))}
        </select>
        {value !== recommended && recommendedChoice && (
          <p className="text-xs text-muted-foreground">Agora recommends {recommendedChoice.label}.</p>
        )}
      </div>
    </li>
  );
}

/** The bordered list `GameSettingRow`s go in; it is what the rows measure their width against. */
export function GameSettingList({ children }: { children: ReactNode }) {
  return <ul className="@container divide-y divide-border rounded-lg border border-border">{children}</ul>;
}

/**
 * The selected game's "New instances" page: the choices that fill in the create form (and the
 * Build a base form on the Bases page) for this game. Each game keeps its own, under keys that name it.
 */
export function NewInstancesSettings({ gameId, gameName }: { gameId: string; gameName: string }) {
  const [settings, setSettings] = useState<Load<GameSettings>>({ status: 'loading' });
  const [saving, setSaving] = useState<keyof GameSettings | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setSettings({ status: 'loading' });
    setSaveError(null);
    loadGameSettings(gameId)
      .then((value) => {
        if (!cancelled) setSettings({ status: 'loaded', value });
      })
      .catch((error: unknown) => {
        if (!cancelled) setSettings({ status: 'failed', message: formatError(error) });
      });
    return () => {
      cancelled = true;
    };
  }, [gameId]);

  const change = async <K extends keyof GameSettings>(name: K, value: GameSettings[K]) => {
    if (settings.status !== 'loaded') return;
    const previous = settings.value;
    setSettings({ status: 'loaded', value: { ...previous, [name]: value } });
    setSaving(name);
    setSaveError(null);
    try {
      await saveGameSetting(gameId, name, value);
    } catch (error) {
      setSettings({ status: 'loaded', value: previous });
      setSaveError(`That setting was not saved. ${formatError(error)}`);
    } finally {
      setSaving(null);
    }
  };

  const choices = settings.status === 'loaded' ? settings.value : null;

  return (
    <SettingsSection
      card="game-new-instances"
      icon={Boxes}
      title="New instances"
      description={`These fill in the form when you create an instance of ${gameName}. You can still change them there, and on each instance's page.`}
    >
      {settings.status === 'loading' && <p className="text-xs text-muted-foreground">Reading these settings…</p>}
      {settings.status === 'failed' && (
        <p role="alert" className="text-xs text-destructive">
          Agora could not read the settings for {gameName}. {settings.message}
        </p>
      )}
      {choices !== null && (
        <GameSettingList>
          <GameSettingRow
            label="Base type"
            description="Linked shares most game files with the install and takes little space. Copied keeps its own copy of the game files. “Build a base” on the Bases page starts with this choice too."
            value={choices.baseType}
            recommended={RECOMMENDED_GAME_SETTINGS.baseType}
            choices={BASE_TYPE_CHOICES}
            disabled={saving !== null}
            onChange={(value) => void change('baseType', value)}
          />
          <GameSettingRow
            label="How new instances run"
            description="Automatic uses the game's own choice. Another mode is set on each new instance and can be changed on its page."
            value={choices.deployment}
            recommended={RECOMMENDED_GAME_SETTINGS.deployment}
            choices={DEPLOYMENT_CHOICES}
            disabled={saving !== null}
            onChange={(value) => void change('deployment', value)}
          />
          <GameSettingRow
            label="Saves when importing a profile"
            description="An imported profile can keep its saves where they are, or copy them into the new instance's own save folder."
            value={choices.importCopySaves}
            recommended={RECOMMENDED_GAME_SETTINGS.importCopySaves}
            choices={IMPORT_SAVES_CHOICES}
            disabled={saving !== null}
            onChange={(value) => void change('importCopySaves', value)}
          />
        </GameSettingList>
      )}
      {saveError !== null && (
        <p role="alert" className="text-xs text-destructive">{saveError}</p>
      )}
    </SettingsSection>
  );
}
