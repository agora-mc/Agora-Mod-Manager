# Adding a game to Agora

You can add a game to Agora without forking it or asking anyone. A game is a plugin: a JSON description of the game, plus a script for whatever JSON can't say. Agora's official game plugins use the same fields and the same script API, and they get no special access.

> **This format is still changing.** It's free to break until its first public release. Set `apiRange` so your plugin stops loading rather than misbehaving when it does.

**A worked example:** [`packages/tracers/cyberpunk-2077`](../../packages/tracers/cyberpunk-2077), about 240 lines of JSON and 150 of script. It places three kinds of mod, runs REDmod before a modded launch, keeps a load order in a format Agora has never heard of, and warns about a broken framework. Read it alongside this page.

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

Check the whole plugin at any time, with no game installed:

```bash
agora plugin validate path/to/my-game
```

It checks the manifest, the crash packs and the game packages. Every contributed package is parsed, its `parents` and `extends` are resolved as far as this folder allows, and the same registration checks the app runs are applied. A parent that lives in another plugin is reported as a note rather than an error, because it cannot be read here. Every script function the package names is checked against the entrypoint when its exports can be read without running it; when they cannot, the command says so. It exits non-zero on a problem and names the file and, when it can, the field. It runs no code and writes nothing.

## Start with the game

A package holds `games`, `frameworks` and `tools`. Only the parts you use need to be written; an empty list can be left out. The minimum for a game is who it is, how to find it, and how to run it:

```json
{
  "id": "example.my-game",
  "version": "0.1.0",
  "api_range": ">=0.1, <0.2",
  "games": [{
    "id": "my-game",
    "name": "My Game",
    "stores": [{ "store": "gog", "product": "1234567890" }],
    "version_sources": [{ "kind": "executable", "path": "bin/game.exe" }],
    "deployment": "virtual_file_system",
    "launch": {
      "executable": { "root": "runtime", "path": "bin/game.exe" },
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
{ "version": { "exact": ["1.6.1170.0"] } }
{ "all": [{ "present": "mods/*/info.json" }] }
{ "any": [{ "framework": "red4ext" }, { "framework": "cyber-engine-tweaks" }] }
{ "not": { "present": "mods/*/info.json" } }
```

- `present` looks at the instance as the game will see it, with all its mods merged.
- `framework` looks at the frameworks installed in the instance.
- A bare `version` looks at the game.
- A `version` gate takes `exact`, a list of versions, or `min` (inclusive) and `below` (exclusive), which compare dotted numbers.
- `all`, `any` and `not` combine other conditions. `all` of nothing is true; `any` of nothing is false.

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

If no rule matches, Agora tries `content_layout`, which says where data-folder content goes and which top-level names mean the item already has the game's shape:

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

A script's rule is `before`, `requires` or `conflicts`, written as JSON from your `load_order_rules` hook:

```json
{ "kind": "before", "first": "a", "second": "b" }
{ "kind": "requires", "item": "a", "master": "b" }
{ "kind": "conflicts", "first": "a", "second": "b", "optional": true }
```

With `optional: true`, a rule applies only when both items are present, or both active for `conflicts`. Without it, a missing item is reported to the player.

## Tools

A tool is a program that works on the instance's mods. By default whatever it writes is kept in a generated layer of its own, so the player can compare or undo it. `output` can instead be `mod`, which imports a folder the tool wrote as a content item.

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

Formats name their own functions: `parse` and `render`. Those are not in `behaviours`; the load order's `format` names them. A list's `rules.entry` names its rule function, and wins over `behaviours.load_order_rules`.

Every hook is called with one object, and its reply is JSON:

