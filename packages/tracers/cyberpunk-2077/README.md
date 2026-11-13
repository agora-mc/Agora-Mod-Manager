# Cyberpunk 2077 tracer

The package that proves MASTER_SPEC §30.12: a working modded Cyberpunk 2077 using only
`agora-plugin.json`, `games/package.json` and `main.js`. There is no Agora code behind it.

## What it places

| Item shape | Where | Why |
|---|---|---|
| `<Mod>/info.json` (a REDmod) | `mods/<Mod>` | REDmod mods live in `mods/<Name>/` and carry `info.json` at the top of their folder. <https://www.cyberpunk.net/en/modding-support> and <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077> |
| Only `*.archive` files | `archive/pc/mod` | Legacy archive mods go in `archive/pc/mod`. <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077> |
| A root-shaped archive (`archive/`, `bin/`, `r6/`, `red4ext/`, `engine/`, `mods/`) | game root | Those top-level folders merge with the game folder. Same wiki page. |
| Loose `*.reds` files under one folder | `r6/scripts/<folder>` | Redscript is read from `r6/scripts`, not from an archive. Same wiki page. This is the script `classify` hook because a `.reds` set has no marker file. |

## The tool

`redmod-deploy` runs `tools/redmod/bin/redMod.exe deploy -root=<game> -modlist=<game>/mods/modlist.txt`
before a launch when the instance has a REDmod. The tool's output is the existing `generated_layer` model: whatever
the run writes is kept as the tool's own layer in the instance stack, so the next deployment
shows it above the base and the game reads it. REDmod writes the staged result under
`r6/cache` (the wiki below shows `r6/cache/modded/final.redscripts`).

- Executable path and name: <https://www.cyberpunk.net/en/modding-support> (`\Cyberpunk 2077\tools\redmod\bin\redMod.exe`).
- `redmod deploy -root=<path>`: <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077/redmod/commands/deploy>.
- `-modded` starts a game that loads deployed REDmods: the CDPR REDmod documentation, <https://cdn-l-cyberpunk.cdprojektred.com/REDmod-docs.pdf>, and <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077/redmod/usage>.

The tool's freshness inputs are `mods/**`, because REDmod deploys only `/mods`; a legacy
`archive/pc/mod` change does not need a redeploy. The brief named `mods/*`; Agora's globs are
per path segment, so `mods/*` would not see `mods/<Mod>/info.json`. `mods/**` is the same
intent and actually matches.

## The load order

`mods/modlist.txt` is a script format (`parseModlist`/`renderModlist`): one REDmod folder name
per line, first line highest precedence, which is the order `-modlist` documents. The file is
maintained by core, and the `redmodRules` hook puts a `!`-prefixed mod before the others.
ASCII order already puts `!` first; the wiki's load-order page says punctuation changes
precedence and that the first mod to change a file wins
(<https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/users-modding-cyberpunk-2077/load-order>).

**Unverified:** the `-modlist` option is named on the deploy page above, but its file layout is
not documented there. One name per line is this package's choice; the owner's real game will
settle it.

The list declares `"destination": "view"`. Core renders it from the instance copy into the
instance's own generated layer, and the deployment puts that file in the game view above content
and tool output. The tool reads it as the ordinary runtime path `mods/modlist.txt`, so the real
game folder is never written and no `user_files` mapping is needed. Reordering the list changes
the view file and the tool's fingerprint, so the next launch deploys again.

`redmodRules` returns optional rules where both names are present: with `optional: true`, a rule
that names a mod the player does not have says nothing instead of reporting a missing item.

## Frameworks

- RED4ext: marker `red4ext/RED4ext.dll`, version from the DLL's PE `FileVersion`. The install is
  two files, `red4ext/RED4ext.dll` and the `bin/x64/winmm.dll` loader:
  <https://docs.red4ext.com/getting-started/installing-red4ext>.
- Cyber Engine Tweaks: marker `bin/x64/plugins/cyber_engine_tweaks.asi`; its folder and log are
  `bin/x64/plugins/cyber_engine_tweaks/` and `cyber_engine_tweaks.log`:
  <https://wiki.redmodding.org/cyber-engine-tweaks/getting-started/installing>. It requires
  RED4ext: <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-creators-theory/core-mods-explained>.
- Frameworks are content in the instance: the base excludes them, so a framework arrives as a
  content item and its marker is read from the merged view. A framework left in the real game
  folder by another manager is **not handled yet**; the Vortex importer is the later job for that.
- `diagnose` warns when `red4ext/RED4ext.dll` is there but `bin/x64/winmm.dll` is not, and reads
  `red4ext/logs/red4ext.log` through `game:read` to surface a last line that reports an error.
  Log path: <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/user-guide-troubleshooting/finding-and-reading-log-files>.

## Saves and user files

- Saves: `%USERPROFILE%\Saved Games\CD Projekt Red\Cyberpunk 2077`:
  <https://support.cdprojektred.com/en/cyberpunk/pc/sp-technical/issue/1706/where-are-my-game-saves-located>.
- Settings: `%LOCALAPPDATA%\CD Projekt Red\Cyberpunk 2077\UserSettings.json`, journaled-swapped
  per instance: <https://steamcommunity.com/sharedfiles/filedetails/?id=2316174052>.
- Crash paths are not declared yet.

`red4ext/**`, `bin/x64/plugins/**`, `mods/**` and `r6/scripts/**` stay excluded from the base:
a base is never polluted with mods or frameworks. Framework detection reads the merged view, so a
framework is visible when it is content in the instance.

## What is not wired down

`redmodRules` names only folders that are in the list, and it marks its pairs optional. That is
why it reads the list from the hook's `context.load_order` instead of returning a fixed pair.
