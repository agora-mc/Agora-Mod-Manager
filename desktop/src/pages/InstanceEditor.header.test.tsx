/**
 * The instance page header and its two tab rows. The header keeps the small
 * controls (rename, loader, lock) beside the name while the rest lives in a
 * "More" menu, and every panel keeps its id under one of six top tabs.
 */
import { fireEvent, render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { InstanceEditor } from './InstanceEditor';
import { PackInstallProvider } from '../components/PackInstallProgress';
import type { HealthReport } from '../lib/tauri';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));

let locked = false;

function detail() {
  return {
    row: {
      instance_id: 'alpha', name: 'Alpha', minecraft_version: '1.21', loader: 'fabric', loader_version: '0.16.9',
      is_modpack: false, is_locked: locked, last_launched_at: null, jvm_memory_mb: 4096,
      jvm_memory_mode: 'manual', jvm_gc: 'auto', jvm_custom_args: '', jvm_always_pre_touch: true,
      created_at: '2026-01-01T00:00:00Z', java_path: null, java_incompatible_override: false,
      icon_path: null, launch_mode_override: 'auto', import_source: null,
    },
    manifest: {
      instance_id: 'alpha', name: 'Alpha', created_from_pack: null, minecraft_version: '1.21', loader: 'fabric',
      loader_version: '0.16.9', is_locked: locked, mods: [], resourcepacks: [], shaders: [], datapacks: [],
      worlds: [], user_preferences: {},
    },
    snapshot_readiness: 'ready',
    snapshot_error: null,
    pack_gaps: [],
  };
}

const clean: HealthReport = { score: 'green', scan_token: 't', blockers: [], warnings: [], recommendations: [] };

function calls(command: string) {
  return invokeMock.mock.calls.filter(([name]) => name === command);
}

beforeEach(() => {
  vi.clearAllMocks();
  locked = false;
  invokeMock.mockImplementation(async (command: string) => {
    switch (command) {
      case 'get_instance_detail': return detail();
      case 'check_instance_health': return clean;
      case 'plan_loader_change':
        return {
          instance_id: 'alpha', loader: 'fabric', minecraft_version: '1.21',
          current_loader_version: '0.16.9', recommended_loader_version: null,
          current_report: {
            current_status: 'satisfied', requirements: [], compatible_versions: [],
            indeterminate_versions: [], recommended_version: null, conflicts: [],
          },
        };
      case 'list_instance_content':
      case 'enrich_instance_content':
      case 'get_dependency_graph':
      case 'list_instance_templates':
      case 'list_snapshots':
      case 'list_loadout_profiles':
      case 'list_capturable_template_files':
      case 'list_screenshots':
        return [];
      case 'get_mod_groups': return {};
      default: return null;
    }
  });
});

function editor() {
  return <PackInstallProvider><InstanceEditor instanceId="alpha" onBack={() => undefined} /></PackInstallProvider>;
}

async function openMore() {
  await userEvent.click(await screen.findByRole('button', { name: 'More' }));
  return screen.findByRole('menu');
}

describe('instance header controls', () => {
  it('keeps rename, the loader chip, the lock and the folder action in the header', async () => {
    render(editor());

    expect(await screen.findByRole('button', { name: 'Rename' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Change loader version' })).toBeTruthy();
    expect(screen.getByRole('button', { name: /stops mod changes/ })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Open in Folder' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'More' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'Play Alpha' })).toBeTruthy();
  });

  it('offers every secondary action from the More menu', async () => {
    render(editor());
    const menu = await openMore();
    const names = within(menu).getAllByRole('menuitem').map((item) => item.textContent?.trim());
    expect(names).toEqual([
      'Add mods from a pack…',
      'Import pack as new instance',
      'Check readiness',
      'Diagnose a crash',
      'Set image',
      'Revert',
    ]);
  });

  it('hides Revert while the instance is locked', async () => {
    locked = true;
    render(editor());
    const menu = await openMore();
    expect(within(menu).queryByRole('menuitem', { name: 'Revert' })).toBeNull();
    expect(within(menu).getByRole('menuitem', { name: 'Set image' })).toHaveAttribute('aria-disabled', 'true');
  });

  it('opens the menu from the keyboard', async () => {
    render(editor());
    const more = await screen.findByRole('button', { name: 'More' });
    more.focus();
    fireEvent.keyDown(more, { key: 'Enter' });
    expect(await screen.findByRole('menuitem', { name: 'Check readiness' })).toBeTruthy();
  });

  it('runs the readiness check from the menu', async () => {
    render(editor());
    const menu = await openMore();
    await userEvent.click(within(menu).getByRole('menuitem', { name: 'Check readiness' }));
    expect(calls('check_instance_health')).toHaveLength(1);
  });

  it('opens the loader chooser from the loader chip', async () => {
    render(editor());
    fireEvent.click(await screen.findByRole('button', { name: 'Change loader version' }));
    expect(calls('plan_loader_change')).toHaveLength(1);
    expect(await screen.findByRole('dialog', { name: /Change Loader Version/i })).toBeTruthy();
  });
});

describe('instance page tabs', () => {
  it('starts on Content with its four panels, and Worlds has no second row', async () => {
    render(editor());
    const content = await screen.findByRole('button', { name: 'Content' });
    expect(content).toHaveAttribute('aria-current', 'page');
    for (const name of ['Mods (0)', 'Resource Packs (0)', 'Shaders (0)', 'Data Packs (0)']) {
      expect(screen.getByRole('button', { name })).toBeTruthy();
    }

    fireEvent.click(screen.getByRole('button', { name: 'Worlds' }));
    expect(await screen.findByRole('heading', { name: 'Worlds' })).toBeTruthy();
    // A single-panel tab shows the panel but no second row of its own.
    expect(screen.queryByRole('button', { name: 'Mods (0)' })).toBeNull();
  });

  it('groups the four backup panels under Backups & Versions', async () => {
    render(editor());
    fireEvent.click(await screen.findByRole('button', { name: 'Backups & Versions' }));
    for (const name of ['Snapshots', 'Loadout Profiles', 'Templates', 'Version Move']) {
      expect(screen.getByRole('button', { name })).toBeTruthy();
    }
    expect(screen.getByRole('button', { name: 'Backups & Versions' })).toHaveAttribute('aria-current', 'page');
  });

  it('returns to the last panel opened in a top tab', async () => {
    render(editor());
    fireEvent.click(await screen.findByRole('button', { name: 'Screenshots & Notes' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Notes' }));
    expect(screen.getByRole('button', { name: 'Notes' })).toHaveAttribute('aria-current', 'page');

    fireEvent.click(screen.getByRole('button', { name: 'Content' }));
    fireEvent.click(screen.getByRole('button', { name: 'Screenshots & Notes' }));
    expect(screen.getByRole('button', { name: 'Notes' })).toHaveAttribute('aria-current', 'page');
  });

  it('opens the first panel of a top tab that was never used', async () => {
    render(editor());
    fireEvent.click(await screen.findByRole('button', { name: 'Import & Export' }));
    expect(screen.getByRole('button', { name: 'Other launchers & files' })).toHaveAttribute('aria-current', 'page');
    expect(screen.getByRole('button', { name: 'Import & Export' })).toHaveAttribute('aria-current', 'page');
  });
});
