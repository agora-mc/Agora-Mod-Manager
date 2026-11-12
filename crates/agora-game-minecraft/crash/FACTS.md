# Minecraft crash facts

The built-in Minecraft crash pack (`pack.json`) ships with three parsers compiled into Agora:
`crash_report`, `latest_log` and `hs_err`. They use only the crash contract's scoped reads
(`read_bytes`, `read_lines`) and return the same `ParserOutput` a plugin's script returns. A
plugin's crash pack for Minecraft can query every fact below and add rules of its own.

A generic core parser was not enough here: the formats need cause chains in order, per-loader mod
tables, a stable bucket identity, and facts that carry refs. A script through the parser bridge
needs an installed plugin package, which a built-in pack does not have. The Rust parsers use only
the public parser contract, so a plugin's script can produce the same facts.

## Artifact kinds

| Parser | Artifact |
|---|---|
| `crash_report` | `crash-reports/crash-*.txt`, including a report embedded in a log line |
| `latest_log` | `logs/latest.log` |
| `hs_err` | `hs_err_pid*.log` in the instance root or `logs/` |

Empty input is an error ("there is nothing to read"), never a clean parse. A file that only looks
like the format returns a `wrong_format` note and no facts, so the engine reads "could not check".

## Save risk

This pack declares no `save_risk`: every Minecraft suspect is `unknown` when an instance is
present. A mod jar can add blocks, items, entities or dimensions to a world, and there is no cheap
way to tell from the jar alone, so Agora does not guess that any Minecraft mod is safe to remove
from an existing world. The answer tells the user to back the world up first.

## Facts

An example below is invented. `mod_id` refs name a mod; `class` refs name a Java class; `module`
refs name a native library; `file` refs name the jar a frame annotation named. The pack's
`ref_maps` say how each kind is looked up.

### `core.exception`

| Attribute | Meaning |
|---|---|
| `class` | the exception class, dotted |
| `category` | one of the contract's categories: `managed`, `oom`, `load_failure`, `assert`, `access_violation`, `fast_fail`, `unknown` |
| `message` | the exception message, when it printed one |
| `depth` | 0 for the outermost exception, then 1 for each `Caused by` |
| `access`, `fault_address`, `near_null`, `pointer_class` | native faults only (`hs_err`; `pointer_class` describes how a bad address was formed) |

Refs: `class` (Mention) from a crash report or log. Provided by all three artifact kinds.

```json
{ "kind": "core.exception",
  "attrs": { "class": "java.lang.NullPointerException", "category": "managed", "message": "widget was null", "depth": 1 },
  "refs": [{ "kind": "class", "value": "java.lang.NullPointerException", "strength": "mention" }] }
```

### `core.frame`

| Attribute | Meaning |
|---|---|
| `index` | 0 is the top of its stack trace |
| `kind` | `managed` for a Java frame, `probable` for the `hs_err` problematic frame |
| `symbol_scheme` | `jvm` or `native` |
| `symbol` | `class.method`, or `library+offset` for native |
| `source_file`, `line` | from `(Thing.java:42)`, when present |

Refs: `class` (Exec; the first frame of the deepest cause is ExecTop), `file` (Mention, the jar a
Forge/NeoForge frame annotation names), `module` (ExecTop, the library an `hs_err` frame is in).
Provided by all three artifact kinds.

```json
{ "kind": "core.frame",
  "attrs": { "index": 0, "kind": "managed", "symbol_scheme": "jvm", "symbol": "com.example.mod.ExampleWidget.draw", "source_file": "ExampleWidget.java", "line": 88 },
  "refs": [{ "kind": "class", "value": "com.example.mod.ExampleWidget", "strength": "exec_top" },
           { "kind": "file", "value": "examplemod-1.2.3.jar", "strength": "mention" }] }
```

### `core.module`

Refs: `module`. Attributes: none here (the contract allows `version`, `sha256`, `base_address`,
`size`). Provided by `hs_err`.

```json
{ "kind": "core.module", "attrs": {}, "refs": [{ "kind": "module", "value": "examplegfx.dll", "strength": "mention" }] }
```

### `core.process`

| Attribute | Meaning |
|---|---|
| `image_version` | the game version the report or log states |
| `pid` | the process id an `hs_err` header states |

Provided by all three artifact kinds.

```json
{ "kind": "core.process", "attrs": { "image_version": "1.20.1" } }
```

### `core.system`

| Attribute | Meaning |
|---|---|
| `os` | the operating system line |
| `ram_free_mb` | free physical memory, MiB (`hs_err`) |

```json
{ "kind": "core.system", "attrs": { "os": "Windows 11 (amd64) version 10.0" } }
```

