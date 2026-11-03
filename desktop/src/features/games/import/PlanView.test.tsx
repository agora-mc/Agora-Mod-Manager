/**
 * The import preview's rows read like the Library's cards: the priority as a chip, the name, the
 * size and a quiet note, and every fact the old raw lines carried is still there.
 */
import { render, screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import type { Mo2PlanDto } from './api';
import { PlanView, skipReasonText } from './PlanView';

const plan = {
  setup: 'D:/MO2/ModOrganizer.ini',
  profile: 'Default',
  mo2GameName: 'Skyrim Special Edition',
  gameId: 'skyrim-se',
  installLocation: 'D:/Games/Skyrim',
  store: 'steam',
  instanceName: 'Default',
  mods: [
    { line: 1, folder: 'SkyUI', enabled: true, files: 12, bytes: 2 * 1024 * 1024, provenance: 'Nexus mod 12604 (not checked)', pluginFiles: ['SkyUI_SE.esp'], metaIniExcluded: true },
    { line: 2, folder: 'Old Patch', enabled: false, files: 1, bytes: 512, provenance: 'No meta.ini', pluginFiles: [], metaIniExcluded: false },
  ],
  modsEnabled: 1,
  modsDisabled: 1,
  files: 13,
  bytes: 2 * 1024 * 1024 + 512,
  metaIniExcluded: 1,
  separators: 0,
  unmanaged: [],
  problems: [
    { line: 9, entry: 'Ghost Mod', reason: "the mod's folder is missing from the mods folder; skipped" },
    { line: null, entry: 'modlist.txt', reason: 'listed twice' },
  ],
  overwrite: { present: false, files: 0, bytes: 0, generated: [], restFiles: 0 },
  plugins: { kept: true, listed: 1, active: 1, locked: [], implicitSkipped: [], notes: [] },
  localSettings: false,
  inis: [],
  saves: { local: false, files: 0, bytes: 0, note: '' },
  warnings: [],
} as unknown as Mo2PlanDto;

describe('PlanView rows', () => {
  it('shows each mod as a chip, a name, its size and a quiet provenance note', () => {
    render(<PlanView plan={plan} blockReason={null} busy={false} onImport={() => undefined} />);
    const rows = within(screen.getByRole('list', { name: 'Mods in this profile' })).getAllByRole('listitem');
    expect(rows).toHaveLength(2);
    expect(within(rows[0]).getByLabelText('Priority 1')).toHaveTextContent('1');
    expect(rows[0]).toHaveTextContent('SkyUI');
    expect(rows[0]).toHaveTextContent('12 files, 2.0 MB');
    expect(rows[0]).toHaveTextContent('Nexus mod 12604 (not checked)');
    expect(rows[0]).toHaveTextContent('1 plugin');
    expect(rows[0]).not.toHaveTextContent('line 1');
    expect(rows[1]).toHaveTextContent('Disabled');
    expect(rows[1]).toHaveTextContent('1 file,');
    expect(rows[1]).toHaveTextContent('No meta.ini');
  });

  it('lists skipped entries the same way, with their reason as a sentence', () => {
    render(<PlanView plan={plan} blockReason={null} busy={false} onImport={() => undefined} />);
    const rows = within(screen.getByTestId('import-plan-problems')).getAllByRole('listitem');
    expect(within(rows[0]).getByLabelText('Priority 9')).toBeInTheDocument();
    expect(rows[0]).toHaveTextContent('Ghost Mod');
    expect(rows[0]).toHaveTextContent("The mod's folder is missing from the mods folder");
    expect(within(rows[1]).queryByLabelText(/Priority/)).toBeNull();
    expect(rows[1]).toHaveTextContent('Listed twice');
  });

  it('drops only the trailing "skipped" from a reason', () => {
    expect(skipReasonText('the folder holds no files; skipped')).toBe('The folder holds no files');
    expect(skipReasonText('cannot be read: denied; not read')).toBe('Cannot be read: denied; not read');
  });
});