| Hook | Arguments | Reply |
|---|---|---|
| `detect_runtime` | `{ "install": GameInstall }` | `RuntimeIdentity`, or `null` |
| `classify` | `{ "context": GameContext, "itemName": string, "files": [path] }` | `Suggestion`, or `null` |
| `recognize_download` | `{ "fileName": string, "entries": [path] }` | `DownloadRecognition`, or `null` |
| `load_order_rules` | `{ "context": GameContext, "listId": string }` | `[LoadOrderRule]`, or `null` |
| `diagnostics` | `{ "context": GameContext }` | `[Diagnostic]`, or `null` |
| `prepare_launch` | `{ "context": GameContext }` | `LaunchRecipe`, or `null` |
| `parse` | `{ "context": GameContext, "listId": string, "bytes": [number] }` | `[LoadOrderLine]`, or `null` |
| `render` | `{ "context": GameContext, "listId": string, "lines": [LoadOrderLine] }` | a string, or `[number]` byte values |

- `GameInstall` has `id`, `runtime`, `kind` (`base_game`, `add_on` or `tool`), `location`, `volume` and `capabilities`.
- `RuntimeIdentity` has `game`, `store`, `version` and optional `build`.
- `GameContext` has `instance`, `runtime`, `base`, `frameworks`, `layers`, `load_order` and `settings`.
- `Suggestion` is `{ "type": "place", "source_path", "mount_path", "reason" }`, `{ "type": "installer", "reason" }` or `{ "type": "unknown", "top_level", "note" }`; `note` is optional.
- `DownloadRecognition` has `kind`, `reason` and `add`, where `add` is `{ "mode": "archive" }` or `{ "mode": "file", "folder": "..." }`.
- `Diagnostic` has `code`, `message`, `severity` (`info`, `warning` or `error`) and `repairs`.
- `LoadOrderRule` is the `before`, `requires` or `conflicts` object shown above.
- `LoadOrderLine` has `name` and `active`.

**Null means "I have no opinion".** A throw is reported, never read as "nothing to say". Some hooks stop the action; a failed `diagnostics` becomes a warning finding, so the check is not shown as clean. A malformed reply counts as an error too. Hooks have time and memory limits. Names the script sees, like an item's, come from archives: treat them as untrusted.

## What a script can reach