### `core.dependency_problem`

| Attribute | Meaning |
|---|---|
| `state` | `absent`, `disabled` or `wrong_version` |
| `requirement` | the range or version the loader named |
| `needed_by` | the loader id of the mod that needs it |

Refs: `mod_id` of the missing or wrong mod. Provided by `latest_log`. A recommendation
(`recommends ...`) is deliberately not a dependency problem; only a hard requirement is.

```json
{ "kind": "core.dependency_problem",
  "attrs": { "state": "absent", "requirement": "2.0.0 or later", "needed_by": "examplemod" },
  "refs": [{ "kind": "mod_id", "value": "examplelib", "strength": "mention" }] }
```

### `core.log_line`

| Attribute | Meaning |
|---|---|
| `level` | `error` for the first fatal line |
| `section` | `fatal` |
| `text` | the line itself |

Provided by `latest_log`. Use it only to quote; prefer a fact.

```json
{ "kind": "core.log_line", "attrs": { "level": "error", "section": "fatal", "text": "java.lang.OutOfMemoryError: Java heap space" } }
```

### `minecraft.system`

| Attribute | Meaning |
|---|---|
| `java_version` | the Java version line |
| `java_vendor` | the vendor after the comma, when present |
| `java_bits` | 32 or 64 when the file states it |
| `memory_max_mb` | the JVM's maximum heap, MiB |
| `java_vm` | the `hs_err` Java VM line |
| `launcher_brand` | the launcher a crash report names (`crash_report`) |
| `os` | from a log line, when the report has no System Details |

Provided by all three artifact kinds. A rule can compare `java_version` with a version gate, or
`java_bits` with a number.

```json
{ "kind": "minecraft.system", "attrs": { "java_version": "17.0.8", "java_vendor": "Example JDK Vendor", "java_bits": 64, "memory_max_mb": 3814 } }
```

### `minecraft.loader`

| Attribute | Meaning |
|---|---|
| `id` | `fabric`, `quilt`, `forge` or `neoforge` |
| `version` | the loader version the report or log states |

Provided by `crash_report` and `latest_log`.

```json
{ "kind": "minecraft.loader", "attrs": { "id": "fabric", "version": "0.16.0" } }
```

### `minecraft.mod`

| Attribute | Meaning |
|---|---|
| `id` | the loader's mod id |
| `version` | the version as listed |
| `name` | the display name, when listed |
| `file` | the jar name, in the Forge/NeoForge table |
| `state` | the loader's state column (`DONE`, `COMMON_SET`, `Fabric`, ...) |

Refs: `mod_id`. Provided by `crash_report`, in the Fabric, Quilt, Forge and NeoForge list formats
(including the older Forge `Name Version (Mod Id)` table).

```json
{ "kind": "minecraft.mod", "attrs": { "id": "examplemod", "version": "1.2.3", "name": "Example Mod", "file": "examplemod-1.2.3.jar", "state": "COMMON_SET" },
  "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "mention" }] }
```

### Naming a mod from the report's own mod list

A class frame names a Java class, not a mod. The pack's second `class` ref map reads the
`minecraft.mod` facts in the same report: it keys each fact by its `id` and shows its `name`.
Matching is `segment`: the ref value is split at `.`, `$` and `/`, the last part (the class's own
name) is dropped, and every other part is normalised to lower-case ASCII letters and digits. A mod
id matches when it equals one of those parts. Ids under three characters never match, and the map's
`ignore` list rules out words too common to identify a mod (`client`, `server`, `common`, `api`,
`core`, `mixin`, `util`, `lib`, `impl`, `mod`, `mods`, and the package roots `net`, `com`, `org`,
`io`, `me`, `dev`). If two different keys match, the longer wins; two different keys of the same
length are ambiguous and resolve to nothing. The first `class` map is still the instance's
`java_package` index, so an installed jar index wins when there is one; the facts map is tried when
that map misses. A report with no mod list, or an empty one, leaves the class naming itself.

### `minecraft.crash`

| Attribute | Meaning |
|---|---|
| `description` | the report's `Description:` line |
| `time` | the report's `Time:` line |

```json
{ "kind": "minecraft.crash", "attrs": { "description": "Rendering an example entity", "time": "2026-01-02 03:04:05" } }
```

### `minecraft.section`

| Attribute | Meaning |
|---|---|
| `name` | the text inside a `-- Name --` walkthrough header |

```json
{ "kind": "minecraft.section", "attrs": { "name": "System Details" } }
```

### `minecraft.mixin_failure`

| Attribute | Meaning |
|---|---|
| `severity` | `critical` |
| `config` | the mixin config, without its `:MixinClass` suffix |
| `target` | the class the mixin tried to change, when the line names one |

