# Community plugins: experimental author guide

The plugin host uses API **0.1.1** (0.1.1 added content providers; every 0.1.0 plugin still loads). It adds behavior to Agora, and support for whole games, without rebuilding Agora. This is a development feature; the public release acceptance gates are recorded in `implementation-status.md`.

Adding a game? See [Adding a game to Agora](game-packages.md). The other plugin docs are [content providers](providers.md), [publishing](publishing.md) and [implementation status](implementation-status.md).

## Try the examples

Build the desktop application, open **Settings → Services → Plugins**. Choose **Load a folder** and select one of the directories in `examples/plugins`. Review the requested permissions, and allow the plugin to run its code when asked (see "Who may run code" below). Restart Agora to run startup activation. Pages and commands also activate their scripts when first invoked.

- `dashboard`: reads real instances, renders a host-owned table, and saves a count to plugin storage from a button.
- `diagnostics`: lists disabled mods in an instance and proposes enabling one. The instance's integration area shows findings and a repair review; no repair runs until the user approves it. Its launch reminder feeds into the normal launch flow.
- `theme`: adds selectable Forest accents. Choose it in the Plugins page. Built-in appearance preferences remain stored and resume when the plugin is disabled.
- `home-replacement`: offers to render Agora's home screen instead of adding a page. Installing it changes nothing until you choose it under **Who draws each screen**, and Agora's own screen comes back the moment you disable it.
- `provider`: adds a small content source to Browse. Its projects appear next to curated and Modrinth results, have their own detail page, and install through the same reviewed install flow. Written up, with the trust model, in [`providers.md`](providers.md).

## Official plugins in this repository

Some plugins are Agora's own. They live under `packages/official/<name>/`, and the signed registry
pins them in `registry/official-plugins/<plugin-id>.json`. There are two for Skyrim:
`packages/official/creation-engine/` (plugin id `agora.creation-engine`, no game) holds the shared
Creation Engine rules and a script, and `packages/official/skyrim-se/` (plugin id
`agora.skyrim-se`, game id `skyrim-se`) inherits them. The app does not compile Skyrim in, so
`agora games list` shows it only when both plugins are installed.

To use them while developing, install both folders as development plugins, base first. The base
ships code, so allowing it is a step of its own:

```bash
agora --data-dir ./tmp/agora-dev plugin install --yes development packages/official/creation-engine
agora --data-dir ./tmp/agora-dev plugin install --yes development packages/official/skyrim-se
agora --data-dir ./tmp/agora-dev games list
```

The CLI tests install them the same way. To build each release zip reproducibly:

```bash
python scripts/build_official_plugin.py creation-engine
python scripts/build_official_plugin.py creation-engine --verify-registry
python scripts/build_official_plugin.py skyrim-se
python scripts/build_official_plugin.py skyrim-se --verify-registry
```

`--update-registry` rewrites the signed-registry entry from the built bytes; `--verify-registry`
refuses an entry that does not pin them. The release workflow attaches each zip to its own
`official-<name>-v<version>` release, which is the URL the entry names.

The same operations exist in the standalone CLI, which is usually faster while iterating:

```bash
agora plugin install --yes development ./my-plugin
agora plugin list
agora plugin log author.my-plugin
agora plugin disable-all
```

`agora plugin --help` lists the rest, and `docs/CLI.md` documents consent, JSON output, and the
data-preserving removal default. The CLI and the desktop app drive the same core service, so
they always agree about what is installed.

Every folder contains `agora-plugin.json` at its root. A local ZIP must have that file at the archive root, not inside an extra enclosing folder. Load folders during development; ZIP installation copies files into Agora's package directory. Never distribute secrets in either format. For publishing updates from your own host, and what the signature on them does and does not prove, see [publishing.md](publishing.md).

## The manifest

`agora-plugin.json`, at the root of your folder or ZIP. Here is one with every field, so you do not
have to infer any of them:

