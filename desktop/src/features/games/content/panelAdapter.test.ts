import { describe, expect, it } from 'vitest';
import { GAME_CONTENT_COLUMNS, GAME_CONTENT_FILTERS, toPanelRow, whereItGoes } from './panelAdapter';
import type { InstanceContentRow } from './api';

function gameRow(overrides: Partial<InstanceContentRow> = {}): InstanceContentRow {
  return {
    position: 1,
    itemId: 'aaaa1111',
    layerId: 'content:aaaa1111',
    name: 'Odin Patch',
    sourceKind: 'archive',
    sourceLabel: 'Archive: odin.zip',
    enabled: true,
    mountPath: 'Data',
    sourcePath: '',
    ...overrides,
  };
}

describe('the game content panel adapter', () => {
  it('keeps the identity, name, source and on/off state the panel acts on', () => {
    const panel = toPanelRow(gameRow());
    expect(panel.key).toBe('content:aaaa1111');
    expect(panel.display_name).toBe('Odin Patch');
    expect(panel.source_label).toBe('Archive: odin.zip');
    expect(panel.enabled).toBe(true);
  });

  it('gives a row whose name is missing a readable name instead of a blank row', () => {
    const panel = toPanelRow(gameRow({ name: '   ' }));
    expect(panel.display_name).toBe('Unnamed item');
  });

  it('leaves the Minecraft-only fields empty, so their columns and actions stay out', () => {
    const panel = toPanelRow(gameRow());
    expect(panel.modrinth_id).toBeNull();
    expect(panel.curation_status).toBe('unknown');
    expect(panel.agora_score).toBeNull();
    expect(panel.modrinth_downloads).toBeNull();
    expect(panel.loader_mod_id).toBeNull();
    expect(panel.pack_managed).toBe(false);
    expect(panel.filename).toBe('');
  });

  it('offers only the columns and filters a game can fill', () => {
    expect(GAME_CONTENT_COLUMNS).toEqual(['enabled', 'name', 'source', 'actions']);
    expect(GAME_CONTENT_FILTERS).toEqual({
      categories: false,
      curation: false,
      source: true,
      enabled: true,
      missingFiles: false,
      group: false,
    });
  });

  it('says where an item goes, in the words the old tab used', () => {
    expect(whereItGoes(gameRow())).toBe('Installs into the Data folder');
    expect(whereItGoes(gameRow({ mountPath: '', sourcePath: 'textures' }))).toBe(
      'Installs textures into the game folder',
    );
    expect(whereItGoes(gameRow({ mountPath: '', sourcePath: '' }))).toBe('Installs into the game folder');
  });
});
