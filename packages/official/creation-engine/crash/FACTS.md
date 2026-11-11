# Creation Engine crash facts

This pack turns the two Creation Engine crash logs into facts, and holds the
first rules over them (see "Rules" at the end). Rule writers work from this
file and the fact vocabulary in `crates/agora-game-api/src/crash/facts.rs`.

The parsers are `parseCrashLogger` and `parseTrainwreck` in `main.js`. They use
only the `crash.readLines` call the contract allows, so they never see a path.
Addresses and form ids are **text** (`"0x7FF600001000"`, `"0x00012345"`),
never script numbers.

## Sources

| Source | Where | Filename | Session binding | Certainty |
|---|---|---|---|---|
| `crash_logger` | `Documents/My Games/Skyrim Special Edition/SKSE` | `crash-*.log` | `pid_in_facts`, else `time` (2 s before, 10 s after) | Verified |
| `trainwreck` | the same folder | `crash_*.log` | `time` (2 s before, 10 s after) | **Unverified** |

Crash Logger writes `crash-<YYYY-MM-DD-HH-MM-SS>.log` in the SKSE log folder by
default. That is from its own source: `src/main.cpp` (`SetupCrashLogPath` fails
when `logger::log_directory()` is missing) and `src/Settings.cpp`, whose default
is `Documents\my games\{Skyrim Special Edition|Skyrim VR}\SKSE\`. Its
`PROCESS INFO` block (1.16 and later) has `Process ID`, so a log binds to the
session by pid. Earlier versions have no process block and bind by time.

Trainwreck's folder and filename are **unverified**: no Trainwreck source is in
this workspace, and the samples' own text never states where the file lives.
The pack assumes the same SKSE folder and a `crash_` prefix. If a real
installation disagrees, change `sources[1]`; the parser itself does not depend
on the name.

Both sources have no `format`, so the content decides which parser runs. A file
that is not one of these logs is never claimed.

## Function names

Crash Logger usually prints only an Address Library id (`-> 19142+0x10`), while
other analyzers match crashes by function name. `crash/addrlib-names.json` and
`crash/addrlib-names.js` are generated from
[CommonLibSSE-NG](https://github.com/alandtse/CommonLibSSE-NG) at commit
`f49373ed922fde1dfbff0208f63a49bf8846c109` (GPL-3.0 with modding exceptions; see
its `COPYING.txt` and `EXCEPTIONS.md`). `scripts/gen_addrlib_names.py` reads the
`RELOCATION_ID(se, ae)`, `REL::RelocationID(se, ae)`, `REL::VariantID(se, ae, vr)`
and `REL::ID(id)` uses inside CommonLib functions and writes the two tables:
1432 SE ids and 1290 AE ids, with 42 ids named differently by more than one
definition (the first sorted path wins). It does not read the vtable/RTTI offset
tables, which name data rather than functions, and it cannot resolve ids passed
through a macro.

The table is a label, not evidence: a named frame says which function the game
was in, never which mod is at fault. An id the table does not name stays
unnamed, and `addrlib-vr` and `addrlib-unknown` frames are never named.

## Format bands

The parsers feature-detect every section; the bands are where the observed
sections or line shapes really change. They are not version gates.

### Crash Logger (CrashLoggerSSE)

| Band | Versions seen | What it adds or changes |
|---|---|---|
| A | 1.0–1.14 | `PROBABLE CALL STACK` with `[n] 0xADDR module+offset -> symbol+offset`. `MODULES`, `SKSE PLUGINS`, `PLUGINS`. No process block, no `Access Violation` line, no analysis sections. GPU memory first appears at 1.10. |
| B | 1.15 | Adds `Exception Flags`, `Number of Parameters`, `Access Violation: Tried to read/write/execute memory at …`. |
| C | 1.16 | Adds `CRASH TIME`, `PROCESS INFO` (pid, uptime, working folder, command line, executable hash), `Exception Thread ID`, virtual/process memory. |
| D | 1.17–1.19 | Adds `POSSIBLE RELEVANT OBJECTS` (one-line decoded objects). |
| E | 1.20–1.25 | Call stack becomes `CALL STACK ([P]robable / [S]tack scan):`, with `[n][P]` and `[n][S]` frames. Adds `ACCESS VIOLATION ANALYSIS` and `THREAD CONTEXT (HEURISTIC)`. |

Versions 1.9 and 1.21 have no sample, so they are covered by the band on either
side and by feature detection. A file whose first line lost its leading `S`
(`kyrim SSE v…`) is still read: the logger header and the exception identify
it, and the game line is matched without that first letter.

### Trainwreck

| Band | Versions seen | What it writes |
|---|---|---|
| T | 1.2.1–1.4.0 | `SkyrimSE`/`SkyrimVR`, then `trainwreck v…`. Four-space indentation. `PROBABLE CALL STACK` frames are `[n] 0xADDR module+offset symbol`, followed by instruction and `[source:line]` lines. `MODULES` has `Address`, `SHA` (SHA-512) and `Version`. `SCRIPT EXTENDER PLUGINS`. No process block and no load-order `PLUGINS`. |

## Plugin headers (pre-flight)

The `diagnostics` hook reads each enabled plugin's `HEDR` subrecord. Its first field is a
float32 header version: UESP's [TES4 format page](https://en.uesp.net/wiki/Skyrim_Mod:Mod_File_Format/TES4)
says 0.94 for older files and 1.7 for Skyrim SE, and xEdit's `gdHEDRVersion`
([wbDefinitionsTES5.pas, `dev-4.1.6`](https://github.com/TES5Edit/TES5Edit/blob/dev-4.1.6/Core/wbDefinitionsTES5.pas))
writes 1.71 for SSE. Game [1.6.1130](https://en.uesp.net/wiki/Skyrim:Special_Edition_Patch) raised
the ESL form-id range from 2048 to 4096 records, so a 1.71 header needs 1.6.1130 or later. A plugin
with 1.71 on an older game can stop the game while it loads its plugin list.

**Backported Extended ESL Support** backports that range to 1.5.97. The check treats
`Data/SKSE/Plugins/BackportedESLSupport.dll` as the exception and never reads its version. The DLL
name is the one the mod's own package installs (Nexus Mods, Skyrim Special Edition); the page
blocks automated fetches, so the name was confirmed from its published file listing rather than
read from source here.

## Facts

Every example below is invented for this file. `place` is a register name or a
stack offset; `depth` is 0 at a register or stack slot and grows for nested
decoded objects.

### `core.exception`

The unhandled exception, including a C++ exception block one level deeper.

| Attribute | Meaning |
|---|---|
| `class` | The quoted code (`EXCEPTION_ACCESS_VIOLATION`, `C06D007E`) or the C++ type. If the log omits the code but states an access violation, this is `EXCEPTION_ACCESS_VIOLATION`. |
| `category` | `access_violation`, `assert`, `oom`, `fast_fail` or `unknown`. |
| `message` | The C++ exception's `Info` text, when present. |
| `access` | `read`, `write` or `execute`, from the `Access Violation` or `In-Page Error` line. |
| `fault_address` | The address the faulting access touched, as text. |
| `near_null` | `true` when the fault address is below `0x10000`. |
| `pointer_class` | `null`, `near_null`, `non_canonical`, `kernel` or `user`, a documented guess from the fault address. |
| `depth` | 0 for the main exception, 1 for a C++ exception beside it. |

Refs: `module` (strength `exec_top`) when the log names the faulting module.

```json
{ "kind": "core.exception",
  "attrs": { "class": "EXCEPTION_ACCESS_VIOLATION", "category": "access_violation",
             "access": "read", "fault_address": "0x000000000010", "near_null": true,
             "pointer_class": "near_null", "depth": 0 },
  "refs": [{ "kind": "module", "value": "ExampleGame.exe", "strength": "exec_top" }] }
