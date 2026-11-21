import { useEffect, useState } from 'react';
import {
  listLoadoutProfiles,
  createLoadoutProfile,
  listLoadoutUnlistedMods,
  applyLoadoutProfile,
  deleteLoadoutProfile,
  previewLoadoutUpdate,
  updateLoadoutProfile,
  formatError,
  type LoadoutProfile,
  type LoadoutChange,
  type UnlistedModsChoice,
} from '@/lib/tauri';
import {
  Dialog,
  DialogContent,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog';

function countOf(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** Plain-words summary of what a loadout profile switches on, from the counts saved with it. */
export function describeLoadoutCoverage(prof: LoadoutProfile): string {
  if (prof.game) {
    const on = prof.game.content.filter((item) => item.enabled).length;
    const lists = prof.game.plugin_lists.map((list) => list.list);
    const parts = [countOf(on, 'content item', 'content items')];
    if (lists.length > 0) parts.push(`plugin list: ${lists.join(', ')}`);
    if (prof.game.inis.length > 0) parts.push(countOf(prof.game.inis.length, 'INI file', 'INI files'));
    if (prof.game.rules.length > 0) parts.push('load-after rules');
    parts.push(`saves: ${prof.game.saves}`);
    return `Restores: ${parts.join(', ')}.`;
  }
  const covers = prof.covers;
  if (!covers) {
    return `Saved before counts were recorded: ${countOf(prof.enabled_mods.length, 'item', 'items')} switched on. Mods it does not list are asked about when you apply; other items it does not list are left alone.`;
  }
  const parts = [
    countOf(covers.mods, 'mod', 'mods'),
    countOf(covers.resourcepacks, 'resource pack', 'resource packs'),
    countOf(covers.shaders, 'shader pack', 'shader packs'),
    countOf(covers.datapacks, 'data pack', 'data packs'),
    countOf(covers.worlds, 'world', 'worlds'),
  ];
  return `Switches on: ${parts.join(', ')}. Mods it does not list are asked about when you apply; other items it does not list are left alone.`;
}

/** One line for the Update review: what saving the profile again does to one item. */
export function describeLoadoutChange(change: LoadoutChange): string {
  const kind = change.kind;
  if (kind === 'turned_on') return `Turn on ${change.filename}`;
  if (kind === 'turned_off') return `Turn off ${change.filename}`;
  if ('added' in kind) return `Add ${change.filename} (${kind.added.enabled ? 'on' : 'off'})`;
  return `Drop ${change.filename}, which is no longer on this instance`;
}

interface LoadoutProfilesPanelProps {
  instanceId: string;
  /** `game` records plugin lists, INIs, rules and the save choice too. */
  kind?: 'minecraft' | 'game';
  disabled?: boolean;
  onStatus?: (message: string) => void;
  onError?: (message: string | null) => void;
  onApplied?: () => void | Promise<void>;
}

/**
 * The loadout picker, used by the Minecraft instance editor and by a game
 * instance's page. One component, not a copy: both call the same commands and
 * both show the same review and unlisted-item prompts.
 */
export function LoadoutProfilesPanel({
  instanceId,
  kind = 'minecraft',
  disabled = false,
  onStatus,
  onError,
  onApplied,
}: LoadoutProfilesPanelProps) {
  const [profiles, setProfiles] = useState<LoadoutProfile[]>([]);
  const [profileNameInput, setProfileNameInput] = useState('');
  const [profileBusy, setProfileBusy] = useState<string | null>(null);
  const [confirmDeleteProfile, setConfirmDeleteProfile] = useState<string | null>(null);
  const [unlistedChoice, setUnlistedChoice] = useState<{ profile: string; mods: string[] } | null>(null);
  const [updateReview, setUpdateReview] = useState<{ profile: string; changes: LoadoutChange[] } | null>(null);
  // A page that gives no callback (a game instance's Loadouts tab) still shows
  // what happened instead of swallowing it.
  const [localStatus, setLocalStatus] = useState<string | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);

  const isGame = kind === 'game';
  const reportStatus = (message: string) => {
    if (onStatus) onStatus(message);
    else setLocalStatus(message);
  };
  const reportError = (cause: unknown) => {
    const message = formatError(cause);
    if (onError) onError(message);
    else setLocalError(message);
  };
  const clearError = () => {
    if (onError) onError(null);
    else setLocalError(null);
  };

  const reloadProfiles = async () => {
    try {
      setProfiles(await listLoadoutProfiles(instanceId));
    } catch (e) {
      reportError(e);
    }
  };

  useEffect(() => {
    void reloadProfiles();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [instanceId]);

  const runProfileApply = async (name: string, unlistedMods: UnlistedModsChoice) => {
    setProfileBusy(name);
    clearError();
    try {
      await applyLoadoutProfile(instanceId, name, unlistedMods);
      await onApplied?.();
      reportStatus(
        isGame
          ? `Profile "${name}" applied. The setup it replaced was saved as the undo point.`
          : `Profile "${name}" applied. Before switching, Agora saved an undo point under Snapshots, labelled before-loadout (only the newest is kept).`,
      );
    } catch (e) {
      reportError(e);
    } finally {
      setProfileBusy(null);
    }
  };

  const requestProfileApply = async (name: string) => {
    clearError();
    setProfileBusy(name);
    const unlisted = await listLoadoutUnlistedMods(instanceId, name).catch((e: unknown) => {
      reportError(e);
      return null;
    });
    setProfileBusy(null);
    if (unlisted === null) return;
    if (unlisted.length === 0) {
      await runProfileApply(name, 'keep');
      return;
    }
    setUnlistedChoice({ profile: name, mods: unlisted });
  };

  const requestProfileUpdate = async (name: string) => {
    clearError();
    setProfileBusy(name);
    try {
      const preview = await previewLoadoutUpdate(instanceId, name);
      if (preview.changes.length === 0) {
        reportStatus(`Profile "${name}" already matches the current setup. Nothing to save.`);
      } else {
        setUpdateReview({ profile: name, changes: preview.changes });
      }
    } catch (e) {
      reportError(e);
    } finally {
      setProfileBusy(null);
    }
  };

  const saveProfileUpdate = async (name: string) => {
    setProfileBusy(name);
    clearError();
    try {
      await updateLoadoutProfile(instanceId, name);
      await reloadProfiles();
      reportStatus(`Profile "${name}" now records the current setup of this instance.`);
    } catch (e) {
      reportError(e);
    } finally {
      setProfileBusy(null);
    }
  };

  return (
    <>
      <section className="rounded-xl border border-border bg-card p-4 space-y-4">
        <div className="flex items-center justify-between">
          <h3 className="font-semibold text-sm">Loadout Profiles</h3>
          <div className="flex gap-2">
            <input
              type="text"
              value={profileNameInput}
              disabled={disabled}
              onChange={(e) => setProfileNameInput(e.target.value)}
              placeholder="Profile name…"
              className="rounded-lg border border-input bg-background px-3 py-1.5 text-sm w-48"
            />
            <button
              onClick={async () => {
                const name = profileNameInput.trim();
                if (!name) return;
                clearError();
                try {
                  await createLoadoutProfile(instanceId, name);
                  await reloadProfiles();
                  setProfileNameInput('');
                  reportStatus(`Profile "${name}" created.`);
                } catch (e) {
                  reportError(e);
                }
              }}
              disabled={disabled || !profileNameInput.trim()}
              className="rounded-lg bg-primary px-4 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90 whitespace-nowrap"
            >
              Create Profile
            </button>
          </div>
        </div>

        <p className="text-xs text-muted-foreground">
          {isGame
            ? 'A profile records which content is on, the plugin list, the INI files, the load-after rules and the save choice. Apply restores them together, after saving the setup it replaced as an undo point. Update from setup saves the profile again from what the instance has now, after showing you what would change. The game install and your own files are not part of a profile.'
            : 'A profile records which mods, resource packs, shader packs, data packs and worlds are switched on or off. Apply sets each item it records to that state. A mod it does not list, such as one added since it was saved, is asked about when you apply: keep it on, or switch it off. Packs and worlds it does not list are left as they are. Update from setup saves the profile again from what the instance has now, after showing you what would change. Configs, versions and the loader are not part of a profile.'}
        </p>

        {profiles.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No loadout profiles yet. Enter a name and click Create Profile.
          </p>
        ) : (
          <div className="space-y-2">
            {profiles.map((prof) => (
              <div key={prof.name} className="flex items-center justify-between rounded-lg border border-border px-3 py-2 text-sm">
                <div className="min-w-0 flex-1">
                  <span className="font-medium block">{prof.name}</span>
                  <span className="text-xs text-muted-foreground block">{describeLoadoutCoverage(prof)}</span>
                </div>
                <div className="flex gap-2 ml-3">
                  <button
                    onClick={() => requestProfileApply(prof.name)}
                    disabled={profileBusy === prof.name || disabled}
                    className="text-xs text-foreground hover:underline disabled:opacity-50"
                  >
                    {profileBusy === prof.name ? 'Applying…' : 'Apply'}
                  </button>
                  <button
                    onClick={() => requestProfileUpdate(prof.name)}
                    disabled={profileBusy === prof.name || disabled}
                    className="text-xs text-foreground hover:underline disabled:opacity-50"
                  >
                    Update from setup
                  </button>
                  {confirmDeleteProfile === prof.name ? (
                    <div className="flex gap-1">
                      <button
                        onClick={async () => {
                          setProfileBusy(prof.name);
                          clearError();
                          try {
                            await deleteLoadoutProfile(instanceId, prof.name);
                            await reloadProfiles();
                            setConfirmDeleteProfile(null);
                          } catch (e) {
                            reportError(e);
                          } finally {
                            setProfileBusy(null);
                          }
                        }}
                        disabled={disabled}
                        className="text-xs text-destructive font-medium disabled:opacity-50"
                      >
                        Confirm
                      </button>
                      <button
                        onClick={() => setConfirmDeleteProfile(null)}
                        disabled={disabled}
                        className="text-xs text-muted-foreground"
                      >
                        Cancel
                      </button>
                    </div>
                  ) : (
                    <button
                      onClick={() => setConfirmDeleteProfile(prof.name)}
                      disabled={disabled}
                      className="text-xs text-destructive hover:underline disabled:opacity-50"
                    >
                      Delete
                    </button>
                  )}
                </div>
              </div>
            ))}
          </div>
        )}
      </section>

      {(localStatus || localError) && (
        <p className={localError ? 'text-xs text-destructive' : 'text-xs text-muted-foreground'} role="status">
          {localError ?? localStatus}
        </p>
      )}

      <Dialog
        open={unlistedChoice !== null}
        onOpenChange={(open) => { if (!open) setUnlistedChoice(null); }}
      >
        <DialogContent className="max-h-[85vh] max-w-lg overflow-hidden flex flex-col gap-3">
          <DialogTitle className="shrink-0">Mods this profile does not list</DialogTitle>
          <DialogDescription className="shrink-0">
            {`Profile "${unlistedChoice?.profile ?? ''}" does not list ${unlistedChoice?.mods.length ?? 0} enabled ${unlistedChoice?.mods.length === 1 ? 'mod' : 'mods'}, such as ones added since it was saved. Keep them on, or switch them off?`}
          </DialogDescription>
          <ul className="min-h-0 flex-1 overflow-y-auto rounded-lg border border-border p-2 font-mono text-xs">
            {(unlistedChoice?.mods ?? []).map((mod) => (
              <li key={mod} className="truncate py-0.5">{mod}</li>
            ))}
          </ul>
          <div className="flex shrink-0 justify-end gap-2">
            <button
              type="button"
              onClick={() => setUnlistedChoice(null)}
              className="rounded-lg border border-input px-4 py-2 text-sm font-medium hover:bg-accent"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={() => {
                const choice = unlistedChoice;
                setUnlistedChoice(null);
                if (choice) void runProfileApply(choice.profile, 'keep');
              }}
              className="rounded-lg border border-input px-4 py-2 text-sm font-medium hover:bg-accent"
            >
              Keep them on
            </button>
            <button
              type="button"
              onClick={() => {
                const choice = unlistedChoice;
                setUnlistedChoice(null);
                if (choice) void runProfileApply(choice.profile, 'off');
              }}
              className="rounded-lg bg-primary px-4 py-2 text-sm font-semibold text-primary-foreground hover:bg-primary/90"
            >
              Switch them off
            </button>
          </div>
        </DialogContent>
      </Dialog>

      <Dialog
        open={updateReview !== null}
        onOpenChange={(open) => { if (!open) setUpdateReview(null); }}
      >
        <DialogContent className="max-h-[85vh] max-w-lg overflow-hidden flex flex-col gap-3">
          <DialogTitle className="shrink-0">{`Update "${updateReview?.profile ?? ''}" from the current setup?`}</DialogTitle>
          <DialogDescription className="shrink-0">
            {`Saving records everything on this instance as it is now, and drops the parts that are gone. These changes would be saved (${updateReview?.changes.length ?? 0}):`}
          </DialogDescription>
          <ul className="min-h-0 flex-1 overflow-y-auto rounded-lg border border-border p-2 text-xs">
            {(updateReview?.changes ?? []).map((change) => (
              <li key={`${change.content_type}:${change.filename}`} className="truncate py-0.5">{describeLoadoutChange(change)}</li>
            ))}
          </ul>
          <div className="flex shrink-0 justify-end gap-2">
            <button
              type="button"
              onClick={() => setUpdateReview(null)}
              className="rounded-lg border border-input px-4 py-2 text-sm font-medium hover:bg-accent"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={() => {
                const review = updateReview;
                setUpdateReview(null);
                if (review) void saveProfileUpdate(review.profile);
              }}
              className="rounded-lg bg-primary px-4 py-2 text-sm font-semibold text-primary-foreground hover:bg-primary/90"
            >
              Save changes
            </button>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