Refs: `mod_id` of the owning mod. For the `Mixin apply failed` form that does not name a mod, the
config's leading token is used and marked Mention; it is usually the mod id. A crash report's
`Mixin [...] ... FAILED during` line is read too; when another transformer took the target
(`removed by another injector`, `could not find any targets matching`) the owner is the victim, so
no fact is emitted. Provided by `crash_report` and `latest_log`.

```json
{ "kind": "minecraft.mixin_failure", "attrs": { "severity": "critical", "config": "examplemod.mixins.json", "target": "net.minecraft.class_123" },
  "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "exec_top" }] }
```

### `minecraft.duplicate_mod`

| Attribute | Meaning |
|---|---|
| `id` | the duplicated mod id |

Refs: `mod_id`. Provided by `latest_log` (`Found a duplicate mod ...`).

```json
{ "kind": "minecraft.duplicate_mod", "attrs": { "id": "examplemod" }, "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "mention" }] }
```

### `minecraft.loader_problem`

| Attribute | Meaning |
|---|---|
| `kind` | `resolution`, `incompatible`, `duplicate_classpath` or `missing_dependency` |
| `message` | the loader's line |

Refs: `mod_id` when the line names one (`incompatible`). Provided by `latest_log`.

```json
{ "kind": "minecraft.loader_problem", "attrs": { "kind": "duplicate_classpath", "message": "duplicate ASM classes found on classpath" } }
```

### `minecraft.mod_error`

| Attribute | Meaning |
|---|---|
| `message` | the loader's "failed to load mod" line |

Provided by `latest_log`.

```json
{ "kind": "minecraft.mod_error", "attrs": { "message": "Failed to load mod file examplemod-1.2.3.jar" } }
```

### `minecraft.mod_crash`

| Attribute | Meaning |
|---|---|
| `id` | the loader's mod id (also a ref) |
| `phase` | `load` or `entrypoint` |
| `order` | 0 for the first mod the file names, then 1, 2, ... |
| `name` | the display name the loader printed, when present |
| `message` | the loader line that named the mod |