```

### `creation.exception_site`

Where the exception line says the fault happened. Separate from the first call
frame so a log with no call stack still carries the site.

| Attribute | Meaning |
|---|---|
| `address` | The exception address as text. |
| `module` | The module named after the address. |
| `offset` | The offset inside that module. |
| `symbol` | A Trainwreck function name after the offset. |
| `name` | The frame's function name, when the call stack names the same module offset (the log's own name, else the Address Library table). |
| `source_file`, `line` | A `file.cpp:NNN` the line gives. |
| `instruction` | The disassembled instruction, when present. |

```json
{ "kind": "creation.exception_site",
  "attrs": { "address": "0x7FF600001000", "module": "ExampleGame.dll",
             "offset": "0x001000", "instruction": "mov rax, [rcx]" } }
```

### `core.frame`

One call-stack frame, in log order. `index` 0 is the first/top frame.

| Attribute | Meaning |
|---|---|
| `index` | The frame number the log printed; 0 is first. |
| `kind` | `probable` or, from a 1.20+ `[S]` line, `scan` (weak). |
| `address` | The absolute address as text. |
| `rva` | The offset inside the module, as text. |
| `symbol_scheme` | `addrlib-ae`, `addrlib-se`, `addrlib-vr`, `addrlib-unknown` or `pdb`. |
| `symbol` | Address Library id, or the function name when the log gives one. |
| `symbol_offset` | The offset after `+` in the symbol, as a number. |
| `source_file`, `line` | A source location the log gives. |

Refs: `module` (strength `exec_top` for the first probable frame, `exec` for
later probable frames, `mention` for scanned frames).

```json
{ "kind": "core.frame",
  "attrs": { "index": 0, "kind": "probable", "address": "0x7FF600001000",
             "rva": "0x001000", "symbol_scheme": "addrlib-ae",
             "symbol": "11111", "symbol_offset": 16 },
  "refs": [{ "kind": "module", "value": "ExampleGame.dll", "strength": "exec_top" }] }