```jsonc
{
  // The *schema* version of this file, always 1. Note the field is called
  // `manifest` — it is not called `schema`.
  "manifest": 1,

  // publisher.plugin — lowercase letters, digits and inner hyphens in each half.
  // Not verified by anyone; it is a namespace, not a claim of authorship.
  "id": "acme.census",
  "name": "Mod census",
  "version": "1.0.0",                 // semver, for your plugin
  "license": "MIT",                   // SPDX identifier. Required.
  "description": "Counts things.",    // optional
  "source": "https://github.com/...", // optional, must be https

  // Which plugin API versions you support. Not Agora's version — the two are
  // separate. Pin `>=0.1, <0.2` while the API is experimental.
  "apiRange": ">=0.1, <0.2",

  // Package-relative. No leading "./", no "..", no absolute paths.
  // Bundled JavaScript only: .js or .mjs. TypeScript is compiled by you.
  "entrypoint": "main.js",

  // Optional, and only with an `entrypoint`: one plain-text sentence (at most
  // 240 characters, no control characters) saying what stops working when the
  // user installs without letting your code run. The install prompt shows it
  // beside that choice, so people can decide with the cost in front of them.
  "withoutCode": "Sorting and the saved masterlist will not be used.",

  // When your script is loaded. Omit this and it loads at startup. Each entry
  // must name something you actually contribute, or it would never fire.
  //   onStartup | onInstanceOpened | onView:<id> | onCommand:<id> | onEvent:<name>
  // `onView:` covers pages, instance panels and replacements.
  "activation": ["onView:panel", "onCommand:recount"],

  // An object with two arrays — not a bare array.
  // `required` must all be granted or the plugin will not install.
  // `optional` are granted if the user agrees and simply absent if not, so
  // your code must cope without them.
  "capabilities": {
    "required": ["instance:read", "content:read"],
    "optional": ["launch:prepare"]
  },

  // Other plugins that must be installed and enabled, by version range.
  "dependencies": { "other.library": ">=1.2, <2" },

  // Bump when the shape of what you put in `storage` changes. Agora keeps a
  // copy of the old data before loading the new version, which the user can
  // restore if your migration goes wrong.
  "dataVersion": 1,

  // Only meaningful with the `network` capability. Hostnames (each also covers
  // its subdomains; `*.example.com` means the same as `example.com`): no IP
  // literals, no ports, no localhost. At most 10. `"*"` (any host) or a longer
  // list installs only when the user has Reduced security mode on.
  "network": { "hosts": ["api.example.com"] },

  "contributions": {
    // A full-width page in the sidebar.
    "pages": [
      { "id": "overview", "title": "Overview", "icon": "LayoutDashboard",
        "view": { "kind": "host", "export": "overview" } }
    ],

    // A panel inside an opened instance. Your export receives { instanceId }.
    "instancePanels": [
      { "id": "panel", "title": "Mod census",
        "view": { "kind": "host", "export": "panel" } }
    ],

    // Command palette, and optionally an instance's context menu. An
    // instance-context command receives { instanceId }.
    "commands": [
      { "id": "recount", "title": "Recount", "export": "recount",
        "surfaces": ["palette", "instance-context"] }
    ],

    // Settings the host renders and validates. An ARRAY, and each entry has a
    // `key` — it is not an object keyed by setting name.
    "settings": [
      { "key": "warn", "title": "Warn before launching", "type": "boolean", "default": true },
      { "key": "label", "title": "Label", "type": "string", "default": "hi", "maxLength": 40 },
      { "key": "limit", "title": "Limit", "type": "number", "default": 5, "min": 1, "max": 10 },
      { "key": "density", "title": "Density", "type": "enum", "default": "compact",
        "options": [{ "value": "compact", "label": "Compact" },
                    { "value": "roomy", "label": "Roomy" }] }
    ],

    // Checks the user runs against an instance. Export returns a
    // DiagnosticReport; receives { instanceId }.
    "diagnostics": [
      { "id": "check", "title": "Check mods", "export": "check" }
    ],

    // Runs before a launch. Same DiagnosticReport shape, same { instanceId }.
    // `timeoutMs` is clamped to 5000 by the host. `onFailure` is "warn"
    // (default — Agora warns, the user decides) or "block".
    "launchChecks": [
      { "id": "preflight", "title": "Preflight", "export": "preflight",
        "timeoutMs": 1500, "onFailure": "warn" }
    ],

    // Offer to render one of Agora's own screens. See below.
    "replacements": [
      { "id": "home", "title": "Compact", "surface": "home",
        "view": { "kind": "host", "export": "home" } }
    ],

    // Colour tokens. Declarative — a theme needs no script at all.
    "theme": { "id": "forest", "title": "Forest", "light": {}, "dark": {} },

    // Game packages defined by this plugin. Requires `game:define`.
    "gamePackages": [
      { "path": "games/package.json" }
    ],

    // Crash doctor packs: sources, parsers and rules. Requires `crash:contribute`.
    // Core reads and lints them; script parsers run through the same host and
    // see only the artifact the call puts in scope.
    "crashPacks": [
      { "path": "crash/pack.json" }
    ]
  }
}
```

