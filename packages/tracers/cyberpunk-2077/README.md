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

`redmod-deploy` runs `tools/redmod/bin/redMod.exe deploy -root=<game>` before a launch when
the instance has a REDmod. The tool's output is the existing `generated_layer` model: whatever
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
not documented there. One name per line is this package's choice.

**Gap:** the list is rendered into the instance, but the deploy tool cannot pass it to REDmod.
Tool recipes may only name runtime/install/base paths (an `instance` path is refused), and the
list file does not live in the game view. So `-modlist` is not on the command line; REDmod falls
back to its documented ASCII folder order. See the hand-back.

## Frameworks

- RED4ext: marker `red4ext/RED4ext.dll`, version from the DLL's PE `FileVersion`. The install is
  two files, `red4ext/RED4ext.dll` and the `bin/x64/winmm.dll` loader:
  <https://docs.red4ext.com/getting-started/installing-red4ext>.
- Cyber Engine Tweaks: marker `bin/x64/plugins/cyber_engine_tweaks.asi`; its folder and log are
  `bin/x64/plugins/cyber_engine_tweaks/` and `cyber_engine_tweaks.log`:
  <https://wiki.redmodding.org/cyber-engine-tweaks/getting-started/installing>. It requires
  RED4ext: <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-creators-theory/core-mods-explained>.
- `diagnose` warns when `red4ext/RED4ext.dll` is there but `bin/x64/winmm.dll` is not, and reads
  `red4ext/logs/red4ext.log` through `game:read` to surface a last line that reports an error.
  Log path: <https://wiki.redmodding.org/cyberpunk-2077-modding/for-mod-users/user-guide-troubleshooting/finding-and-reading-log-files>.

## Saves and user files

- Saves: `%USERPROFILE%\Saved Games\CD Projekt Red\Cyberpunk 2077`:
  <https://support.cdprojektred.com/en/cyberpunk/pc/sp-technical/issue/1706/where-are-my-game-saves-located>.
- Settings: `%LOCALAPPDATA%\CD Projekt Red\Cyberpunk 2077\UserSettings.json`, journaled-swapped
  per instance: <https://steamcommunity.com/sharedfiles/filedetails/?id=2316174052>.
- Crash paths are not declared yet.

`red4ext/**` and `bin/x64/plugins/**` are deliberately not in `excluded_paths`: the base
must keep a pre-installed framework so its marker and PE version can be read. Keeping
`mods/**` and `r6/scripts/**` excluded means files already in the game folder are not baked
into the base as unmanaged content; they must be added to an instance to be seen.

## What is not wired down

`redmodRules` names only folders that are in the list. That is why it reads the list from the
hook's `context.load_order` instead of returning a fixed pair: a fixed pair would report a
missing-mod warning for every player who does not have both.