```

### `creation.frame_name`

The function name for a frame, and where it came from. The log's own name (a
Crash Logger arrow tail or a Trainwreck symbol) is kept with `source: "log"`;
an Address Library id the generated table names gets
`source: "address_library_names"`. An id the table does not name emits no fact.

```json
{ "kind": "creation.frame_name",
  "attrs": { "index": 0, "name": "ExampleNamespace::ExampleUpdate", "source": "log" } }
```

### `core.module`

A loaded module from `MODULES`. Crash Logger prints only the base; Trainwreck
prints a version when it has one.

| Attribute | Meaning |
|---|---|
| `version` | The module version, when the log shows it. |
| `base_address` | The load base, as text. |

Refs: `module`.

```json
{ "kind": "core.module", "attrs": { "base_address": "0x7FF600000000" },
  "refs": [{ "kind": "module", "value": "ExampleGame.dll", "strength": "mention" }] }
```

### `creation.module_hash`

Trainwreck's `SHA` line. The hash is SHA-512, so it is not put in
`core.module`'s `sha256` attribute.

```json
{ "kind": "creation.module_hash",
  "attrs": { "hash": "AAAA…", "algorithm": "sha512" },
  "refs": [{ "kind": "module", "value": "ExampleGame.dll", "strength": "mention" }] }
```

### `core.process`

The game process. The image and version come from the header, so this fact
exists even in versions with no `PROCESS INFO`.

| Attribute | Meaning |
|---|---|
| `image` | `SkyrimSE.exe` or `SkyrimVR.exe`, when the edition is known. |
| `image_version` | The game version from the header. |
| `pid` | `Process ID`. |
| `cwd` | `Working Directory`. |
| `command_line` | `Command Line`. |
| `uptime_s` | `Process Uptime`, in seconds. |
| `image_hash` | `Executable MD5`, when present. |

```json
{ "kind": "core.process",
  "attrs": { "pid": 9001, "image": "SkyrimSE.exe", "image_version": "1.6.1170",
             "cwd": "C:\\Games\\Invented Game", "uptime_s": 110.886 } }
```

### `core.system`

The `SYSTEM SPECS` summary.

| Attribute | Meaning |
|---|---|
| `os` | The `OS` line. |
| `gpu` | A list of the `GPU #n` lines. |
| `ram_free_mb` | Free physical memory, in MB (`total - used`). |
| `commit_pct` | Commit or page-file use, as a percentage. |
| `vram_pct` | GPU memory use, as a percentage. |

```json
{ "kind": "core.system",
  "attrs": { "os": "Microsoft Windows 11 Pro v10.0.26200",
             "gpu": ["Invented Graphics 9200"], "ram_free_mb": 7168,
             "commit_pct": 7.6, "vram_pct": 37.5 } }
```

### `creation.processor`

The `CPU` line and its core counts.

| Attribute | Meaning |
|---|---|
| `vendor` | The first word of the `CPU` line. |
| `name` | The rest of the `CPU` line. |
| `logical`, `physical`, `packages` | `CPU Cores`, when present. |

```json
{ "kind": "creation.processor",
  "attrs": { "vendor": "InventedVendor", "name": "Invented CPU 9200",
             "logical": 20, "physical": 12, "packages": 1 } }
```