Everything under `contributions` is optional; a theme-only plugin needs no `entrypoint` at all.
`withoutCode` is the author's own plain-text sentence about what its code is for; it only makes
sense with an `entrypoint`, and without one it is a validation error. `examples/plugins/` has
runnable versions of each of these. Where this document and
`crates/agora-plugin-api/src/manifest.rs` disagree, the code is right and this is a bug — please
report it.

A plugin installed or enabled while Agora runs takes effect on the next start.

### Crash packs

A crash pack is JSON with `sources`, `parsers`, `inspectors`, `ref_maps`, `roles` and `rules`. A
`ref_map` says how a ref kind traces to the content that provides it:

| `lookup` | What it reads |
|---|---|
| `path` | Files in the instance view, matched by `patterns`; `archives` also searches inside listed archives |
| `index` | A name the instance view indexed for its items, such as a declared mod id |
| `facts` | A fact of the same analysis, such as the mod list in the crash report being read. It needs `kind`, `key` (the attribute matched against the ref) and `name` (the attribute shown, falling back to `key`) |

`match` is `exact`, `casefold`, `prefix_dot` or `segment`. `segment` is valid only with a `facts`
lookup: it splits the ref value at `.`, `$` and `/`, drops the last part (the class's own name),
and compares the rest with the fact key after lower-casing and keeping only ASCII letters and
digits. Keys under three characters never match. `ignore` lists segments that never match, so a
class in a `net.example.client` package does not name a mod just because one of those words is its
id. If two different keys match, the longer wins; two of the same length are ambiguous and resolve
to nothing.

A kind may have more than one ref map. They are tried in order, and a later one is used when an
earlier one finds nothing. Every finding still names the plugin that made it.

An `inspector` opens one file a rule's `verify` points at and reports `pass`, `fail` or `unknown`.
It gets the same request and read scope as a parser, with the target file as its artifact, so it
still never sees a path. `accepts` lists the file names it will open (case-insensitive `*` and
`?`). A rule's `verify.target` names a ref-bound variable; the engine resolves that ref through the
same ref maps as attribution before the inspector runs, and only an instance has a file to open. A
missing target, a file the inspector does not accept, or a file outside the instance view is
`missing`, which takes the rule's `on_unknown` effects. A script error, timeout or invalid output is
"could not check" and names the plugin, never a pass.

A rule that matches more than 32 times does not fire, and says so: a match set that large usually
means the rule is too loose. A rule meant to check every item in a list (each script-extender
plugin a log lists, say) can raise its own limit with `"max_branches"`, up to 512.

### The runtime

Scripts run in QuickJS through `rquickjs 0.13`, with separate bounded runtimes on host-owned worker threads. This is **not an OS process sandbox**. QuickJS supports ES modules, promises, and async functions; the host adds the `agora` module. It does not add `require`, `process`, filesystem access, DOM APIs, native modules, or global `fetch`. Bundle third-party JavaScript and mark `agora` external. Use the standalone declarations in `sdk/` for TypeScript.

Scripts run in QuickJS through `rquickjs 0.13`, with separate bounded runtimes on host-owned worker threads. This is **not an OS process sandbox**. QuickJS supports ES modules, promises, and async functions; the host adds the `agora` module. It does not add `require`, `process`, filesystem access, DOM APIs, native modules, or global `fetch`. Bundle third-party JavaScript and mark `agora` external. Use the standalone declarations in `sdk/` for TypeScript.

View and command calls have a ten-second host deadline. Diagnostic calls use their host deadline, and launch checks clamp the manifest timeout. Exceptions, timeouts, and allocation limits produce errors. These controls do not establish protection against native engine defects. Windows/MSVC was tested locally; other platform packaging remains an acceptance gate.

### WebAssembly modules

A plugin can ship a WebAssembly module and run it with `agora.wasm.run(module, options)`. A module is a WASI command: it reads a request from stdin and writes its answer to stdout. `module` is a path inside your package, so it is hash-pinned with the rest of the plugin.

Running a module needs no capability. It gets a clock, randomness, stdin, stdout and stderr, and nothing else: no network, no environment variables, no arguments beyond the module name, and no writable folder. A module that traps or overruns its limits returns an error.

`options.stdin` is a string or bytes. The result is `{ exitCode, stdout, stderr, stdoutText, stderrText }`. `stdout` and `stderr` are byte arrays; the `*Text` fields are the same bytes decoded as UTF-8.

Mounts are read-only and staged into a temporary folder that is deleted afterwards:

- `{ guest: "/data", view: { folder: "Data", patterns: ["*.esp"] } }` stages matching files from the current hook's instance view. It needs `game:read` and a hook bound to an instance, exactly like `game.readBytes`.
- `{ guest: "/input", files: { "name.yaml": "..." } }` stages bytes your script supplies.
- `{ guest: "/input", stored: ["masterlist.yaml", "prelude.yaml"] }` stages blobs from your own `storage.putBytes` store, hardlinked or copied into the sandbox. Nothing travels through the script.

Guest paths are absolute and cannot nest or repeat. `patterns` is a glob list; an empty list means every file below `folder`. A folder that exists but matches nothing is a mount with no entries; a folder that is missing or unreadable is an error naming it, and the module does not run.

Staging has its own allowance, separate from the `game:read` byte budget: 4 GiB and 100,000 files per run. Staged bytes stay inside the sandbox and only the module's capped stdout and stderr come back, so hardlinking spends this allowance but not the script's read budget. Lower it with `limits: { mountBytes, mountFiles }`; you cannot raise it. A mount over the allowance is an error naming that mount. The copy fallback reads bytes but still counts against the same allowance.

A module gets 1 GiB of memory and 120 seconds unless you ask for less with `limits: { memoryBytes, timeoutMs }`. Its table is capped separately. You cannot raise any limit. The engine is wasmtime, and its compile cache lives under the data root. The wall-clock deadline covers staging, compilation and the run, including time spent in a host call. At most two modules compile at once across the whole process; a call that finds no free slot waits within its deadline and then fails as a timeout.

### Plugin file storage

`storage.putBytes(key, bytes)` stores a blob for this plugin and returns `{ written }`. `storage.getBytes(key)` returns a `Uint8Array`, or `null` when the key was never saved. `storage.deleteBytes(key)` returns `{ deleted }`.

Keys are plain names: no `/`, `\`, `..`, NUL, a Windows device name such as `CON` or `LPT1`, or any other path or stream syntax. The store is per plugin, so one plugin cannot read another's blobs. A write is atomic: core writes a temp file and renames it over the old one, so a failed write leaves the old bytes. A plugin may hold 256 MiB in total; a write over that is an error, and the old bytes stay.

A `stored` mount reaches the same blobs from a module without passing them through the script.


## Service API

| SDK | Capability | Behavior |
|---|---|---|
| `instances.list/get` | `instance:read` | Stable instance DTOs |
| `instances.rename/setMemory` | `instance:write` | Existing instance services |
| `content.list` | `content:read` | Installed content DTOs |
| `content.enable/disable/setUpdatePinned` | `content:write` | Existing content services |
| `launch.state/history` | `launch:read` | Read launch information |
| Launch-check contributions | `launch:prepare` | Bounded checks before normal GUI launch preflight |
| `storage.get/set/all/remove/forInstance` | No extra capability | Small namespaced values |
| `storage.putBytes/getBytes/deleteBytes` | No extra capability | This plugin's blob store: files under the plugin's own data folder, plain keys, atomic writes, 256 MiB total |
| `settings` | No extra capability | Activation-time snapshot of declared settings; restart to refresh |
| `ui.refresh/notify`, `log` | No extra capability | Attributed UI messages and plugin logs |
| `on/off` | Event's read capability | Subscribe/unsubscribe to documented events |
| `net.fetchJson` | `network` | Declared host allowlist, network opt-in, and live lockdown policy |
| Game packages | `game:define` | Define games: their stores, versions, folders and how to launch them |
| `crash.readBytes/readLines` | `crash:contribute` | Scoped reads of the artifact in a parser or inspector call: bytes or text, never a path |
| Crash packs | `crash:contribute` | Read the crash evidence Agora gives them and add rules to its diagnosis; every finding names the plugin |
| `game.readBytes/readLines/list` | `game:read` | Scoped reads of one instance's merged view by game path, with a byte budget; a script never sees a real path |
| `game.writeBytes/writeText` | `game:write` | Write into this plugin's own generated layer for one instance, with byte and file budgets; it cannot change the game, a save, or another layer |
| `wasm.run` | No extra capability | Run a shipped WASI command in a sandbox; a view mount needs `game:read` |
| `behaviours` in a game package | `game:define` | `detect_runtime`, `classify`, `recognize_download`, `load_order_rules`, `diagnostics`, `prepare_launch` and script load-order parsing/rendering, each time- and memory-limited by the plugin runtime |

All service failures reject the promise with a structured error code and message. Treat errors as failures; never display a successful mutation after a rejection. The low-level `call` function exposes the same capability checks, not an escape hatch. There is no shell or arbitrary executable API. Current method definitions are in `agora-core/src/plugins/dispatch.rs`.

## What your exports receive, and what they must return

Import the types from `agora` and let `tsc` check this for you — the shapes below are declared in
`sdk/index.d.ts`. Unknown fields are **ignored, not rejected**, so a misspelled one produces a view
that renders with something quietly missing rather than an error.

| Contribution | Receives | Returns |
|---|---|---|
| Page | `null` | `ViewModel` |
| Instance panel | `{ instanceId }` | `ViewModel` |
| Replacement (`home`) | `null` | `ViewModel` |
| Replacement (`instance-overview`) | `{ instanceId }` | `ViewModel` |
| Palette command | `null` | anything serialisable |
| Instance-context command | `{ instanceId }` | anything serialisable |
| Action button | the button's `args`, else the view's args | anything serialisable |
| Diagnostic | `{ instanceId }` | `DiagnosticReport` |
| Launch check | `{ instanceId }` | `DiagnosticReport` |

### A view is a list of blocks

A `ViewModel` is `{ title?, subtitle?, blocks: [] }`. **The `blocks` array is required** — a bare
table is not a view. Each block is one of `heading`, `text`, `stats`, `table`, `list`, `status`,
`actions`, `divider`, discriminated by `type`.

Table rows are arrays of **cells**, in column order — not objects keyed by column. A cell is
`{ type: "text", text }`, `{ type: "badge", text, tone? }` or `{ type: "flag", value }`. Columns
carry a `label` and optional `align`; they have no id.

```js
export async function panel({ instanceId }) {
  // Positional, not an options object — see `sdk/index.d.ts`.
  const items = await content.list(instanceId);
  return {
    title: 'Mod census',
    blocks: [
      { type: 'stats', items: [{ label: 'Installed', value: String(items.length) }] },
      {
        type: 'table',
        columns: [{ label: 'Name' }, { label: 'Enabled' }],
        rows: items.map((item) => [
          { type: 'text', text: item.displayName ?? item.filename },
          { type: 'flag', value: item.enabled },
        ]),
        emptyMessage: 'Nothing installed yet.',
      },
    ],
  };
}
```

At most 200 blocks per view and 500 rows per table. Going over is an error, not a truncation, so a
view that loops while building itself says so rather than freezing the window.

### Values you will be comparing against

Two vocabularies are easy to guess wrong, so they are declared in `sdk/index.d.ts` as `Loader` and
`ContentType` and repeated here:

- **`Instance.loader`** is `vanilla`, `fabric`, `forge`, `neoforge` or `quilt`. `vanilla` is the
  sentinel for "no modloader"; an empty string also occurs on older instances, so test for both.
- **`ContentItem.contentType`**, and `content.list`'s optional filter, is **singular**: `mod`,
  `resourcepack`, `shader`, `datapack`, `world`. Not `mods` — a plural filter matches nothing and
  returns an empty list rather than an error.

Both are typed as a union widened with `string`, so a newer Agora adding a value does not break
your build.

### Diagnostics and launch checks

Both return a `DiagnosticReport`: `{ findings: [], incompleteReason? }`. An empty `findings` array
means "nothing wrong" — that is the success case, not `null`. Set `incompleteReason` when you could
not finish; a partial report with an honest note beats a clean report that silently checked nothing.

A finding needs a stable `id` (so the host can tell "still broken" from "broken again"), a `title`,
and optionally `severity` (`info` | `warning` | `error`, default `info`), a `summary`, `evidence`
label/value pairs, and `repairs`.

```js
export async function preflight({ instanceId }) {
  const items = await content.list(instanceId);
  const enabled = items.filter((item) => item.enabled);
  if (enabled.length > 0) return { findings: [] };
  return {
    findings: [{
      id: 'no-enabled-mods',
      title: 'No mods are enabled',
      severity: 'warning',
      summary: 'This instance has a modloader but every mod is switched off.',
      evidence: [{ label: 'Installed', value: String(items.length) }],
    }],
  };
}
```

Neither needs an activation event. Opening a diagnostic or starting a launch runs your script if it
is not already running, the same way opening a contributed page does. Declare `onView:`/`onCommand:`
for the surfaces that have them and leave `activation` empty otherwise.

A launch check that throws or times out is reported and the launch continues — `onFailure: "warn"`
is the default because Agora warns rather than vetoing. `onFailure: "block"` stops the launch.
`timeoutMs` is clamped to 5000.

Repairs are a **closed set** of actions the host knows how to perform: `disableContent`,
`enableContent`, `pinContentUpdate`, `unpinContentUpdate`, `setJvmMemory`, `resetJvmArgs`,
`createSnapshot`. You propose one; the host re-validates the target and performs it through the same
service the GUI uses. It reports applied, failed and stale actions separately, and a stale result is
not success. Repairs offered together must not contradict each other.

Theme tokens accept six-digit hex colors. Supported tokens are background, foreground, surface, surface-foreground, primary, primary-foreground, accent, accent-foreground, border, muted, muted-foreground, and destructive. Unknown names and other color syntax are ignored by the renderer. Light and dark token maps apply only to the corresponding mode. Theme CSS never loads remote resources.

## Replacing a built-in screen

A plugin can offer to render one of Agora's own screens instead of adding a new one:

```json
"replacements": [
  {
    "id": "home",
    "title": "Compact",
    "description": "One dense list instead of cards.",
    "surface": "home",
    "view": { "kind": "host", "export": "home" }
  }
]
```

Declaring this does **not** take the screen over. It puts "Compact" on a list in Settings, and
Agora's own screen renders until a user chooses yours. Two plugins offering the same surface is a
list of two, not a race decided by install order, and the user can always go back. Write the
`title` to say what your version *is* rather than repeating the surface name — it appears beside
Agora's own entry.

`surface` is a closed set: `home` and `instance-overview`. A surface joins it when the launcher can
genuinely hand it over; naming one a plugin can declare but never render would be worse than not
offering it. Adding a surface later is additive, so a manifest written today keeps working.

`instance-overview` receives `{ instanceId }`, so one view serves every instance rather than the
plugin having to work out which is open.

Only `kind: "host"` may replace a surface. A replacement is the whole screen, and the host-rendered
path is themed, accessible and controller-navigable by construction. Declare `onView:<id>` with the
replacement's id so your plugin starts when the screen opens.

Expect to be fallen back on. If your plugin is disabled, removed, or fails to start, Agora renders
its own screen and tells the user why; the choice is remembered, so fixing the problem restores it.
`examples/plugins/home-replacement/` is a working example, including how to handle an optional
capability the user declined.

## Custom views were withdrawn

`view: { kind: "custom", html: "..." }` shipped as a prototype in an early 0.1 build and has been
**removed**. A manifest still declaring one is refused with a message pointing here.

It was withdrawn rather than finished because the script inside that frame ran in the WebView,
outside every bound the plugin runtime exists to impose. A QuickJS plugin gets a memory ceiling, an
interrupt handler and a deadline; a 512 KiB HTML document got none of them and could hang the
launcher with `while (true) {}`. A bounded file and a throttled command bridge do not make a
bounded view. Accessibility and controller navigation were also the author's problem inside the
frame, in an application where controller support is first-class — and the frame's isolation could
not be demonstrated for the packaged app, because Tauri documents that on some platforms it cannot
distinguish IPC from an embedded frame from IPC from the window containing it.

Use `{ kind: "host", export: "..." }` and return a view model. If you have an interaction the view
model genuinely cannot express — a graph, a map, a spatial editor — that is a good reason to open
an issue asking for a new block type. It is not a reason to reintroduce a second renderer: the
right fix adds a component every plugin can use, themed and navigable, rather than one plugin's
private HTML.

## Who may run code: one answer per plugin, and an emergency stop

A plugin's **scripts** (anything with an `entrypoint`) run only if the user allowed *that plugin* to run code. This is asked once, with the install, as part of the install consent: "<name> runs code. Allow it?" The user can allow it, or install the plugin without running its code. The answer is stored per plugin in local state (never in the manifest) and can be changed later in Settings -> Plugins (the "Allow code" switch on the plugin's row) or with `agora plugin scripts <ID> allow|deny`. Turning it off stops that plugin now. One plugin's answer never affects another's, and it is the same rule for every plugin that has code. Official plugins follow the same rule: they may carry scripts, and the signed package hash covers them. Updating a plugin keeps the answer for the same code; a version that brings new or changed code, or code from a different publisher, asks again and does not run until the user says so. An update signed by a different publisher than the one pinned at install is shown as such in the update prompt and has to be confirmed explicitly.

Above that sits the **emergency stop**, Settings -> Plugins -> "Stop all plugin code" (`agora plugin stop` / `agora plugin resume`). It is **not engaged** by default. While it is engaged no plugin script runs, whatever each plugin was allowed. Engaging it also stops what is running right now. It is stored as `plugins_emergency_stop`. An unreadable or malformed value counts as engaged.

**Data-only plugins run no code**, so neither of these applies to them: a plugin that only contributes game packages (Skyrim support, for example) loads whatever the stop says, and has no code to allow. Their install consent (the capabilities, `game:define`) is the opt-in. A plugin that has both a game package and scripts loads the game package regardless and runs its scripts only under the two rules above.

The old `plugins_enabled` setting ("community plugins on", off by default) is gone. It meant the opposite of the stop and was seeded `false` in every database, so reading it with the new meaning would have engaged the stop for everyone. It is ignored and treated as absent, whatever value it holds.

Official plugins (listed in Agora's signed registry) may carry scripts like any other plugin; the signed hash covers their code too. What official status changes is verification and loading: an official plugin whose installed files no longer match the hashes recorded at install, one the signed catalog explicitly revokes, and any plugin that uses an official plugin's id without being it, is **blocked** (status "Blocked", with the reason) and its game packages do not load. If the catalog itself cannot be checked (missing, corrupt, offline) or no longer lists an old version, the installed files are checked against the hashes recorded at install: matching files keep loading with a warning on the plugin and its games, and an Update action where the catalog offers one. A data-only plugin keeps working in that state; a plugin with scripts does not run them until a verified catalog says it is official again.

## Recovery and compatibility

Per-plugin disable removes contributions and subscriptions; uninstall asks separately about retained data. **Turn all off** disables every plugin; the emergency stop (above) stops all plugin code without changing which plugins are enabled or allowed, so releasing it puts things back the way they were. Built-in destinations remain available. Saved plugin destinations show an actionable fallback if unavailable.

API 0.1 is experimental. Pin its range and keep data migrations backward compatible. The implementation does not yet provide complete update rollback, signed package distribution, or a stable support window. Do not use package bytes alone as proof that plugin data can be rolled back. These limitations must be resolved before API v1 is advertised as stable.
