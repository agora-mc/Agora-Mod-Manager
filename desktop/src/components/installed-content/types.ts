import type { InstalledContentRow, UpdateInfo } from '../../lib/tauri';
import type React from 'react';

export type InstalledContentType = 'mod' | 'resourcepack' | 'shader' | 'datapack';

export type ContentColumn =
  | 'name'
  | 'author'
  | 'source'
  | 'size'
  | 'installed'
  | 'enabled'
  | 'actions'
  | 'version'
  | 'categories'
  | 'curation'
  | 'agora_score'
  | 'modrinth_downloads'
  | 'update_status'
  | 'loader_mod_id';

export type SortColumn =
  | 'name'
  | 'author'
  | 'source'
  | 'size'
  | 'installed'
  | 'enabled'
  | 'filename'
  | 'agora_score'
  | 'modrinth_downloads'
  | 'load_order';

export interface SortState {
  column: SortColumn;
  direction: 'asc' | 'desc' | null;
}

/**
 * How the installed list is grouped. Every mode is *derived* from data the row
 * already carries, so grouping needs no stored state of its own.
 */
export type GroupMode = 'none' | 'pack' | 'category' | 'source' | 'custom';

export interface ContentGroup {
  /** Stable identity for collapse state; not shown to the user. */
  key: string;
  label: string;
  rows: InstalledContentRow[];
}

export interface ContentFilters {
  categories: string[];
  curation: string;
  source: string;
  enabled: 'all' | 'enabled' | 'disabled' | 'missing';
}

/**
 * Optional load-order mode. Minecraft does not use it; a game instance's Content tab does. The
 * panel shows each row's true position and asks the caller to perform a move through the same
 * core call the old Move up / Move down buttons used.
 */
export interface LoadOrderConfig {
  /** The row's true 1-based load-order position (1 is the first row and lowest priority). */
  positionForRow: (row: InstalledContentRow) => number;
  /** Move the row to a 1-based position. Rejecting shows the error and keeps the true order. */
  onMove: (row: InstalledContentRow, position: number) => Promise<boolean | void>;
}

/** Which of the panel's controls a caller's data can actually answer. */
export interface ContentPanelFeatures {
  categories?: boolean;
  curation?: boolean;
  source?: boolean;
  enabled?: boolean;
  /** Whether the enabled filter can offer "Missing file". Defaults to true. */
  missingFiles?: boolean;
  group?: boolean;
}

export interface InstalledContentPanelProps {
  contentType: InstalledContentType;
  rows: InstalledContentRow[];
  locked: boolean;
  /**
   * Why the panel is locked when it is not the instance's own lock: an
   * operation holding the instance or the game running. Shown instead of
   * "unlock the instance".
   */
  lockedReason?: string | null;
  onAdd?: () => void;
  addLabel?: string;
  /** Columns the caller's data can fill. Defaults to every Minecraft column. */
  availableColumns?: ContentColumn[];
  /** Column heading overrides, when the Minecraft label names something else. */
  columnLabels?: Partial<Record<ContentColumn, string>>;
  /** Columns selected when the user has no saved preference for this panel. */
  defaultColumns?: ContentColumn[];
  /** Filters the caller's data can answer. Every filter is on by default. */
  availableFilters?: ContentPanelFeatures;
  /** localStorage key for this panel's column and sort preferences. */
  preferencesKey?: string;
  /** Heading and description shown above the table, when the Minecraft wording is wrong. */
  title?: string;
  description?: string;
  /** The noun used in counts and empty states ("mods", "content"). */
  contentLabel?: string;
  /** Replaces the built-in empty state. */
  emptyState?: React.ReactNode;
  /** A line under the row name, used when the row has no filename to show. */
  rowSubtitle?: (row: InstalledContentRow) => string | null;
  /** Label for the row action that reveals a row, when it is not a file. */
  revealLabel?: string;
  /** Whether a row can be revealed. Defaults to "when it has a resolved path". */
  canRevealFile?: (row: InstalledContentRow) => boolean;
  /** The row menu's Copy filename item. On by default. */
  showCopyFilename?: boolean;
  /** The test id for each content row. */
  rowTestId?: string;
  /** Load-order mode: positions, Move up/down and drag-and-drop. */
  loadOrder?: LoadOrderConfig;
  onToggle: (row: InstalledContentRow) => Promise<boolean | void>;
  onBulkToggle: (rows: InstalledContentRow[], enabled: boolean) => Promise<boolean>;
  /**
   * Returns whether the removal was accepted, so the panel knows to clear its
   * selection. Async because confirming now happens in an in-app dialog rather
   * than a blocking `window.confirm`.
   */
  onBulkRemove: (rows: InstalledContentRow[]) => boolean | Promise<boolean>;
  onRemove: (row: InstalledContentRow) => void;
  onOpenDetails?: (row: InstalledContentRow) => void;
  onRevealFile?: (row: InstalledContentRow) => void;
  onSetCustomIcon?: (row: InstalledContentRow) => void;
  onCheckUpdates?: () => Promise<UpdateInfo[]>;
  onApplyUpdate?: (row: InstalledContentRow, update: UpdateInfo) => void;
  /** Apply every available update for this panel as one reviewed transaction. */
  onUpdateAll?: (updates: UpdateInfo[]) => void;
  /** Pin or unpin a row against updates. */
  onTogglePin?: (row: InstalledContentRow, pinned: boolean) => void;
  /** Data packs: choose which worlds a row is synced into. */
  onChooseWorlds?: (row: InstalledContentRow) => void;
  /** Data packs: copy the enabled packs into the instance's worlds now. */
  onSyncWorlds?: () => void;
  /** Open the "why is this mod here?" trace for a row. */
  onExplainPresence?: (row: InstalledContentRow) => void;
  /** User-defined groups for this instance, as group name -> filenames. */
  modGroups?: Record<string, string[]>;
  /** Open the group picker for these rows. The panel never assigns directly —
   *  choosing or creating a group is the editor's dialog to own. */
  onChooseGroup?: (rows: InstalledContentRow[]) => void;
  /**
   * Last persisted update check, read from cache so results survive navigation
   * and restart. Must be a stable reference — a fresh array each render would
   * re-seed on every parent render.
   */
  initialUpdates?: UpdateInfo[] | null;
  /**
   * True while author and display-name enrichment is still running, so rows
   * that can still gain details show a loading state rather than "Unknown".
   */
  metadataLoading?: boolean;
  onError?: (message: string) => void;
  onDrop?: React.DragEventHandler<HTMLElement>;
  extraActions?: React.ReactNode;
  iconForRow?: (row: InstalledContentRow) => string | null;
}