### `creation.system_memory`

Memory in use and total, in MB, whichever lines the log has.

| Attribute | Meaning |
|---|---|
| `physical_used_mb`, `physical_total_mb` | `PHYSICAL MEMORY` or `RAM`. |
| `virtual_used_mb`, `virtual_total_mb` | `VIRTUAL MEMORY`. |
| `pagefile_used_mb`, `pagefile_total_mb` | `PAGE FILE`. |
| `gpu_used_mb`, `gpu_total_mb` | `GPU MEMORY`. |

```json
{ "kind": "creation.system_memory",
  "attrs": { "physical_used_mb": 9216, "physical_total_mb": 16384 } }
```

### `creation.access_violation`

The 1.20+ `ACCESS VIOLATION ANALYSIS` block.

| Attribute | Meaning |
|---|---|
| `instruction` | The disassembled instruction. |
| `base`, `index`, `scale`, `disp` | The decoded memory operand. |
| `base_value` | The base register's value, as text. |
| `computed_address` | The address the operand computes, as text. |
| `fault_address` | The address the log reported, as text. |
| `mismatch` | `true` when the log marks the two as a mismatch. |

```json
{ "kind": "creation.access_violation",
  "attrs": { "instruction": "call [rdi+0x10]", "base": "rdi", "index": "none",
             "scale": "0", "disp": "+0x10", "computed_address": "0x0000027EDCEFFF90",
             "fault_address": "0x000000000010", "mismatch": true } }
```

### `creation.script_extender_plugin`

A line under `SKSE PLUGINS` (Crash Logger) or `SCRIPT EXTENDER PLUGINS`
(Trainwreck).

| Attribute | Meaning |
|---|---|
| `version` | The version after the name, when shown. |

Refs: `module` (the plugin is a native module).

```json
{ "kind": "creation.script_extender_plugin", "attrs": { "version": "1.0.0" },
  "refs": [{ "kind": "module", "value": "ExampleBridge.dll", "strength": "mention" }] }
```

### `creation.plugin`

One entry in the load-order `PLUGINS` list. Trainwreck has no such list.

| Attribute | Meaning |
|---|---|
| `index` | The index as printed, uppercased: `00`, `A`, `FE:000`. |
| `light` | `true` for a `[FE:…]` entry. |

Refs: `plugin`.

```json
{ "kind": "creation.plugin", "attrs": { "index": "FE:000", "light": true },
  "refs": [{ "kind": "plugin", "value": "ExampleLight.esl", "strength": "object" }] }
```

### `creation.load_order_summary`

The `Light: N Regular: N Total: N` line before the list.

```json
{ "kind": "creation.load_order_summary",
  "attrs": { "light": 1, "regular": 3, "total": 4 } }
```

### `creation.load_order_error`

A line in `PLUGINS` that is not a summary or a plugin entry, such as the
logger's own `ERROR`.

```json
{ "kind": "creation.load_order_error", "attrs": { "message": "ERROR" } }
```

### `creation.object`

A decoded register, stack slot, relevant object, or a nested object inside one.
The type and place are always kept; the named contents are refs.

| Attribute | Meaning |
|---|---|
| `type` | The C++ type the logger printed. A nested object's `type` is the field that introduced it (for example `ParentCell`). |
| `place` | The register or `RSP+n` the root object came from; nested objects keep the same place. |
| `value` | The root object's address or value, as text. |
| `field` | For a nested object, the field label that introduced it. |
| `depth` | 0 for a root object, 1 or more nested. |
| `name` | `Name`, `Full Name`, `Object Reference`, `GetFullName` or a nested field's value. |
| `form_id` | The form id, as text. |
| `editor_id` | `EditorID`, without quotes. |
| `form_type` | `FormType`. |
| `flags` | The `Flags` line. |
| `rtti` | `RTTIName`. |

Refs, all strength `object` unless noted:

| Ref kind | Comes from |
|---|---|
| `plugin` | `File:` (a plugin name), and the chunks of `Modified by` (see below). |
| `form` | `FormID` / `Form ID`, and the bracketed ids in a compact one-line object. |
| `file` | Any asset path: `Name`, `Skeleton Name`, `Animation Name`, `fxpFilename`, `inputFilePath`, `filePath`, `streamName`, `Path`, `Model`, or any field whose value is a path with a known asset extension. |
| `script` | `Script` or `Script Name`. |

