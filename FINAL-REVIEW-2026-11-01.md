# Final review of the multi-game branch (2026-11-01)

Branch `ccr-edd9eded-13lohf`, everything pushed at `d3c48f67`. Last full run: Windows 4,566 tests + 481 end-to-end; Linux 4,116, 0 failing.
Untracked note in your main checkout; delete it whenever.

## How it was reviewed
- **6 Sonnet bug hunters**, one risk area each, every bug proven with a failing test:
  file safety, VFS and launching, downloads and trust, plugins, desktop UI, content store and archives.
- **1 Opus ship-readiness review** (read-only): what would hurt a first real player, spec vs build,
  false-confidence tests, year-1 traps.
- **Fix rounds** on DeepSeek (OpenCode Go), one per area, each fix checked by removing it and watching
  its test fail; file safety got two extra Sonnet passes because crash-ordering fixes tend to move bugs.
- **Docs**: a Sonnet agent corrected the spec and wrote a player guide topic for other games.

## What was found and fixed
| Area | Proven | Worst | Status |
|---|---|---|---|
| File safety | 7 + 5 suspected, then 3 more in pass 2 | a crash lost a player's save file; a half-deleted farm wiped saves; a torn backup overwrote the original | fixed after 3 review passes (hash-first journal, verified backups, atomic writes) |
| VFS | 11 + 2 suspected | `..` wrote outside the sandbox; a caught panic left the thread unprotected | fixed |
| Downloads/trust | 5 + 3 suspected | an old signed catalog could replace a newer one; catalog installs read an unverified catalog | fixed |
| Plugins | 2 + 5 suspected | script consent leaked across an update; a publisher change kept consent silently | fixed |
| Desktop UI | 3 + 2 suspected | Install could target the previous game's instance | fixed |
| Content store | 4 + 3 suspected | removing one item deleted another item's icon | fixed |

## Opus's ship blockers and what happened to each
1. **The Skyrim plugin's download (`official-skyrim-se-v0.1.0`) does not exist yet.** It publishes
   automatically when the branch reaches master. The catalog compile now leaves out an official entry
   whose download is missing (with a warning) and fails only if the bytes don't match their pin.
2. **A Skyrim that discovery misses was a dead end.** Now: **Add game folder** in Settings > Game
   support, and right after installing a game plugin.
3. **The spec promised Minecraft "no change".** Corrected: it records the shared-store decision.
4. **Skyrim vanished when the catalog couldn't be verified or dropped its version.** Now it keeps
   loading with a warning; only tampered files, an explicit revocation or an impostor block it.
5. **CI never tested the VFS crates.** Now it builds/tests them, with a Windows job for the VFS write
   matrix and real-injection tests (run locally here: 12 + 14 passed).

## Found during the last merge
- Agora's new atomic write silently replaced player files marked **read-only** on Linux (Windows refused).
  Fixed: read-only files are never replaced on either platform, and replaced files keep their permissions.

## Accepted leftovers (judgement calls I made; overrule any)
- One Linux test flaked once (`a_base_build_holds_its_lock_between_the_rename_and_the_record`, a 30 s
  lock wait under heavy load); passed on rerun.
- Minecraft's own catalog installs read the catalog without re-checking its signature at install time
  (as on master); it is verified at startup and sync, so only a local writer could exploit it.
- A damaged store item can't be removed while any instance tracks mods by fingerprint (no
  "remove anyway" yet).
- A game that truncates its own save at exit can't be told apart from a real edit.
- Plugin consent re-asks even when an update changes only a data file (strict on purpose).
- Earlier batches: nested pack files untracked; same-size-and-time file swaps unseen until a cache
  rebuild; an open handle writing in the milliseconds after the last check.

## Needs you
1. **Click-through** in the real app: Minecraft (shared mod files, "Access denied" outside Agora,
   Library, Unused items, Share toggle + convert) and a first Skyrim setup (More games, Add game folder).
2. **Real launches** (need your yes): Pandora; a Skyrim run to prove the VFS hook fixes tests can't reach
   (delete-on-close, deny-write fallback, root-handle rights, stream/rename paths).
3. **Decide**: ship Skyrim with **zero curated catalog entries**, or curate some first?
4. Phase 4 timings: the 84 GB import re-time and a LOOT sort on a big setup.
5. Branding/website (still on hold), then merge to master (that publishes the Skyrim release).

## Year-1 traps Opus flagged (not changed)
- "Two of everything": Minecraft's own instance model beside the generic one, two placement paths,
  `download_cache` beside the store. Fixes land twice.
- Skyrim pins plugin API `>=0.1, <0.2`: plan a compatibility window before the API moves.
- The store never cleans itself; only Library > Unused items. Old jars from every update stay.
- The core-coupling budget counts tokens, not coupling.
