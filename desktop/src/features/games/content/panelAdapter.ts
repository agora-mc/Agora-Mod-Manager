/**
 * Maps a game instance's content rows onto the installed-content panel's rows. The panel is the
 * same component Minecraft's mod list uses, so the game only supplies the data it actually has:
 * a name, a source, an on/off state and a load-order position. Modrinth, curation, update and
 * pack columns are not offered because there is nothing behind them for a game.
 */

import type { ContentColumn, ContentPanelFeatures } from '@/components/installed-content/types';
import type { InstalledContentRow } from '@/lib/tauri';
import type { InstanceContentRow } from './api';

export const GAME_CONTENT_COLUMNS: ContentColumn[] = ['enabled', 'name', 'source', 'actions'];

export const GAME_CONTENT_COLUMN_LABELS: Partial<Record<ContentColumn, string>> = {
  name: 'Name',
};

export const GAME_CONTENT_FILTERS: ContentPanelFeatures = {
  categories: false,
  curation: false,
  source: true,
  enabled: true,
  missingFiles: false,
  group: false,
};

export function whereItGoes(row: InstanceContentRow): string {
  const mount = row.mountPath ? `the ${row.mountPath} folder` : 'the game folder';
  return row.sourcePath ? `Installs ${row.sourcePath} into ${mount}` : `Installs into ${mount}`;
}

/**
 * A game row as the shared panel reads it. `filename` is blank because a game item has no single
 * file name to show; the tab supplies `rowSubtitle` with where the item goes instead. All the
 * Minecraft-only fields stay empty, which is what keeps their columns and actions out of the way.
 */
export function toPanelRow(row: InstanceContentRow): InstalledContentRow {
  return {
    key: row.layerId,
    filename: '',
    display_name: row.name.trim() || 'Unnamed item',
    version: null,
    content_type: 'mod',
    enabled: row.enabled,
    installed_at: '',
    source: row.sourceKind,
    source_label: row.sourceLabel || 'Unknown source',
    pack_managed: false,
    installed_as_dependency: false,
    update_pinned: false,
    source_url: null,
    registry_id: null,
    modrinth_id: null,
    mod_jar_id: null,
    loader_mod_id: null,
    size_bytes: null,
    file_present: true,
    file_state: 'ok',
    resolved_path: null,
    author: null,
    categories: [],
    icon_url: null,
    curation_status: 'unknown',
    agora_score: null,
    modrinth_downloads: null,
    metadata_status: 'unavailable',
    world_sync: null,
  };
}

export function toPanelRows(rows: InstanceContentRow[]): InstalledContentRow[] {
  return rows.map(toPanelRow);
}