```json
{ "kind": "creation.object",
  "attrs": { "type": "ExampleForm*", "place": "RDX", "value": "0x1F000000000",
             "depth": 0, "form_id": "0x00012345", "form_type": "FormList (91)",
             "editor_id": "ExampleEditorId", "flags": "0x00000009 kInitialized" },
  "refs": [{ "kind": "plugin", "value": "ExampleBase.esm", "strength": "object" },
           { "kind": "form", "value": "0x00012345", "strength": "object" },
           { "kind": "file", "value": "meshes\\example\\invented.nif", "strength": "object" }] }
```

A nested object is its own fact, with the same `place` and a larger `depth`:

```json
{ "kind": "creation.object",
  "attrs": { "type": "ParentCell", "field": "ParentCell", "place": "RSP+0",
             "depth": 1, "form_id": "0x0000003C", "form_type": "Cell (60)" },
  "refs": [{ "kind": "plugin", "value": "ExampleBase.esm", "strength": "object" }] }
```

### `creation.plugin_chain`

The `Modified by` chain, split out and chunked so a long chain never exceeds
`max_refs_per_fact`. `start` is the index of the first plugin in this chunk.

| Attribute | Meaning |
|---|---|
| `place`, `depth` | The object the chain was found under. |
| `field` | Always `modified_by`. |
| `start` | The chain index of the first ref in this fact. |
| `form_id` | The owning object's form id, when it has one. |

Refs: `plugin`, in chain order.

```json
{ "kind": "creation.plugin_chain",
  "attrs": { "place": "RDX", "depth": 0, "field": "modified_by", "start": 0 },
  "refs": [{ "kind": "plugin", "value": "ExamplePatch.esp", "strength": "object" },
           { "kind": "plugin", "value": "ExampleBase.esm", "strength": "object" }] }
```

### `creation.code_pointer`

A scalar (usually `void*`) whose value points into a module; an address, not an
object.

```json
{ "kind": "creation.code_pointer",
  "attrs": { "place": "RAX", "value": "0x7FF700001000", "module_offset": "0x001000" },
  "refs": [{ "kind": "module", "value": "ExampleGame.exe", "strength": "mention" }] }
```

### `creation.model_request`

A line from a model-loading section, when the log has one. The paths are often
the best clue to what the failing thread was loading.

| Attribute | Meaning |
|---|---|
| `place` | An `RSP+n` the line names. |
| `path` | A path the line names; also a `file` ref. |
| `text` | The line, when it names no path. |

```json
{ "kind": "creation.model_request",
  "attrs": { "place": "RSP+0x20", "path": "meshes\\example\\invented.nif" },
  "refs": [{ "kind": "file", "value": "meshes\\example\\invented.nif", "strength": "object" }] }
```

## Inspectors

An inspector opens one file a rule points at. `skse_plugin_declaration` accepts
`*.dll` and reads only the PE headers, the export table, and the
`SKSEPlugin_Version` data. It is `inspectSksePluginDeclaration` in `main.js`.
It sees the file by id through `crash.readBytes`; the rule's ref is resolved to
the deployed file before the script runs, and the script never sees a path.

### `creation.skse_plugin_declaration`

| Attribute | Meaning |
|---|---|
| `data_version` | The struct's first field. This reader knows `1`. |
| `plugin_version` | The plugin's own version, decoded to `1.2.3` or `1.2.3.4`. |
| `name` | The name the plugin declares, when it is non-empty. |
| `address_independent` | `versionIndependence` bit `0x1`: uses the post-AE address library. |
| `signature_scanning` | `versionIndependence` bit `0x2`: finds addresses by signature. |
| `structs_post_629` | `versionIndependence` bit `0x4`: built for the structs that changed in 1.6.629. |
| `uses_no_structs` | `versionIndependenceEx` bit `0x1`: uses none of the structs that changed. |
| `compatible_runtimes` | Each non-zero packed runtime version, decoded to `1.5.97` style. |
| `minimum_xse_version` | The declared minimum SKSE version, decoded the same way. |

The fact also has a `module` ref naming the DLL (a mention).

### PE and struct offsets

Source: `.spike/CommonLibSSE-NG/include/SKSE/Interfaces.h` (commit
`f49373ed`, GPL-3 with modding exceptions) and
`.spike/CommonLibSSE-NG/include/REL/Version.h`.

