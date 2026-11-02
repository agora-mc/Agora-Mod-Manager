# Slice notes: on/off settings are switches

Branch `par/switches`, based on `e0bdc842` (the approved switch design). The rollout commits
(`14dc1cfb`…`2a97d511`) were already on the branch; this pass verified them, finished the gaps
(controller coverage, labels on the pre-existing Privacy switches, the small size in dense
rows), and repaired the stale switch test. The two `ds switches attempt` stashes were used as a
map and dropped.

## Controls converted from checkboxes (or hand-rolled role=switch buttons)

| File | Controls |
|---|---|
| `pages/Settings.tsx` | Show advanced settings; Auto-confirm clean installs; Always auto-confirm installs; Sort versions by date; Show changelogs before updating; Use in-app launcher; AlwaysPreTouch; Modrinth Integration; Modrinth-sourced catalog entries; GitHub-sourced catalog entries; Direct-hash catalog entries; Curated pack entries; Technic-sourced pack entries; Packs curated from content sources; Technic browsing; Allow low security downloads; Reduced security mode; AI / MCP Server |
| `pages/settings/AppearanceSettings.tsx` | Use custom block color; Use custom navigation color; Use custom background; Use custom text color; Use custom background text color; Use custom border color; Living background; Ambient sounds; Hide the standard background |
| `pages/LivingBackground.tsx` | Ambient sounds; Cursor companion; Rainbow; Hide the standard background |
| `pages/settings/GameSupportSettings.tsx` | Offer `<game>` (one per supported game row) |
| `components/ContentProvidersList.tsx` | `<provider>` content source (one per provider) |
| `components/WorldsPanel.tsx` | Share game settings; Server list |
| `pages/Browse.tsx` | Hide installed |
| `pages/Instances.tsx` | Show snapshots; Automatic memory allocation |
| `pages/InstanceEditor.tsx` | Automatic memory allocation; Automatic GC selection; Pre-touch allocated memory; Allow this Java version even when Minecraft requests a different version |
| `components/InstanceTemplatePanel.tsx` | Include this instance's Java and memory settings |
| `features/games/content/LibraryStoreSection.tsx` | Show mods for every game |
| `features/games/import/index.tsx` | Copy this profile's saves into the new instance |
| `features/games/tools/InstanceToolsTab.tsx` | Its output is a mod |
| `pages/Onboarding.tsx` | Modrinth live browsing; Technic modpacks; Low security downloads; AI / MCP Server; Direct launch (in-app launcher); Prepare Java 25 for modern Minecraft |

Pre-existing switches checked in this pass (not converted from checkboxes): Privacy endpoint list
and Lockdown Mode, PluginManager (stop all plugin code, update checks, network, per-plugin enable /
allow code), PluginSettingsForm booleans, ContentSharingSettings sharing mode, LauncherImportWizard
preserve settings, HealthDialog warning mutes.

`size="sm"` is used where a switch sits in a table or a dense row: the installed-content table,
the game-support list, the Privacy endpoint list, the content-provider list and the curated-source
list. Other settings keep the default size.

## Deliberately left as checkboxes

| File | Control | Reason |
|---|---|---|
| `pages/Library.tsx` | Unused items | selecting items from a list |
| `components/DatapackWorldsDialog.tsx` | Worlds | choosing worlds for a data pack |
| `components/installed-content/InstalledContentPanel.tsx` | Category filter, Columns, select-all, row selection | filters and bulk selection |
| `components/LauncherImportWizard.tsx` | Instance list | selecting instances to import |
| `components/OrphanCleanupDialog.tsx` | Orphan list | selecting files to remove |
| `components/UpdateChangelogDialog.tsx` | Update list | selecting updates to apply |
| `components/InstallFlow.tsx` | Optional dependencies | selecting dependencies |
| `components/DependencyPrompt.tsx` | Dependency candidates | selecting dependencies |
| `components/InstanceTemplatePanel.tsx` | Template file list | selecting files to capture |
| `features/games/tools/InstanceToolsTab.tsx` | Executable candidates | selecting executables to add |
| `pages/settings/RuntimeReclaim.tsx` | Reclaim categories | selecting categories to reclaim |
| `features/games/catalog/index.tsx` | Install even if the file differs… | explicit consent override |
| `features/games/instances/InstanceOverview.tsx` | Launch anyway | explicit consent override |
| `features/games/fomod/FomodWizard.tsx` | FOMOD options | option pickers (radio/checkbox) |
| `components/ui/dropdown-menu.tsx` | DropdownMenuCheckboxItem | menu primitive, not a setting |

## Controller and tests

- Radix renders the switch as a `<button role="switch">`, which `focusableElements` already
  includes; Accept ends in `active.click()`. No controller registration changed.
- `components/ui/switch.test.tsx`: off/on (neutral track vs accent), `size="sm"`, click, Space and
  Enter, label association, disabled, animation + reduced-motion rules.
- `features/controller/controllerNavigation.test.tsx`: navigating to a switch and toggling it with
  Accept (twice, on then off).
- `pages/Privacy.test.tsx`: every network switch has its own accessible name. Negative control:
  removing the endpoint `aria-label` made this test fail; restoring it passed.
- Existing unit and Playwright queries for converted controls now use `role="switch"`.

## Known limitation

Lockdown Mode used to paint its checked track red through
`**:data-[state=checked]:bg-red-600`. The approved `.agora-switch[data-state="checked"]` rule wins,
so it now uses the theme accent like every other switch. A danger variant needs a design decision;
the component was not changed.
