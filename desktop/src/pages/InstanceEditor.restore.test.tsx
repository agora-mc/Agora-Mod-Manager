/**
 * Restoring from the Minecraft instance editor.
 *
 * A restore reports a folder its snapshot captured but the game no longer declares, and this page
 * has to say so: the Recovery tab showed it, the editor did not, so the same restore was quiet in
 * one of the two places a person can invoke it (MASTER_SPEC §32.7).
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { InstanceEditor } from './InstanceEditor';
import { PackInstallProvider } from '../components/PackInstallProgress';
import { ConfirmProvider } from '../components/ui/confirm';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: vi.fn(async () => () => undefined) }));

function detail(id: string) {
  return {
    row: {
      instance_id: id, name: id, minecraft_version: '1.21', loader: 'fabric', loader_version: '0.16.9',
      is_modpack: false, is_locked: false, last_launched_at: null, jvm_memory_mb: 4096,
      jvm_memory_mode: 'manual', jvm_gc: 'auto', jvm_custom_args: '', jvm_always_pre_touch: true,
      created_at: '2026-01-01T00:00:00Z', java_path: null, java_incompatible_override: false,
      icon_path: null, launch_mode_override: 'auto', import_source: null,
    },
    manifest: {
      instance_id: id, name: id, created_from_pack: null, minecraft_version: '1.21', loader: 'fabric',
      loader_version: '0.16.9', is_locked: false, mods: [], resourcepacks: [], shaders: [], datapacks: [],
      worlds: [], user_preferences: {},
    },
    snapshot_readiness: 'ready',
    snapshot_error: null,
    pack_gaps: [],
  };
}

const SNAPSHOT = {
  id: 'snap-1', label: 'before the pack', created_at: '2026-01-01T00:00:00Z', file_count: 3,
  size_estimate: 10, origin: 'automatic', effective_origin: 'automatic', unreadable: null,
  is_pre_restore: false, is_lkg: false, is_current_lkg: false,
};

const NOTICE =
  "folder 'characters' is no longer tracked by minecraft; restored from the snapshot anyway";

beforeEach(() => {
  vi.clearAllMocks();
  invokeMock.mockImplementation(async (command: string, args: { instanceId?: string } = {}) => {
    switch (command) {
      case 'get_instance_detail': return detail(args.instanceId ?? 'alpha');
      case 'list_instance_content': return [];
      case 'enrich_instance_content': return [];
      case 'get_dependency_graph': return [];
      case 'list_loadout_profiles': return { profiles: [], damaged: [] };
      case 'list_snapshots': return [SNAPSHOT];
      case 'restore_snapshot':
        return {
          snapshotId: 'snap-1', coverage: 'exact', restoredRoots: ['mods', 'characters'],
          preservedRoots: [], notices: [NOTICE],
        };
      case 'list_instance_templates':
      case 'list_capturable_template_files':
        return [];
      case 'get_mod_groups': return {};
      default: return null;
    }
  });
});

function editor(instanceId: string) {
  return (
    <ConfirmProvider>
      <PackInstallProvider>
        <InstanceEditor instanceId={instanceId} onBack={() => undefined} />
      </PackInstallProvider>
    </ConfirmProvider>
  );
}

describe('restore from the instance editor', () => {
  it('says which folder the snapshot restored although the game no longer tracks it', async () => {
    render(editor('alpha'));
    fireEvent.click(await screen.findByRole('button', { name: 'Backups & Versions' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Snapshots' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Restore' }));

    await screen.findByText(/no longer tracked by minecraft/);
    expect(screen.getByText(/Snapshot restored\./)).toBeTruthy();
  });
});