| Field | Offset | Source |
|---|---|---|
| DOS header magic / `e_lfanew` | `0x00` / `0x3C` | standard PE layout |
| PE signature | `e_lfanew` | standard PE layout |
| number of sections | COFF + `0x06` | standard PE layout |
| size of optional header | COFF + `0x14` | standard PE layout |
| export directory RVA/size | optional header data directory 0 (`0x70` PE32+, `0x60` PE32) | standard PE layout |
| section table | `e_lfanew + 24 + SizeOfOptionalHeader` | standard PE layout |
| export directory fields | `NameCount` at `0x18`, `AddressOfFunctions` at `0x1C`, `AddressOfNames` at `0x20`, `AddressOfNameOrdinals` at `0x24` | standard PE layout |
| `dataVersion` | `0x000` | Interfaces.h:613 |
| `pluginVersion` | `0x004` | Interfaces.h:614 |
| `pluginName[256]` | `0x008` | Interfaces.h:615 |
| `author[256]` | `0x108` | Interfaces.h:616 |
| `supportEmail[252]` | `0x208` | Interfaces.h:617 |
| `versionIndependenceEx` | `0x304` | Interfaces.h:618 |
| `versionIndependence` | `0x308` | Interfaces.h:619 |
| `compatibleVersions[16]` | `0x30C` | Interfaces.h:620 |
| `xseMinimum` | `0x34C` | Interfaces.h:621 |
| struct size | `0x350` | Interfaces.h:622 |

Flag values: `AddressLibraryPostAE = 1 << 0`, `Signatures = 1 << 1`,
`StructsPost629 = 1 << 2` (Interfaces.h:546-548); `NoStructUse = 1 << 0`,
`AddressLibraryV5 = 1 << 1` (Interfaces.h:553-554). Version packing is
`major << 24 | minor << 16 | patch << 4 | build` (Version.h:94-107), so `1.6.1170`
is `[1, 6, 1170, 0]`.

### What "supported" means

The header says the struct layouts changed in **1.6.629**, not 1.6.1130:
"In AE 1.6.629, however, the layout of some structs changed. If this is flagged
as independent, then SKSE will let your plugin work with runtimes before and
after this change" (Interfaces.h:786-796). The matching bit is
`kVersionIndependent_StructsPost629` (1.6.629); `kVersionIndependentEx_NoStructUse`
is the other way a plugin says it avoids the changed structs (Interfaces.h:553).

The inspector applies that reading:

- **Version-independent** (`address_independent` or `signature_scanning`): any
  runtime is accepted, except a struct mismatch. On or after 1.6.629 it needs
  `uses_no_structs` or `structs_post_629`; before 1.6.629 it must not set
  `structs_post_629`.
- **Not version-independent**: the running version must appear in
  `compatible_runtimes`. If that list is empty, the answer is `unknown`, not
  `fail`, because the file does not declare enough to judge.

That is derived from the header's documented meaning, not from SKSE's own
loader source (not in this checkout). The `pass`/`fail` split only uses what the
DLL declares; a `fail` still never says "certainly".

## Coverage keys

`coverage` says what each parser looked for. A rule about a kind with no fact
and no `checked` entry reads as **could not check**, never as "not there".