| Capability | Gives you | Limit |
|---|---|---|
| `game:read` | `game.readBytes`, `game.readLines`, `game.list` by game path, on the instance as the game will see it | A budget per call, and you never see a real path |
| `game:write` | `game.writeBytes`, `game.writeText`, adding files to your plugin's own layer for that instance | Size and file limits. You can't write to the game, a save or another layer |
| `instance:read`, `instance:write`, `content:read`, `content:write` | The same calls Minecraft plugins use, for every game | As in the [service API](README.md#service-api) |

Inside a hook, reads and writes are bound to the hook's instance.

**`game:write` is a real trust decision, and the player is told so.** A file you add can be a program or a mod the game runs, so a plugin with this capability can run code on the player's PC. Ask for it only if you need it.

**Launch recipes stay inside the instance.** The executable and working folder must be in the instance's view, and a declared argument can't name a path outside it. A recipe can't set an environment variable that loads or redirects code, like `LD_PRELOAD`, `DOTNET_STARTUP_HOOKS` or `PATH`. Values the player chose aren't checked, because they're the player's call. If your game truly needs to run something outside this, that's the helper-program tier, which doesn't exist yet: [open an issue](https://github.com/agora-mc/Agora-Mod-Manager/issues) and tell us what you need.

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
- **Lists of objects with an `id`** merge by `id`; a replaced item keeps the parent's position, and a new one is added at the end.
- **Other lists** join up, parent first.
- **`null`** removes a field.

A package's `frameworks` and `tools` come along from its parents the same way. A missing parent or template, or a cycle, stops that package registering, with a warning naming the chain. Other packages carry on.

## Known gaps

Tell us about others: finding them is the point of this stage.

- **Per-instance saves** work only for games whose save folder is set in an INI file. For other games, instances share saves.
- **A framework left in the real game folder** by another mod manager isn't detected. A framework counts when it's content in the instance.
- **Crash Doctor rules** have a contract and an engine, but no guide yet.

## Every field

Every field the format accepts is listed here. A check in this repository fails if a key in an
official package, or a field on a package type, isn't documented below.

### Package

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | The package's id, in `publisher.name` form |
| `version` | string | required | The package's semver version |
| `api_range` | string | required | The plugin API range it was written for, as a semver range |
| `parents` | list of strings | optional | Package ids it inherits from |
| `games` | list of games | optional | The games it defines; may be empty when a parent supplies them |
| `frameworks` | list of frameworks | optional | The frameworks it defines; may be empty |
| `tools` | list of tools | optional | The tools it defines; may be empty |
| `game_templates` | map of id to partial game | optional | Partial games a game can start from with `extends` |
| `behaviours` | object | optional | The script function names below |

### Behaviours

Each value is a function name in your script. Every field is optional.

| Field | Meaning |
|---|---|
| `detect_runtime` | Finds the runtime for a `package_behaviour` version source |
| `classify` | Places an item nothing else placed |
| `recognize_download` | Recognises a download |
| `load_order_rules` | Returns rules for a `script` order without its own `entry` |
| `diagnostics` | Adds findings to an instance's launch checks |
| `prepare_launch` | Builds a launch recipe for a game with no declarative `launch` |

### Game

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | The game id, unique across installed packages |
| `name` | string | required | What the player sees |
| `extends` | string | optional | A `game_templates` entry to merge before parsing |
| `stores` | list of `{ store, product }` | required | Where the game is sold; may be empty |
| `version_sources` | list of version sources | required | How Agora finds the game's version |
| `deployment` | string | required | `redirect` or `virtual_file_system` |
| `content_rules` | list of content rules | optional | Placement rules, tried in order |
| `content_layout` | content layout | optional | The fallback shape of the game's content |
| `framework_ids` | list of strings | optional | Framework ids the game may use; may be empty |
| `tool_ids` | list of strings | optional | Tool ids the game may use; may be empty |
| `launch` | launch recipe | optional | How to launch it; leave it out when `prepare_launch` supplies one |
| `log_paths` | list of game paths | optional | Where its logs are; may be empty |
| `crash_paths` | list of game paths | optional | Where crash evidence is; may be empty |
| `user_files` | list of user file mappings | optional | Per-user files kept in the instance; may be empty |
| `save_paths` | list of game paths | optional | Where its saves are; may be empty |
| `linked_archive_patterns` | list of globs | optional | Archives linked rather than copied; may be empty |
| `declared_writes` | list of globs | optional | Paths the game may write outside its folder |
| `copy_patterns` | list of globs | optional | Extra small text files copied in Links mode |
| `excluded_paths` | list of globs | optional | Paths never taken from the game folder |
| `load_orders` | list of load orders | optional | Ordered lists the game keeps |
| `launch_alternatives` | list of launch alternatives | optional | Another executable when a condition holds |
| `runtime_files` | list of runtime file rules | optional | Files a framework must have for this runtime |
| `save_location` | list of save location rules | optional | How per-instance saves are chosen |
| `mo2_game_name` | string | optional | The game's name in Mod Organizer 2, for imports |
| `session` | session rules | optional | Which process ends a session and which exits are normal |

A store is `{ "store": "gog", "product": "1234567890" }`: `store` names the store and `product`
names the game there.

A version source is `{ "kind": "executable", "path": "bin/game.exe" }`, `{ "kind": "store_record" }`
or `{ "kind": "package_behaviour" }`. Only `executable` needs `path`.

### Content rule and layout

| Field | Type | Required | Meaning |
|---|---|---|---|
| `kind` | string | required | The kind shown to the player, such as `REDmod` |
| `match` | content match | required | What the rule looks for |
| `mount` | path | required | Where the item goes; `{folder}` is the marker's folder |
| `when` | condition | optional | The rule applies only when this is true |
| `marker` | string | optional | A file at any depth; write it or `all_files` |
| `all_files` | glob | optional | Matches only when every file in the item does |
| `data_path` | path | required | Where data-folder content mounts; empty means the game root |
| `data_markers` | list of globs | optional | Top-level names that mean data-folder content |
| `root_markers` | list of globs | optional | Top-level names that mean game-root content |
| `thunderstore_bepinex` | boolean | optional | Lay out Thunderstore BepInEx packages |

### Launch recipe and values

| Field | Type | Required | Meaning |
|---|---|---|---|
| `executable` | game path | required | The program to run |
| `arguments` | list of launch values | optional | Its arguments |
| `environment` | map of string to launch value | optional | Environment variables it gets |
| `working_directory` | game path | required | Where it runs |
| `value` | string or launch value | required | A literal, or the value inside a conditional |
| `kind` | string | optional | `literal`, `path` or `path_list` for the old-shaped form |
| `path` | game path | required for `path` | The path to pass |
| `paths` | list of game paths | required for `path_list` | The paths to join with the platform separator |
| `prefix` | string | required for `path` and `path_list` | Text before the path |
| `suffix` | string | required for `path` | Text after the path |
| `when` | condition | required for a conditional | The argument applies only when this is true |

A launch argument is a plain string, `{ "value": "-x", "when": { ... } }`, or one of the old-shaped
objects `{ "kind": "literal", "value": "-x" }`, `{ "kind": "path", "path": { ... }, "prefix": "",
"suffix": "" }` and `{ "kind": "path_list", "paths": [ ... ], "prefix": "" }`.

### Game path

| Field | Type | Required | Meaning |
|---|---|---|---|
| `root` | string | required | `runtime`, `install`, `base`, `instance`, `layer`, `runtime_component`, `artifact` or `user_data` |
| `path` | path | required | The path under the root; empty is allowed for `runtime` |
| `install` | string | required for `install` | The install id |
| `base` | string | required for `base` | The base id |
| `layer` | string | required for `layer` | The layer id |
| `component` | string | required for `runtime_component` | The runtime component name |
| `artifact` | string | required for `artifact` | The artifact id |
| `location` | string | required for `user_data` | `documents`, `roaming_app_data`, `local_app_data` or `home` |

A game path is `{ "root": "runtime", "path": "bin/game.exe" }`. `runtime` means the instance's
selected runtime; `user_data` means a known user folder, resolved by Agora.

### Load order

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | Local to the game; refused when two lists share it |
| `name` | string | required | What the list is called to a player |
| `file` | path | required | The file the list is written to |
| `destination` | string | optional | `instance` (the default) or `view` |
| `items` | load order items | required | What the list contains |
| `format` | load order format | required | How the file is read and written |
| `rules` | load order rules | required | How the order is judged |
| `folder` | path | required | The game folder the items are in |
| `patterns` | list of globs | optional | Case-insensitive names to list |
| `folders` | boolean | optional | List folders too, as REDmod's `mods/<name>` are |
| `type` | string | required | `lines` or `script` |
| `active_prefix` | string | optional | The prefix that marks an active line in a `lines` file |
| `header` | list of strings | optional | Lines written at the top of a `lines` file |
| `parse` | string | required for `script` | The function that reads the file |
| `render` | string | required for `script` | The function that writes the file |
| `kind` | string | required | `none`, `script` or `creation_engine` |
| `entry` | string | optional | The function that returns rules for a `script` kind |
| `implicit` | list of strings | optional | Plugins the game always loads first |
| `implicit_list_file` | path | optional | A file naming more always-loaded plugins |
| `loot` | object | optional | LOOT masterlist sources |
| `masterlist_repo` | string | required for `loot` | The masterlist repository, as `owner/repo` |
| `prelude_repo` | string | optional for `loot` | The shared prelude repository, as `owner/repo` |
| `branch` | string | required for `loot` | The masterlist branch this libloot expects |
| `game_type` | string | required for `loot` | The game name libloot knows, such as `SkyrimSE` |

### Script load-order rule and line

| Field | Type | Required | Meaning |
|---|---|---|---|
| `kind` | string | required | `before`, `requires` or `conflicts` |
| `first` | string | required for `before` and `conflicts` | The item that comes first, or one side of the conflict |
| `second` | string | required for `before` and `conflicts` | The item that comes second, or the other side |
| `item` | string | required for `requires` | The item that needs the master |
| `master` | string | required for `requires` | The item it needs |
| `optional` | boolean | optional | Say nothing when the other item is absent; for `conflicts`, both must be active |
| `name` | string | required | The item's name as the file writes it |
| `active` | boolean | required | Whether the game loads it |

### Tool

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | Local to the package |
| `game` | string | required | The game it belongs to |
| `name` | string | required | What the player sees |
| `launch` | launch recipe | required | How the tool runs |
| `output` | tool output | optional | What a successful run produces; default `generated_layer` |
| `output_arg` | string | optional | An argument such as `-o:"{output}"` that names Agora's output folder |
| `relevant_settings` | list of strings | optional | `file:section:key` settings its output depends on |
| `after_tools` | list of strings | optional | Tools whose output this one sits above |
| `conflicts_with` | list of strings | optional | Tools whose output would contend with this one |
| `uses_install_path` | boolean | optional | It needs the real install path, so it runs by swap |
| `output_patterns` | list of globs | optional | Files it writes, for an MO2 import |
| `required_outputs` | list of globs | optional | Files a successful run must produce |
| `failure_markers` | list of failure markers | optional | Text that marks a failed run |
| `required_magic` | list of required magic | optional | First bytes a written file must have |
| `run_only` | boolean | optional | The player runs it; there is no build |
| `executable_names` | list of strings | optional | Other names its executable goes by |
| `inputs` | list of tool inputs | optional | Values the player fills in |
| `scope` | string | optional | `instance` (default) or `item` |
| `input_files` | list of globs | optional | Narrows the staleness fingerprint |
| `before_launch` | before launch | optional | Run or offer it before a launch |
| `kind` | string | required | For `output`: `generated_layer` or `mod` |
| `folder` | string | required for `mod` | The folder it wrote; an empty or relative one is beside its executable |
| `user_configured` | boolean | optional for `mod` | The folder comes from the tool's own settings |
| `file` | path | required | The file a failure marker or required magic names |
| `contains` | string | required for a failure marker | The text that means the run failed |
| `magic` | string | required for required magic | The first bytes as hex pairs, with no separators |

### Tool input and before-launch

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | Local to the tool |
| `label` | string | required | What the player sees |
| `kind` | string | required | `file`, `folder`, `choice`, `text` or `flag` |
| `required` | boolean | optional | Refuse a run without a value |
| `default` | string | optional | The starting value |
| `choices` | list of strings | optional | The values for a `choice` input |
| `arg` | string | optional | The argument the value is passed as, such as `--profile` |
| `mode` | string | required | `run` runs it first; `ask` offers it |
| `when` | condition | optional | `before_launch` applies only when this is true |

### Framework

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | The framework id games and tools name |
| `game` | string | required | The game it belongs to |
| `name` | string | required | What the player sees |
| `version` | string | required | The version this definition describes |
| `supported_runtimes` | list of runtime constraints | optional | Alternatives it supports; may be empty |
| `required_frameworks` | list of strings | optional | Frameworks it needs; may be empty |
| `content` | list of strings | optional | Content it ships; may be empty |
| `launch` | launch recipe | optional | Its own launcher, or `null` |
| `detection` | framework detection | optional | How Agora finds it and reads its version |
| `marker` | path | required for detection | A file whose presence means it is installed |
| `version` | string | optional for detection | `pe_file_version`, or absent when presence is all Agora knows |

A runtime constraint is `{ "game": "...", "stores": [...], "versions": { ... }, "builds": [...] }`.
`stores` and `builds` are lists and may be empty, and `versions` is `{ "kind": "any" }`,
`{ "kind": "exact", "value": ["1.0"] }` or `{ "kind": "semver", "value": ">=1.0, <2" }`.

### User files, saves and sessions

| Field | Type | Required | Meaning |
|---|---|---|---|
| `source` | game path | required | The per-user file |
| `instance_path` | path | required | Its copy in the instance |
| `strategy` | string | required | `redirect`, `virtual_file_system` or `journaled_swap` |
| `stores` | list of strings | optional | Stores this applies to; empty means all |
| `ini` | path | required | The per-user file holding the setting |
| `section` | string | required | The section in that file |
| `key` | string | required | The setting's key |
| `own_value` | string | required | The value that gives an instance its own saves; `{instance}` is its folder name |
| `shared_dir` | game path | required | Where saves live when the setting is absent |
| `relative_to` | game path | required | The folder `own_value` is relative to |
| `processes` | list of strings | optional | Image names whose exit ends the session |
| `normal_exit_codes` | list of numbers | optional | Exit codes that mean a normal quit; empty means `[0]` |

### Launch alternative and runtime files

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | Local to the game |
| `when` | condition | required | When this executable is used |
| `executable` | game path | required | The alternative program |
| `reason` | string | required | Why it was chosen, shown to the player |
| `name` | string | required | The file family's name, such as `SKSE` |
| `family` | glob | required | The files that mean the framework is installed |
| `expected` | string | required | The file this runtime needs; `{1}`..`{9}` and `{version}` are filled |
| `applies_to` | list of globs | optional | Versions the rule applies to; empty means all |
| `repair` | string | required | What to do, shown with the finding |

### Objects in hook calls

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | required | An install's id, or an install capability's opaque key |
| `runtime` | runtime identity | required | Its game, store, version and build |
| `game` | string | required | The game id |
| `store` | string | required | The store id |
| `version` | string | required | An exact version; never treat it as semver unless the game does |
| `build` | string | optional | An opaque build number |
| `kind` | string | required | For an install: `base_game`, `add_on` or `tool` |
| `location` | string | required | An install's display path, or a `user_data` location |
| `volume` | volume info | optional | The install's volume, or `null` when unknown |
| `capabilities` | install capabilities | required | What the install can do |
| `filesystem` | string | required | The volume's filesystem |
| `supports_hardlinks` | boolean | required | The volume supports hardlinks |
| `supports_file_clones` | boolean | required | The volume supports file clones |
| `executables_readable` | boolean | required | Agora may read the install's executables |
| `accepts_new_files` | boolean | required | The install accepts new files |
| `relocatable` | boolean | required | The install may move |
| `instance` | string | required | The instance id |
| `base` | base reference | optional | The instance's base, or `null` |
| `frameworks` | list of installed frameworks | required | The frameworks in the instance |
| `layers` | layer stack | required | The instance's layers |
| `load_order` | load order | required | The entries and rules |
| `settings` | map of string to string | required | The instance's settings |
| `entries` | list of load order entries | required | The listed items |
| `rules` | list of script load-order rules | required | The rules in force |
| `enabled` | boolean | required | Whether the entry is on |
| `locked` | boolean | required | Whether the player locked it |
| `type` | string | required | For a suggestion: `place`, `installer` or `unknown` |
| `source_path` | path | required for `place` | The folder inside the item to place |
| `mount_path` | path | required for `place` | Where it goes |
| `reason` | string | required | A sentence explaining the reply |
| `top_level` | list of strings | required for `unknown` | The names Agora could not place |
| `note` | string | optional | Why Agora could not tell |
| `add` | object | required | For a recognition: `{ "mode": "archive" }` or `{ "mode": "file", "folder": "..." }` |
| `mode` | string | required | For `add`: `archive` or `file` |
| `code` | string | required | A stable diagnostic code |
| `message` | string | required | The diagnostic in words |
| `severity` | string | required | `info`, `warning` or `error` |
| `repairs` | list of strings | required | Suggested repairs, may be empty |
