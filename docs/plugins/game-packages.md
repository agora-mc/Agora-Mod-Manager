# Adding a game to Agora

You can add a game to Agora without forking it or asking anyone. A game is a plugin: a JSON description of the game, plus a script for whatever JSON can't say. Agora's official game plugins use the same fields and the same script API, and they get no special access.

> **This format is still changing.** It's free to break until its first public release. Set `apiRange` so your plugin stops loading rather than misbehaving when it does.

**A worked example:** [`packages/tracers/cyberpunk-2077`](../../packages/tracers/cyberpunk-2077), about 180 lines of JSON and 140 of script. It places three kinds of mod, runs REDmod before a modded launch, keeps a load order in a format Agora has never heard of, and warns about a broken framework. Read it alongside this page.

## The shape of a plugin

```text
my-game/
  agora-plugin.json      the manifest
  games/package.json     the game
  main.js                optional: your script
```

The manifest declares the package and what it needs. The [author guide](README.md#the-manifest) covers manifests in general:

```json
{
  "manifest": 1,
  "id": "example.my-game",
  "name": "My Game",
  "version": "0.1.0",
  "license": "MIT",
  "apiRange": ">=0.1, <0.2",
  "entrypoint": "main.js",
  "capabilities": { "required": ["game:define", "game:read"] },
  "contributions": { "gamePackages": [{ "path": "games/package.json" }] }
}
```

Check your work at any time, with no game installed:

```bash
agora plugin validate path/to/my-game
```

## Start with the game

A package holds `games`, `frameworks` and `tools`. The minimum for a game is who it is, how to find it, and how to run it:

```json
{
  "id": "example.my-game",
  "version": "0.1.0",
  "api_range": ">=0.1, <0.2",
  "parents": [],
  "games": [{
    "id": "my-game",
    "name": "My Game",
    "stores": [{ "store": "gog", "product": "1234567890" }],
    "version_sources": [{ "kind": "executable", "path": "bin/game.exe" }],
    "deployment": "virtual_file_system",
    "launch": {
      "executable": { "root": "runtime", "path": "bin/game.exe" },
      "arguments": [],
      "environment": {},
      "working_directory": { "root": "runtime", "path": "" }
    }
  }]
}
```

Now Agora finds the game, builds instances of it and launches them. The rest of this page is what you add next, in the order you'll probably want it.

## Paths and patterns

Everywhere a path appears, it's relative, with forward slashes. Anything that climbs out with `..`, or is absolute, is refused when your plugin loads.

In patterns, `*` stays inside one folder and `**` crosses folders. So `mods/*` matches `mods/a` but not `mods/a/b.txt`, and also matches a stray `mods/.stub`. If you mean "a mod is installed", say what makes it a mod: `mods/*/info.json`.

## Conditions

Anywhere you see `when`, you can write one of these:

```json
{ "present": "mods/*/info.json" }
{ "framework": "red4ext", "version": { "min": "1.20" } }
{ "version": { "below": "2.0" } }
{ "all": [] }
{ "any": [] }
{ "not": {} }
```

- `present` looks at the instance as the game will see it, with all its mods merged.
- `framework` looks at the frameworks installed in the instance.
- A bare `version` looks at the game.

**Agora can answer "I don't know".** If it couldn't detect the game's version, a `version` condition is unknown, neither true nor false. A `when` applies only when it's true, and on unknown the launch log says Agora couldn't tell. A misspelled key stops your package loading, rather than quietly reading as true.

## Where mods go

`content_rules` decide where an item goes when it's added. They're tried in order, and the first match wins. It decides the whole item:

```json
"content_rules": [
  { "kind": "REDmod", "match": { "marker": "info.json" }, "mount": "mods/{folder}" },
  { "kind": "Archive mod", "match": { "all_files": "*.archive" }, "mount": "archive/pc/mod" }
]
```

- `marker` looks for a file, and the folder holding it is what gets placed.
- `all_files` matches only when every file does.
- `{folder}` is that folder's name, or the item's name when the marker is at the top.
- `kind` is what the player sees.

If no rule matches, Agora tries `content_layout`, which says which top-level folders mean "this already has the game's shape":

```json
"content_layout": { "data_path": "", "data_markers": [], "root_markers": ["archive", "bin", "r6"] }
```

Then it tries your script's `classify` hook, and finally asks the player. Agora never guesses a placement silently.

## Load orders

A game can have any number of ordered lists, or none:

```json
"load_orders": [{
  "id": "redmod-mods",
  "name": "REDmod mods",
  "file": "mods/modlist.txt",
  "destination": "view",
  "items": { "folder": "mods", "patterns": ["*"], "folders": true },
  "format": { "type": "script", "parse": "parseModlist", "render": "renderModlist" },
  "rules": { "kind": "script", "entry": "redmodRules" }
}]
```

**`format`** is how the list is stored. Use `{ "type": "lines", "active_prefix": "*", "header": [] }` for a plain text list. A script format handles anything else, because your script reads and writes the bytes.

**`destination`** is where the file goes. `instance`, the default, keeps it with the instance. `view` puts it in the game's view through a layer of its own, so the game and tools can see it, and your real game folder stays untouched.

**`rules`** are what must be true of an order:

| `kind` | Meaning |
|---|---|
| `none` | Any order works, and the player sees no Sort button |
| `script` | Your script returns rules, and Agora checks them and reports cycles |
| `creation_engine` | Masters, light plugins and limits. It's the one built-in, and it's moving into a plugin |

A script's rule is `before`, `requires` or `conflicts`. With `optional: true`, a rule applies only when both items are present. Without it, a missing item is reported to the player.

## Tools

A tool is a program that works on the instance's mods. Whatever it writes is kept in a layer of its own, so the player can compare or undo it.

```json
"tools": [{
  "id": "redmod-deploy",
  "game": "cyberpunk-2077",
  "name": "REDmod deploy",
  "launch": {
    "executable": { "root": "runtime", "path": "tools/redmod/bin/redMod.exe" },
    "arguments": ["deploy"],
    "environment": {},
    "working_directory": { "root": "runtime", "path": "" }
  },
  "output": { "kind": "generated_layer" },
  "input_files": ["mods/**"],
  "before_launch": { "mode": "run", "when": { "present": "mods/*/info.json" } }
}]
```

**`inputs`** ask the player for values, which Agora saves per instance and passes as arguments:

```json
"inputs": [{ "id": "profile", "label": "Profile", "kind": "choice", "required": true, "choices": ["a", "b"], "arg": "--profile" }]
```

`kind` is `file`, `folder`, `choice`, `text` or `flag`.

**`scope`** is `instance`, or `item` for a tool that runs on one mod at a time, with `{item}` in its arguments.

**`input_files`** says what makes the tool's output stale. Leave it out and any change to the instance's content or load orders does.

**`before_launch`** turns on when the tool has never run or is stale, and `when` is true. `run` runs it visibly first, and `ask` offers it. **The player can always launch anyway.**

## Launch arguments

An argument is a string, or a string with a condition:

```json
"arguments": ["-skipintro", { "value": "-modded", "when": { "present": "mods/*/info.json" } }]
```

## Scripts

Name your script's functions under `behaviours` in the package:

```json
"behaviours": { "classify": "classifyItem", "load_order_rules": "redmodRules", "diagnostics": "diagnose" }
```

| Hook | Called when | Returns |
|---|---|---|
| `detect_runtime` | Finding the game's version, for a `package_behaviour` version source | The runtime, or null |
| `classify` | Placing an item nothing else matched | A placement, or null |
| `recognize_download` | Agora sees a download | What it is, or null |
| `load_order_rules` | Checking a `script` rule set | A list of rules |
| `diagnostics` | Launch checks | A list of problems |
| `prepare_launch` | Launching a game with no `launch` | A launch recipe |

Formats name their own functions: `parse` and `render`.

**Null means "I have no opinion".** Throwing means something went wrong, and Agora tells the player rather than treating it as a clean result. A malformed reply counts as an error too. Hooks have time and memory limits. Names the script sees, like an item's, come from archives: treat them as untrusted.

## What a script can reach

| Capability | Gives you | Limit |
|---|---|---|
| `game:read` | `game.readBytes`, `game.readLines`, `game.list` by game path, on the instance as the game will see it | A budget per call, and you never see a real path |
| `game:write` | Adding files to your plugin's own layer for that instance | Size and file limits. You can't write to the game, a save or another layer |
| `instances:*`, `content:*` | The same calls Minecraft plugins use, for every game | As in the [service API](README.md#service-api) |

Inside a hook, reads and writes are bound to the hook's instance.

**`game:write` is a real trust decision, and the player is told so.** A file you add can be a program or a mod the game runs, so a plugin with this capability can run code on the player's PC. Ask for it only if you need it.

**Launch recipes stay inside the instance.** The executable and working folder must be in the instance's view, and a declared argument can't name a path outside it. A recipe can't set an environment variable that loads code, like `LD_PRELOAD` or `DOTNET_STARTUP_HOOKS`. Values the player chose aren't checked, because they're the player's call. If your game truly needs to run something outside this, that's the helper-program tier, which doesn't exist yet: [open an issue](https://github.com/agora-mc/Agora-Mod-Manager/issues) and tell us what you need.

## Sharing a base

Games that share an engine can share what they have in common. Put it in a parent package, under `game_templates`:

```json
{ "id": "example.engine", "game_templates": { "engine": { "content_layout": { "data_path": "Data" } } } }
```

Then build on it:

```json
{ "id": "example.my-game", "parents": ["example.engine"], "games": [{ "id": "my-game", "extends": "engine" }] }
```

Merging works on the JSON, before the game is read, so a template can be partial:

- **Objects** merge deeply, and a child's plain value wins.
- **Lists of objects with an `id`** merge by `id`, keeping the parent's order.
- **Other lists** join up, parent first.
- **`null`** removes a field.

A package's `frameworks` and `tools` come along from its parents the same way. A missing parent or template, or a cycle, stops that package registering, with a warning naming the chain. Other packages carry on.

## Known gaps

Tell us about others: finding them is the point of this stage.

- **Per-instance saves** work only for games whose save folder is set in an INI file. For other games, instances share saves.
- **A framework left in the real game folder** by another mod manager isn't detected. A framework counts when it's content in the instance.
- **Crash doctor rules** have a contract and an engine, but no guide yet.

## Every field

Everything an official plugin uses is listed here, and a check in this repository fails if one isn't.

FIELD_TABLE