| Kind | What `checked` means | When it is `absent` | When `not_applicable` |
|---|---|---|---|
| `core.exception` | The exception block was read, or an access-violation line supplied the exception. | No unhandled-exception line and no access violation. | Never |
| `core.frame` | A call-stack section was present. | No call-stack section. | Never |
| `core.module` | `MODULES` was present. | No `MODULES`. | Never |
| `core.process` | Always: the header names an image, and `PROCESS INFO` is read when present. | Never (a header-only process fact is still a fact). | Never |
| `core.system` | `SYSTEM SPECS` was present. | No `SYSTEM SPECS`. | Never |
| `creation.game` | The header was read. | Never | Never |
| `creation.processor` | `SYSTEM SPECS` was present. | No `SYSTEM SPECS`. | Never |
| `creation.system_memory` | `SYSTEM SPECS` was present. | No `SYSTEM SPECS`. | Never |
| `creation.exception_site` | An exception line was read. | No exception line. | Never |
| `creation.access_violation` | `ACCESS VIOLATION ANALYSIS` was present. | The log has no such section (all versions before 1.20, and Trainwreck). | Never |
| `creation.object` | A registers, stack, relevant-object or thread-context section was present. | None of those sections. | Never |
| `creation.plugin_chain` | Same as `creation.object`. | Same as `creation.object`. | Never |
| `creation.code_pointer` | Same as `creation.object`. | Same as `creation.object`. | Never |
| `creation.frame_name` | A call-stack section was present. | No call-stack section. | Never |
| `creation.module_hash` | `MODULES` was present. | No `MODULES`. | Never |
| `creation.script_extender_plugin` | The script-extender plugin section was present. | That section is missing. | Never |
| `creation.plugin` | `PLUGINS` was present and readable. | No `PLUGINS`, or the logger wrote `ERROR` instead. | Trainwreck |
| `creation.load_order_summary` | `PLUGINS` was present. | No `PLUGINS`. | Trainwreck |
| `creation.load_order_error` | `PLUGINS` was present. | No `PLUGINS`. | Trainwreck |
| `creation.model_request` | A model-loading section was present. | No model-loading section. | Never |
| `core.log_line` | — | — | Always: these parsers do not emit raw log lines. |
| `core.load_verdict` | — | — | Always: a crash log has no load verdict. |
| `core.dependency_problem` | — | — | Always: a crash log has no dependency verdict. |

## Roles and what is left out

The pack marks as `system` the Windows modules and graphics drivers that must
not be accused (`ntdll`, `KERNEL32`, `KERNELBASE`, `user32`, `win32u`, `gdi32`,
`gdi32full`, `ucrtbase`, `msvcrt`, `advapi32`, `sechost`, `rpcrt4`, `combase`,
`ole32`, `dxgi`, `d3d11`, `nvldumdx`, `atiumd64`, `atidxx64`, `amdxx64`,
`amdxc64`, `nvwgf2umx`, and the C++ runtime `vcruntime140`, `vcruntime140_1`,
`msvcp140`). `SkyrimSE.exe` and `SkyrimVR.exe` are `platform`, and so are the
base game's plugins (`Skyrim.esm`, `Update.esm`, the three DLC masters,
`_ResourcePack.esl`). The SKSE loader, the known SKSE DLL names and the
Unofficial Skyrim Special Edition Patch are `framework`. `CrashLogger.dll`,
`trainwreck.dll`, `hdtSMP64.dll`, `EngineFixes.dll` and `tbbmalloc.dll` are
`hook_framework`: they sit on stacks they did not cause.

Left out on purpose: every other module. A short, sure list is better than a
long guess. The pack does not name versioned SKSE DLLs generically (the role
matcher has no wildcard), so only the common exact names are listed; a new SKSE
version needs a new role entry. The Address Library is a `.bin` file, not a
module on the stack, so it has no role.

## Decisions and unverified points

- **Everything lives in this pack, not in `agora.skyrim-se`.** The contract runs
  each pack's parsers by id inside that same pack, so a second plugin's pack
  cannot call a parser from the plugin it depends on. Putting the Skyrim sources
  and roles in this pack is the only way to reuse these parsers today. The pack
  is restricted to `games: ["skyrim-se"]` because its sources are Skyrim's.
- **Trainwreck's folder and filename are unverified** (see Sources). Its line
  shapes are taken from its own logs.
- **`pointer_class` is a documented guess** from the address bits, not a fact
  the logger states.
- **`core.process` exists even without `PROCESS INFO`**, because the header
  names the image and version. The pid is simply absent.
- **A line longer than the 8 KiB read limit is cut.** The parser keeps what it
  read, emits `truncated`, and never throws the log away.

## Rules

Measured on the benchmark's dev set only (129 Skyrim logs with a confirmed
cause); the test set is never read. One family alone tops out at `possible`
(weight 7 or less); two independent families agreeing can reach higher.

