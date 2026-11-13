/**
 * Settings that belong to one game, stored in the app's own settings table under keys that name
 * the game id. Skyrim's choices never change another game's, and a game added by a plugin gets
 * the same defaults as a built-in one.
 *
 * The defaults are the safe choices Agora has always made: a Linked base and an Automatic
 * deployment share the install instead of copying it, and imported saves stay where they are
 * until the user asks for a copy. A stored value that is not one of the choices is an error, not
 * read as the default, so a damaged setting is visible in Settings rather than silent.
 */

import { getSetting, setSetting } from '@/lib/tauri';

export type BaseType = 'linked' | 'copied';
export type InstanceDeployment = 'auto' | 'virtual' | 'links' | 'copies';
export type ImportCopySaves = 'on' | 'off';

export interface GameSettings {
  /** How a new instance's base is built, and the starting choice for "Build a base". */
  baseType: BaseType;
  /** The deployment mode preselected for a new instance. */
  deployment: InstanceDeployment;
  /** Whether an import copies the profile's saves into the new instance by default. */
  importCopySaves: ImportCopySaves;
}

export const DEFAULT_GAME_SETTINGS: GameSettings = {
  baseType: 'linked',
  deployment: 'auto',
  importCopySaves: 'off',
};

/**
 * What Agora advises for each setting, shown as "(recommended)" wherever the choice is offered.
 * Kept apart from the user's saved choice on purpose: someone who saves Copied as their default
 * still sees Linked marked as recommended. Today the advice is the same as the unset default.
 */
export const RECOMMENDED_GAME_SETTINGS: GameSettings = { ...DEFAULT_GAME_SETTINGS };

type SettingName = keyof GameSettings;

const CHOICES: { [K in SettingName]: readonly GameSettings[K][] } = {
  baseType: ['linked', 'copied'],
  deployment: ['auto', 'virtual', 'links', 'copies'],
  importCopySaves: ['on', 'off'],
};

/** The settings key for one game's setting. It always carries the game id. */
export function gameSettingKey(gameId: string, name: SettingName): string {
  return `game.${gameId}.${name}`;
}

function parseChoice<K extends SettingName>(gameId: string, name: K, value: unknown): GameSettings[K] {
  if (value === null || value === undefined) return DEFAULT_GAME_SETTINGS[name];
  const choices: readonly string[] = CHOICES[name];
  if (typeof value !== 'string' || !choices.includes(value)) {
    throw new Error(
      `${gameSettingKey(gameId, name)} is set to ${JSON.stringify(value)}, which Agora does not understand.`,
    );
  }
  return value as GameSettings[K];
}

/**
 * Read every setting for one game. A missing value is its default; a value Agora does not know is
 * an error naming the key, so Settings can show it instead of pretending the default was chosen.
 */
export async function loadGameSettings(gameId: string): Promise<GameSettings> {
  const names: SettingName[] = ['baseType', 'deployment', 'importCopySaves'];
  const values = await Promise.all(names.map((name) => getSetting(gameSettingKey(gameId, name))));
  return {
    baseType: parseChoice(gameId, 'baseType', values[0]),
    deployment: parseChoice(gameId, 'deployment', values[1]),
    importCopySaves: parseChoice(gameId, 'importCopySaves', values[2]),
  };
}

/**
 * The defaults for a game whose settings could not be read. Used only where a setting is a
 * preselection; the Settings page shows the read error itself.
 */
export async function loadGameSettingsOrDefaults(gameId: string): Promise<GameSettings> {
  try {
    return await loadGameSettings(gameId);
  } catch {
    return DEFAULT_GAME_SETTINGS;
  }
}

/** Save one setting for one game. Rejects when the settings store refuses. */
export async function saveGameSetting<K extends SettingName>(
  gameId: string,
  name: K,
  value: GameSettings[K],
): Promise<void> {
  await setSetting(gameSettingKey(gameId, name), value);
}