Refs: `mod_id` (Mention). The shapes read are Forge's `Failure message: <name> (<id>) has failed to
load correctly`, `Caught exception from <name> (<id>)`, `Failed to create mod instance. ModID:
<id>`, `... encountered an error during ...`, and Fabric's `Could not execute entrypoint stage
'<stage>' due to errors, provided by '<id>'!`. Provided by `crash_report` and `latest_log`.

```json
{ "kind": "minecraft.mod_crash", "attrs": { "id": "examplemod", "phase": "load", "order": 0, "name": "Example Mod", "message": "Example Mod (examplemod) has failed to load correctly" },
  "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "mention" }] }
```

### `minecraft.jvm_problem`

| Attribute | Meaning |
|---|---|
| `kind` | `class_version`, `too_new`, `native_library`, `memory_startup`, `graphics` or `id_limit` |
| `message` | the exception or log line |
| `required_java` | for `class_version`, the Java the class-file version needs |

No refs: it names the JVM, the driver or the mod set, not one mod. `class_version` comes from
`UnsupportedClassVersionError`; `too_new` from the JDK-accessibility and removed-Nashorn shapes;
`native_library` from `UnsatisfiedLinkError`; `memory_startup` from heap-reservation and
native-memory messages; `graphics` from OpenGL context and pixel-format failures; `id_limit` from
`maximum id range exceeded`. Provided by `crash_report` and `latest_log`.

```json
{ "kind": "minecraft.jvm_problem", "attrs": { "kind": "class_version", "required_java": 17, "message": "class file version 61.0" } }
```

### `minecraft.file_problem`

| Attribute | Meaning |
|---|---|
| `kind` | `already_exists` |
| `path` | the file name, without its folders |
| `message` | the exception message |

Refs: `file` (Mention) of that name. Provided by `crash_report` and `latest_log`.

```json
{ "kind": "minecraft.file_problem", "attrs": { "kind": "already_exists", "path": "examplemod.toml.old", "message": "C:\\games\\config\\examplemod.toml.old" },
  "refs": [{ "kind": "file", "value": "examplemod.toml.old", "strength": "mention" }] }
```

### `minecraft.ticking_object`

| Attribute | Meaning |
|---|---|
| `kind` | `entity` or `block_entity` |
| `type` | the entity or block type, e.g. `examplemod:example_entity` |
| `name` | the entity's name, when the report prints one |
| `dimension` | the level dimension, when the report prints one |
| `x`, `y`, `z` | the exact or block coordinates, when the report prints them |

Refs: `mod_id` of the type's namespace when it is not `minecraft`, `forge` or `neoforge`.
Provided by `crash_report`, from `-- Entity being ticked --` and `-- Block entity being ticked --`.

```json
{ "kind": "minecraft.ticking_object", "attrs": { "kind": "entity", "type": "examplemod:example_entity", "x": 1.5, "y": 64, "z": -2.5 },
  "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "mention" }] }
```

### `minecraft.mod_conflict`

| Attribute | Meaning |
|---|---|
| `first` | the first mod the loader named |
| `second` | the mod it conflicts with |
| `message` | the loader's line |

Refs: `mod_id` for both names (Mention). Provided by `latest_log` (`Found conflicting mods: ...`).

```json
{ "kind": "minecraft.mod_conflict", "attrs": { "first": "examplemod", "second": "othermod", "message": "Found conflicting mods" },
  "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "mention" }, { "kind": "mod_id", "value": "othermod", "strength": "mention" }] }
```

### `minecraft.world_mod`

| Attribute | Meaning |
|---|---|
| `kind` | `missing` or `version` |
| `mod` | the mod id the world was saved with |
| `expected`, `current` | the saved and installed versions, for `version` |

Refs: `mod_id` (Mention). Provided by `latest_log`, from Forge's `This world was saved with mod
<id> ...` lines.

```json
{ "kind": "minecraft.world_mod", "attrs": { "kind": "missing", "mod": "examplemod" },
  "refs": [{ "kind": "mod_id", "value": "examplemod", "strength": "mention" }] }
```

### `minecraft.config_error`

| Attribute | Meaning |
|---|---|
| `message` | the loader's config failure line |

Provided by `latest_log`. Rules also match a `core.exception` message that names a broken config.

```json
{ "kind": "minecraft.config_error", "attrs": { "message": "Failed to load config file examplemod.toml" } }
```

### `minecraft.world_error`

| Attribute | Meaning |
|---|---|
| `message` | the chunk or world failure line |

Provided by `latest_log`.

```json
{ "kind": "minecraft.world_error", "attrs": { "message": "Failed to load chunk 12,34" } }
```

## Coverage keys

A rule about a kind the parser did not reach reads as "could not check", never false. A kind the
parser looked for and found none in is `checked`.

| Parser | `checked` | `absent` with a reason |
|---|---|---|
| `crash_report` | `minecraft.crash`, `core.exception`, `core.frame`, `core.process`, `core.system`, `minecraft.system`, `minecraft.loader`, `minecraft.mod`, `minecraft.section`, `minecraft.mod_crash`, `minecraft.mixin_failure`, `minecraft.ticking_object`, `minecraft.jvm_problem`, `minecraft.file_problem` | any of those with no data in this report |
| `crash_report` | | `core.log_line`, `core.dependency_problem`, `core.module`, `minecraft.loader_problem`, `minecraft.mod_conflict`, `minecraft.world_mod` are `not_applicable` |
| `latest_log` | `core.exception`, `core.frame`, `core.dependency_problem`, `minecraft.mixin_failure`, `minecraft.duplicate_mod`, `minecraft.loader_problem`, `minecraft.mod_error`, `minecraft.config_error`, `minecraft.world_error`, `minecraft.mod_crash`, `minecraft.mod_conflict`, `minecraft.world_mod`, `minecraft.jvm_problem`, `minecraft.file_problem`, `core.log_line` | `minecraft.loader`, `minecraft.system`, `core.process` when the log does not state them; `minecraft.mod` because `latest.log` does not list the whole mod set; `minecraft.ticking_object` is `not_applicable` |
| `hs_err` | `core.exception`, `core.system` | `core.frame` and `core.module` when there is no problematic frame; `core.process` without a pid; `minecraft.system` without Java details |

When a parser's read budget runs out, a kind it did not reach becomes `absent` with
"the file was not read to the end", and the output carries a `budget_spent` note. Agora's evaluator
also adds a trust note for `truncated` and `budget_spent`, which caps the answer at "likely".

## What a jar index would need to provide

The pack maps `mod_id` through an index named `mod_id` (exact) and `class` through an index named
`java_package` (prefix at a dot boundary). A jar index would need, per installed content item:

- its declared mod ids (`mod_id` → item), including ids from nested jars;
- the Java package prefixes its classes live in (`java_package` → item), so a frame's class can
  trace to a mod;
- the jar file names under `mods/` (the `file` ref map, with `archives`), so a Forge/NeoForge
  frame annotation can trace to its mod.

Until that index exists, a loose crash report still names a mod when its own mod list has the id
(above), and otherwise a frame resolves to "something Agora could not identify" and the engine does
not accuse it. The pack does not build the index.