| Rule | Family | Weight | Why this weight |
|---|---|---|---|
| `module_near_top_of_stack` | execution | 7 | A non-system DLL in the first three probable frames was the cause about 40% of the time before the hook-framework roles, more after. |
| `module_lower_on_stack` | execution | 3 | Below the floor on its own; only adds to other evidence. |
| `plugin_object_in_register` | object reference | 6 | The plugin of a record in a register was the cause 48% of the time. |
| `plugin_object_on_stack` | object reference | 3 | Stack slots hold old data; never names anyone alone. |
| `file_in_register` | object reference | 5 | A file path in a register names a broken mesh or texture in a few cases. |
| `out_of_memory` | finding, environment | `likely` | Only an out-of-memory exception (including `std::bad_alloc`); caps any mod at `possible`. |
| `dll_dependency_failed_to_load` | finding, environment | `possible` | `C06D007E`: a DLL could not load a library it needs. |
| `content_catalog_format_mismatch` | finding, environment | `likely` | `std::invalid_argument` "stoull" while an `InstalledContent` object is live: `ContentCatalog.txt` written by another game build (3 of 3 dev rows). From Tullius's signatures. |
| `missing_masters_known_site` | finding, environment | `likely` | The `0198090` / `5E1F22` crash site, gated to the exact builds it belongs to (8 of 9 dev rows were missing masters or ESL support). From Phostwood. |
| `cot_divide_by_zero` | known signature | 7 | Integer divide-by-zero with Climates of Tamriel loaded (2 of 2). From Phostwood. |
| `d3d11_graphics_injector_fault` | finding, environment | `possible` | The fault is in `d3d11.dll`, where ENB or ReShade hook in. From Phostwood and Tullius. |
| `win24h2_upscaler_fault` | finding, environment | `possible` | `KERNELBASE.dll` fault with the Skyrim Upscaler on the stack. From Phostwood. |
| `dragons_eye_minimap_fault` | known signature | 6 | The minimap loaded with camera or image-space objects live. From Phostwood. |
| `skyclimb_outdated` | known signature | 6 | SkyClimb older than 1.1.2 on game 1.6.1130 or later (8 right, 2 wrong on dev; both wrong rows did have an old SkyClimb). From Phostwood's version table. |
| `hair_mesh_in_use` / `hair_mesh_with_smp` | finding, mechanism | `possible` | A `HairMale*`/`HairFemale*` mesh in use (a `BSDynamicTriShape` name). The only dev case was an HDT-SMP version mismatch; Phostwood flags it on the owner's own logs. Names no plugin: vanilla hair records come from `Skyrim.esm`. From Phostwood. |
| `engine_fixes_outdated` | context | - | Engine Fixes below 7.0.20 on 1.5.97 or 1.6.1170. From Phostwood. |
| `shadow_scene_crash_engine_fixes_can_prevent` | finding, environment | `possible` | A `ShadowSceneNode` in use with Engine Fixes below 7.0.18, which fixes that crash. From Phostwood. |
| `crash_logger_outdated` | context | - | A Crash Logger log older than 1.20.1, which writes more detail. From Phostwood. |
| `skse_plugin_built_for_another_game_version` | verified asset + known signature | 16 + 8 | The inspector opened a listed plugin's DLL and its declared versions exclude the running game. Only an instance has the file, so the rule requires `core.instance.game`. |
| `stack_module_built_for_another_game_version` | verified asset + execution | 16 + 8 | The same check for a DLL on the crash stack (actionable frames 0-16). The inspector reads the DLL's own declaration; the rule says only that it does not support this game. |

What the dev set taught, kept here so nobody re-learns it:

- **Memory nearly full is not evidence.** The 14 dev logs with the most RAM in
  use (92-98%) all had a mod as the confirmed cause, so there is no rule on it.
- **The last plugin in a `Modified by` chain is weak** (right 14% of the time as
  a top pick), so there is no rule on it yet, despite the common advice.
- **No save signature is visible**: save-caused crashes die in the game's own
  code, named only by Address Library ids. A save rule needs the ids of the
  save-loading functions, not guesses.
- About a third of version-mismatch causes appear only in the SKSE plugin list.
  They need the DLL's own declared game versions (an inspector), not a log rule.
- **Phostwood's DLL version table is not adopted as a whole.** It is a 9-entry
  snapshot ending at game 1.6.1129. As suspects it gave 14 right and 17 wrong on
  dev; as a note it touched 6 unrelated logs. Only SkyClimb, its most common
  entry, is a rule. The real answer is the DLL inspector, which reads each
  plugin's own declared versions and cannot go stale.
- Also measured and dropped: Engine Fixes being absent (0 of 12), a plugin count
  near 254 (0 of 3), a near-null access violation (fires on every kind of
  cause), the save/load manager on the stack (4 of 6 were mods).
- Rule conditions: `any` stops at its first true branch, so one rule over a list
  names only the first match. Write one rule per suspect when each should be
  named.
