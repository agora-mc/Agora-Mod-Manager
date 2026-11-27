# Agora CLI reference

### The Agora CLI is an advanced tool for AI agents and commandline familiar users. We do not recommend it for most users.

The `agora` binary exposes the same core instance, catalog, health, launch, snapshot, runtime, install, and community-plugin services used by the desktop application. It is intended for advanced users, support diagnostics, scripting, and AI assistant apps.

The CLI can modify the same data used by the desktop application. Read the safety section before experimenting.

## Build or install

The desktop app includes the CLI: `agora.exe` in the install folder on Windows and beside `Agora Mod Manager.exe` in the portable ZIP, `Agora Mod Manager.app/Contents/MacOS/agora` on macOS, and `/usr/bin/agora` from the Linux packages. Settings → Services → AI assistant shows its exact path. It is not added to `PATH`, so call it by that path or add it yourself. The standalone `agora-cli-*` archives on the release page are for machines without the desktop app.

To build it from source, from the repository root:

```bash
cargo build -p agora-cli
```

The development binary is written under Cargo's target directory:

```text
target/debug/agora
```

To install it into Cargo's binary directory:

```bash
cargo install --path crates/agora
```

Confirm the installed interface:

```bash
agora --version
agora --help
```

## Safety first

Before running a modifying command, print the paths Agora resolved:

```bash
agora paths
```

For experiments, use an isolated data root:

```bash
agora --data-dir ./tmp/agora-cli-test paths
```

Keep these rules in mind:

- The default data root may be shared with the desktop application.
- The desktop app and the CLI can run at the same time. An instance one of them is changing is locked, and the other waits up to 30 seconds and then names who holds it; the desktop app reloads an instance the CLI changed. Avoid editing the same instance setting from both at once: the last save wins.
- Use `--dry-run` on install, remove, and update operations before executing a complex plan.
- Create a snapshot before manual or high-risk changes.
- Use disposable instances when learning destructive commands.
- `instance delete` moves the instance to Recently deleted, where it can be restored for 30 days (or the time set in Settings > General > Storage); then it goes to the OS trash. `snapshots restore`, `snapshots delete`, `lockfile import`, and some install conflict choices can replace or remove files.
- Never place access tokens or account codes in command history, log files, or bug reports.
- `--data-dir` isolates files under the Agora data root. On Windows the Microsoft credential is a file inside that root (DPAPI-protected for the normal root, device-key encrypted for a relocated one), so a disposable root holds its own sign-in and neither the desktop app nor an installed copy is changed. On macOS and Linux the credential is in the operating-system store: `auth status`, `auth login`, and `auth logout` then inspect or change the same entry the desktop app uses, whatever root is selected. A portable copy (a `portable.txt` beside the executable, with no `AGORA_DATA_DIR` override) never uses the operating-system store; it keeps its credentials encrypted in its own data folder, which is obfuscation rather than OS protection because the key travels with them.

## Global syntax

```text
agora [GLOBAL OPTIONS] <COMMAND> [COMMAND OPTIONS]
```

Global options:

| Option | Purpose |
| --- | --- |
| `--data-dir <PATH>` | Override the Agora data root. Without it the CLI resolves the root exactly as the desktop app does, so `AGORA_DATA_DIR` and a `portable.txt` marker beside the executable apply to both |
| `--json` | Shorthand for `--output json` |
| `--output <human|json>` | Select output format |
| `--registry-repo <OWNER/REPO>` | Override the catalog repository for development or testing |
| `--log-file <PATH>` | Append CLI diagnostics to a chosen file |
| `--fresh-database` | Keep a `local_state.db` written by a newer Agora aside and start with a fresh database. Without it, startup refuses the newer file and changes nothing |
| `--migrate-without-backup` | Migrate even when the pre-migration database backup cannot be written. Without it, startup stops and leaves the database unchanged |
| `--help` | Show help |
| `--version` | Show the CLI version |

Human output is the default. Prefer `--output json` for scripts.

NDJSON is not a public output format. Commands return one ordinary JSON value when JSON output is selected. Line-delimited JSON-RPC is available only through `agora mcp serve --stdio`.

Progress and diagnostic messages may be written to standard error while command results are written to standard output. Scripts should capture the streams separately.

## A ten-minute tour

Inspect the environment:

```bash
agora paths
agora registry status
agora list-instances
```

Synchronize the signed catalog:

```bash
agora registry sync
```

Create a disposable instance:

```bash
agora instance create "CLI Test" \
  --mc-version 1.21.1 \
  --loader fabric \
  --loader-version 0.16.10
```

Use `list-instances` to obtain the generated instance ID:

```bash
agora list-instances
```

Search the curated catalog:

```bash
agora mod search sodium --content-type mod --mc-version 1.21.1
```

Resolve an install without changing files:

```bash
agora mod install sodium <INSTANCE_ID> --dry-run
```

Execute only after reviewing the plan:

```bash
agora mod install sodium <INSTANCE_ID>
```

A download whose bytes do not match a curator pin, or the hash Agora recorded when the same release file was installed before, is refused with `ERR_HASH_CONFIRMATION_REQUIRED`. The message names the file, the expected and downloaded SHA-256, and the source of the expectation. Check the file yourself, then install it anyway with `--install-anyway`. A mismatch with the hash the source published cannot be overridden. A file whose source published no checksum installs with a notice, and is recorded as not verified.

```bash
agora mod install sodium <INSTANCE_ID> --install-anyway
```

The same flag works on `agora mod update`.

A required dependency that Agora cannot find, or a mod whose own requirements it cannot read, blocks the install with `ERR_MISSING_DEPENDENCY`, `ERR_REQUIRED_DEPENDENCY` or `ERR_UNCHECKED_REQUIREMENTS`. Add what is missing, or install without it. This is separate from `--install-anyway`, and the same choice as "Install Anyway" in the desktop app. The install names what it went past as a warning, and the mod may not start until you add it.

```bash
agora mod install sodium <INSTANCE_ID> --allow-missing-dependencies
```

Check health and launch:

```bash
agora health <INSTANCE_ID>
agora launch <INSTANCE_ID> --timings
```

Inspect and install a community plugin:

```bash
agora plugin list
agora plugin preview package ./example-plugin.zip
agora plugin install package ./example-plugin.zip
```

## Command map

### Discovery and paths

| Command | Purpose |
| --- | --- |
| `agora paths` | Print resolved application, database, cache, runtime, and instance paths |
| `agora doctor` | Report the local database's path, schema version, integrity, backups, and files kept aside after recovery, plus whether the saved account list needs repair, and what recovery would do about every unresolved crash journal. It reads only — nothing is migrated, copied, renamed or deleted, and it writes no log — and it works even when startup would refuse the database. A recovery journal that needs a hand is a problem, so it exits `10` |
| `agora list-instances` | List local instances |
| `agora get-instance <ID>` | Print one instance |
| `agora inventory <INSTANCE>` | Inspect installed content |
| `agora health <INSTANCE>` | Run the local health scanner |

### The local database safety net

Every command that needs launcher state opens `local_state.db` under the data root. Before it does anything else, startup:

- runs `PRAGMA quick_check` (a few milliseconds on a realistic database) and treats a damaged file as damaged rather than as empty;
- moves a corrupt file aside, never deleting it, and restores the newest `backups/` copy that passes the check; when no backup passes, it starts with a fresh database and says where the damaged file is;
- refuses a database written by a newer Agora unless `--fresh-database` is given, keeping the newer file beside the database;
- before any pending migration, copies the database with `VACUUM INTO` to `backups/local_state-<time>-v<version>.db`. The newest three backups are kept; older ones are deleted after a new backup is written. If the copy cannot be written, startup stops unless `--migrate-without-backup` is given;
- applies migrations one step at a time, each in its own transaction that also records the step's version, so a failed step leaves the last successful version recorded and the database usable;
- runs the one startup recovery pass over every crash journal (`agora_core::startup_recovery::run_at_startup`): interrupted tool swaps are put back, user-file sessions of runs that are no longer alive have their files restored, half-done Recently deleted deletes and restores are finished or called off, and each game package's own journal (Minecraft's launcher-import jobs) is recovered through the game registry. Every result is printed: what recovery did on stdout's warning channel, and a `Warning:` line for anything it left alone — a journal that needs a hand, or one whose lock another running Agora process (the desktop app, another CLI) still holds. Recovery never waits on such a lock and never touches that journal; it is reported and left for the live operation. The desktop and the MCP listener run the same pass at their own startup and report the same lines their own way.

`agora doctor` reports all of this without changing anything, including files kept aside as `local_state.db.corrupt-*` and `local_state.db.newer-v*`. It also runs the same account-list check as `agora auth status` and reports a corrupt or quarantined account index, naming `agora auth repair`; that is a local-state problem too, so it exits `10`.

### The recovery report

`doctor` also runs before startup recovery has had a chance to act, which makes it the one place a recovery that blocks a start can be explained. So it explains it: one entry per unresolved crash journal, with its kind, the resource it covers, where the journal is, its state, and the decision recovery would take.

| Decision | What it means |
|---|---|
| `nothingToDo` | The journal is already settled; nothing is waiting |
| `wouldUndo` | The next start would undo the operation: put back what it replaced |
| `wouldFinish` | The next start would finish the operation: roll forward through its cleanup |
| `busy` | Another Agora holds the journal's lock, or its recorded writer is still running, so nothing would be done — report and retry at the next start |
| `needsHand` | It cannot be decided safely; the reason says why, and the command that resolves or overrides it |
| `undecided` | This build does not decide that kind yet; the next start settles it with that kind's own rules |

The kinds are `plugin-install` (a plugin package being installed, updated, rolled back or removed), `instance-import` (an instance being imported from another launcher), `tool-swap` (a tool running on a real install's `Data` folder), `user-files` (game files swapped in for a run), `database-recovery` (a damaged database moved aside with its restore unfinished), `recently-deleted` (a delete or a restore into, or out of, Recently deleted) and `snapshot-object-migration` (the old `snapshot-objects` store being moved into the content store). Plugin installs and imports live in the local database; the rest are markers or files under the data root.

Doctor reports `snapshot-object-migration` as `wouldFinish` while the journal is `running` or when the journal is `done` and the old store still has regular files with object names. A start resumes or reopens the migration when it can take the store lock. Doctor reads the journal and lists the old store without taking the lock, hashing objects, or writing files. An unusable journal is `needsHand` with `agora games content migrate --restart`. If the old store contains a link that blocks migration, doctor names the path and tells you to remove or replace it before running `agora games content migrate`. Other refusals give the reason and leave the command empty.

Launcher imports are reported with their facts — which instance, which phase, which staging and instance folders — and with `undecided`: their recovery asks more questions than a journal row answers (the instance's runtime lease, the quarantine folder a failed update left, which marker each folder carries), so `doctor` does not repeat those rules. The next start settles it; a journal that keeps appearing is stuck, and nothing has been changed.

Recently deleted works the same way, because it is not on the journal envelope either. Each entry is listed with its id, the instance it covers, and whether `entry.json` and `restoring.json` are there and readable (with the read error when one is not). Its instance's lock is asked about first, so a running instance makes the entry `busy`. An entry with a `restoring.json` is `undecided` — a restore was started, and the next start settles it with that kind's own rules. A plain kept entry whose record reads is `nothingToDo`, with `agora games instance restore <entry>` as the command that brings it back; a kept entry whose record cannot be read is `needsHand` with the error; a folder holding neither file is `nothingToDo` with a note saying so.

The report is read-only, all the way through: no operation's lock is taken, no file is written, renamed or deleted, no stale lease is cleared, and the database is opened read-only — no migration, no busy-check, no journal-mode switch. `doctor` runs before the CLI log is opened, so it writes no log either. To tell a held lock from a free one, the lock file is briefly try-locked and released at once; that never disturbs a live holder, and a recovery that collides with the probe reports `busy` and retries at the next start. A journal whose writer may still be running is `busy`, and so is a liveness or lock answer of "could not tell" — never "would act".

`busy` never carries a command: the operation it belongs to is still going, or may be, and the next start retries it. An entry that names no command prints none: a `recently-deleted` folder with nothing in it, or a plugin journal whose ownership marker cannot be found (no Agora command overrides that yet — the reason says what to check). Every other entry names the command that resolves or overrides it: `agora plugin recover <id> --yes` for an interrupted plugin package, `agora plugin recover <id> --keep-aside --yes` for a stash no database knows about, and `agora games instance restore <entry>` for an entry held in Recently deleted.

It fails closed. A journal that cannot be read — malformed, a folder where a journal file belongs, a row this build cannot parse — is listed as itself, with its path and the error, never skipped, and one bad row does not hide the others. A kind whose journals cannot be searched at all — a database this build does not know (a newer schema), a file that cannot be opened, a folder that cannot be listed — is said so in `notSearched` with the reason and `blocking: true`, and errors stay per kind: a plugin-install problem never hides the import rows, or the other way round. A data root with no database yet is `blocking: false` for the two kinds that live in it (nothing has been written, so nothing is hidden), and the journals an earlier build left as files are listed anyway. `notes` carries the plain facts. None of that is ever reported as "no journals".

`--json` carries the same entries, with stable field names:

```json
"recovery": {
  "entries": [
    {
      "kind": "plugin-install",
      "resource": "acme.one",
      "path": "…/local_state.db",
      "state": "extracted",
      "outcome": "wouldUndo",
      "reason": "The operation had not committed (state \"extracted\"), so recovery would put the previous package back.",
      "command": "agora plugin recover acme.one --yes"
    }
  ],
  "notSearched": [{ "kind": "plugin-install", "reason": "…", "blocking": false }],
  "notes": ["No local database yet, …"]
}
```

A journal that needs a hand, or a kind that could not be searched, is a problem with the local state like any other: `doctor` exits `10` and the report names it. A data root with no database yet is not.

### Games

Lists the game installs Steam, GOG, Epic and the Microsoft Store know about, each
classified as a base game, add-on or tool (MASTER_SPEC §26.3), and manages pinned bases
(MASTER_SPEC §26.4). `--json` is supported on discovery, listing, and all base commands.

| Command | What it does |
|---|---|
| `agora games discover` | List game installs across supported stores |
| `agora games list` | List supported games and their identified installs |
| `agora games folder add <path>` | Add a game folder the store scan did not find (MASTER_SPEC §26.3), such as a copy on another drive or a portable install. The folder must exist, must not be a store install or inside one, must not already be listed, and must hold the executable of exactly one game Agora knows; otherwise it is refused and the message says why. The folder is then searched by `games discover` and `games list` |
| `agora games folder remove <path>` | Stop searching a folder added with `games folder add`. Its files are not touched; a folder that is gone from disk can still be removed |
| `agora games folder list [--json]` | List the folders you added, and whether each is still on disk. A folder that is gone is listed, and is not searched |
| `agora games catalog list <game> [--json]` | List the curated catalog entries for one game other than Minecraft, such as `skyrim-se`, with their stores, game versions and frameworks (MASTER_SPEC §26.8) |
| `agora games catalog install <instance> <item> [--file <PATH>] [--install-anyway] [--allow-missing-frameworks] [--dry-run] [--json]` | Install a curated catalog entry into a game instance (MASTER_SPEC §26.8). The entry's compatibility entry must match the instance's store and version, and its required frameworks must be installed at `min_version` or newer (an unreadable framework version is a warning). A framework that is absent, too old, undetectable or not declared by the game refuses the install. `--allow-missing-frameworks` installs it anyway, and names each one and why (`plan.unmet_frameworks` in `--json`); the mod probably won't work until the framework is added, and `agora games instance check` keeps saying so. The file comes from the entry's GitHub release (the newest non-draft, non-prerelease release with a matching asset) or its `direct_hash` URL, and is checked against what its source published, with `--install-anyway` only overriding a curator pin or a hash remembered from an earlier install. A `manual_link` entry is never downloaded by Agora: without `--file` the command prints the page, the file-name pattern and any hint, changes nothing, and exits `71` (run it again with `--file <path>`). With `--file`, the chosen file is copied into the content store and never moved, changed or deleted; its name must match the entry's pattern (a mismatch prints the expected pattern, changes nothing and exits `71`; `--install-anyway` accepts it), and its SHA-256 is checked against a curator pin or an earlier install exactly like a downloaded file. The page usually publishes no checksum, so the report says the file is not verified and records its SHA-256; only install a file downloaded from the page shown. An archive with a FOMOD installer is imported but not added. That is an expected outcome, not an error: the command prints what to run next (`agora games content fomod install`, with `--defaults` or `--choose`), without an `Error:` prefix, and exits `71` (the decision code in the exit-code table) so a script can tell it did not install. With `--json` it prints the report (`state` `needs_installer`) and the same exit code. `--dry-run` shows the plan, names any unmet framework instead of refusing, and downloads nothing |
| `agora games base build <install-id> [--mode linked\|copied] [--include-excluded]` | Build a pinned base from an identified install (default mode: linked). `--include-excluded` also copies the files the game definition leaves out of an instance. Replacing a damaged base refuses while a live or Recently deleted instance still pins it, or while an instance move holds the locations lock |
| `agora games base list` | List all pinned bases and their availability. A record that cannot be read is listed as damaged, with its file and the reason |
| `agora games base verify <base-id> [--full]` | Verify base integrity (quick cheap check by default, full hashes all files) |
| `agora games base remove <base-id> [--json]` | Delete a pinned base and its manifest. Refuses while a live or Recently deleted instance pins the base, including an entry that has expired but not been purged, and while an instance's manifest or a `recently-deleted` folder pins it with no live row. A damaged record is removed with its folder in the bases folder, when that folder is there; a folder with the base's name under another bases root is named and left alone. A damaged record whose folder is found nowhere is still removed and the command succeeds, with a warning naming every bases root it searched (`warnings` in `--json`, printed to stderr as `Warning: …`): the folder may be on a drive Agora has no root for, and the space is not free until it is checked. If an instance move or another cleanup holds the instance-locations lock, the base is kept and the command asks you to retry |
| `agora games base repair-framework <base-id> <framework-id>` | Copy a framework's declared required files from the base's source install into the base. This repairs a base pinned before the framework declared those files (for example SKSE's `Data/Scripts/SKSE.pex`). Files the source also lacks are named, and the command exits non-zero: install the framework as content in the instance, or reinstall it into the game install, then run it again |
| `agora games content add <path> [--name N] [--without-unwritable-names]` | Add a zip, 7z or RAR archive (RAR is read through Windows' own tar) or a folder to the content store. An entry whose name a disk cannot hold (a Windows reserved name, a trailing dot or space, a `:`, or two names one case-insensitive disk stores as one) refuses the import and names every offending entry and why; `--without-unwritable-names` stores the entries that can be written and lists the ones it left out. With `--json`, the refusal carries the same names as `unwritableNames` and the import reports them as `skipped` |
| `agora games content list` | List all items in the content store |
| `agora games content unused` | List items no instance uses, with their size. An unreadable instance manifest is a warning, and removal refuses while one exists |
| `agora games content fill-details` | Fill missing descriptions, names, versions, page links and icons from Modrinth, in batched lookups. Fields an item already has are never overwritten; Modrinth's Privacy switch and Lockdown Mode apply |
| `agora games content show <item>` | Show files in a content item |
| `agora games content verify [<item>] [--full]` | Verify integrity of content items |
| `agora games content remove <item> [--ignore-unreadable-snapshots]` | Delete an item from the content store. It refuses while an instance or a snapshot uses it. A snapshot refusal names the snapshot, its instance and time, and the exact `agora snapshots delete <instance> <id>` command. The `--ignore-unreadable-snapshots` override lets removal proceed and names each ignored snapshot on stderr. Its objects are omitted from the in-use check and may be reclaimed, so it may not restore. Once the record is gone, one collection decides which stored objects are unreferenced. The command prints what it reclaimed or why it reclaimed nothing. It keeps files stored less than an hour ago. With `--json`, the `collection` field contains `summary` (the same sentence), `reclaimed` (the number of objects freed), and `refused` (the collection freed nothing and `summary` says why) |
| `agora games content gc [--ignore-unreadable-snapshots] [--json]` | Reclaim unreferenced objects from `content/objects` only. The migration moves the old `snapshot-objects` store, which is outside the collection's scope. The collector checks item records, live and Recently deleted instance manifests, snapshot manifests, and pending records. If a record cannot be read, the collection refuses and names it in the summary. The collector keeps objects written less than an hour ago and reports when the grace period applies. It waits briefly for the content-store and instance-locations locks and skips collection while another operation holds them. With `--ignore-unreadable-snapshots`, it names each ignored snapshot on stderr. Its objects are omitted from the in-use check and may have been reclaimed, so it may not restore. With `--json`, it prints `{"status": "collected", "collection": …}` with the same `summary`, `reclaimed`, and `refused` fields as `content remove`. It exits `0` even when the collection refuses. Read `collection` for the reason |
| `agora games content migrate [--restart] [--json]` | Move the old `snapshot-objects` store into the content store, re-hashing every object. Once matching bytes are verified in `content/objects`, migration removes the legacy copy. It keeps and reports mismatches and stray files. Interrupted runs resume, and reopened runs add to the completed counts. Use `--restart` to set an unusable journal aside and walk the old store again. If a link or reparse point blocks migration, remove or replace the named path and run `agora games content migrate`. With `--json`, `done` reports `moved`, `already_present`, `replaced_damaged`, `mismatched` (paths), and `left_alone` (paths and reasons). Other statuses are `nothing_to_do`, `busy`, and `needs_hand`, which gives a `reason` and an optional `command`. `migrate` exits `72` for `needs_hand` and `11` for `busy`. `agora doctor` exits `10` when its report has a needs-a-hand entry |
| `agora games content share [--on \| --off] [--convert] [--instance <id>]... [--dry-run] [--json]` | Show or change the **Share mod files between instances** setting. On, new and converted files are hard links to one stored copy; off, each instance gets its own editable copy. With `--convert` (and `--on` or `--off`) existing instances are converted too: a file whose bytes match its manifest entry is reshared or copied, anything changed by hand is left alone and listed, a running or locked instance is refused, and an interrupted run finishes on the next one. `--dry-run` prints what would change without changing anything |
| `agora games content fomod show <item>` | Show the steps, groups and options of the FOMOD installer in an archive item |
| `agora games content fomod install <item> [--instance <id>] [--choose "Step/Group/Plugin"]... [--defaults]` | Run an archive item's FOMOD installer into a new content item (the archive's objects are shared, the choices are recorded); with `--instance`, check the installer's file conditions against that instance and add the result to it |
| `agora games launch <base-id> [--wait] [--launch-anyway] [--plain]` | Launch a game from its pinned base (`--plain` skips a framework loader such as SKSE's and starts the game's own executable). A framework built for another game version refuses the launch unless `--launch-anyway` is given, as a damaged base does |
| `agora games instance create <install-id> [--name N] [--id ID] [--mode linked\|copied] [--include-excluded]` | Create an instance for a game from an identified install. `--include-excluded` also brings in the files the game definition leaves out of an instance |
| `agora games instance list` | List all game instances, Minecraft included |
| `agora games instance launch <id> [--wait] [--launch-anyway] [--plain] [--deployment virtual\|links\|copies\|auto] [--fall-back] [--skip-before-launch]` | Launch a game instance. **On Linux with a Steam install**, Agora records the pending launch, asks Steam to start the game (`steam steam://rungameid/<appid>`) and prints the launch options to paste when `agora games wrap %command%` is not there; the wrapper Steam starts does the waiting and diagnosis, so `--wait` is not needed. Every Linux launch prints that Proton has not been tested on real hardware yet. On other platforms it starts through a framework loader (SKSE's `skse64_loader.exe`) when one is installed, and says so (`--plain` starts the game's own executable instead; on Linux the wrapper honours the same choice and does not substitute a loader). `--deployment` overrides the instance's rung for this launch. If the virtual file system cannot start (`agora_vfs.dll` missing, injection refused, the game ended before the DLL confirmed its hooks), nothing is launched and Agora says what failed and why, what linked files give up, and asks `Try again from linked files? [y/N]`; yes launches again from linked files for this launch only and prints `set-deployment <id> links` to keep it. Without a terminal, or with `--json`, it never asks: it exits 1 and prints both commands (`--json`: `nextDeployment` and `retry` in the error object). `--fall-back` answers yes in advance, for a rung you chose too, and announces the step down. After a `--wait` session under the virtual file system, a program the game started that the DLL had to stop (it could not protect it) is named with the reason, and the same choice is offered. Before it starts, the launch checks the framework files against the game's version: a framework built for another version (SKSE, or an Address Library for another runtime) refuses the launch and exits 1, naming the framework, what the game has, what it needs and the repair; `--launch-anyway` starts the game anyway and prints the findings as a warning. After the plugin list is synced, the launch also checks the plugin load order (see `plugins check`): a master that is missing or inactive, a master loop, or too many plugins refuses the launch and exits 1, naming each problem; `--launch-anyway` starts the game anyway. A master that loads after its plugin only warns, because the game still starts, but the plugin's references may resolve wrongly: the warning names `agora games instance plugins sort <id>`, which fixes it. An unreadable header or a plugin listed twice only warns too. With `--json`, stdout is exactly one JSON object: the human lines (`Deployed:`, `Restored N per-user file(s)`, the quick-exit hint) are not printed there. Their facts are fields instead: `deploy_summary` (`status` `built` with `linked`, `copied`, `copied_bytes`, `config_copied` and a `harvest` count object, or `up_to_date`; `null` when nothing was deployed), and, with `--wait`, `user_files_restored` (`{restored, changed}`, or `{error}` when the per-user files could not be put back) and `session_ended_quickly`. A warning or failure note still goes to stderr. `--skip-before-launch` starts the game without running the tools its plugin says must run first, even when their output is missing or stale |
| `agora games instance check <id>` | Check the framework files of an instance against its game's version, without deploying or launching anything. It also names any framework a catalog mod in the instance needs and does not have, as a warning that does not change the exit code (`requirement_findings` in `--json`). It prints each finding (what the game has, what it needs, the repair) and exits 1 when a framework was built for another version. A rule that cannot be checked (a bad game definition) is listed as a warning and does not fail the check, and a launch never refuses for it. It makes the same check a launch makes; `--json` prints `status` (`ok`, `warnings` or `findings`) and the `findings`. For a pinned instance of a game with a plugin list it also checks the plugin load order, lists its findings (`--json`: `load_order_findings`), and exits 1 for the ones a launch refuses. It also lists the tool output that is `stale` or `unknown` (`--json`: `generated_findings`) as warnings, which never change the exit code, and the game's declared INI-setting warnings (for Skyrim, `uGridsToLoad` and `uExterior Cell Buffer`), which are also only warnings. Warnings from the game's own plugin are listed last (`--json`: `diagnostics`): for example an SKSE plugin built for another game version, or a plugin whose header needs game 1.6.1130 or later. They are warnings, so they never change the exit code |
| `agora games wrap <command...>` | The wrapper a Steam launch option runs (`agora games wrap %command%`), for Steam games on Linux. It picks the pending launch `games instance launch` recorded in the last two minutes, else the game's default instance, else it runs Steam's command untouched and says why on standard error. When the instance's deployed view has a launch alternative whose condition holds (SKSE's `skse64_loader.exe`), it replaces the game executable in Steam's command with that loader from the same folder and prints one line saying so; the Proton wrapper, its arguments and everything else Steam passed are left alone. A `--plain` launch is recorded in the pending launch, so no loader is substituted; if no argument names the game's executable (declared by the game definition, case-insensitively, with launcher variants such as `SkyrimSELauncher.exe` handled), the command runs untouched with the reason on standard error. It opens the session in the ledger, mounts the instance's view over the game's install folder, bind-mounts the instance's per-user files inside the Proton prefix (`steamapps/compatdata/<appid>/pfx/drive_c/users/steamuser/...`), waits, closes the session and diagnoses it like `games instance launch --wait`. A view that cannot mount is announced and falls back to links or copies. It exits with the game's exit code (MASTER_SPEC §26.5) |
| `agora games instance default <id>` | Make an instance its game's default, so `agora games wrap` picks it when Steam starts the game without a pending launch. The choice is per game |
| `agora games instance set-deployment <id> virtual\|links\|copies\|auto` | Choose how an instance is deployed and run: under the virtual file system, from hardlinks, or from copies. On Windows the virtual rung is Agora's injected copy-on-write VFS; on Linux it is an overlayfs view (see **Deployment on Linux** below). `auto` clears the choice. A launch always asks before stepping down from the virtual file system, whatever is chosen (see `launch`) |
| `agora games instance delete <id> [--force]` | Delete a game instance and its folder. It refuses while a session for the instance is starting or running. `--force` deletes even when the instance's base record cannot be read: the instance folder goes, while a deployed game folder Agora cannot prove is the instance's is left in place and named |
| `agora games instance rename <id> <name>` | Give an instance a new name. Its ID and folder stay the same |
| `agora games instance clone <id> <name> [--copy-saves]` | Make a new instance with the same mods, plugin order, INI files, settings and saved loadouts. `--copy-saves` copies eligible saves into the clone's own folder. Other instances' private saves, backup history and loadout undo entries stay with their original instances |
| `agora games instance stop <id>` | Close the game Agora started for an instance and end its session as usual. A game you started yourself is not stopped; close it yourself |
| `agora games instance deleted <game>` | List a game's instances in Recently deleted (ID, name, when, how long it is kept, size), newest first. Entries past the retention window are cleaned up first, as the desktop does |
| `agora games instance restore <entry> [--name <name>]` | Bring a deleted instance back. Without `--name` it keeps its old ID, and is refused if that ID is taken again; with `--name` it comes back under a new one |
| `agora games instance purge <game> [<entry>] [--yes]` | Send one deleted instance, or all of that game's deleted instances, to the OS trash. Agora cannot restore them afterwards. It asks first in a terminal; without one it needs `--yes`, and otherwise refuses |
| `agora games instance content add <instance> <item> [--into <path>] [--from <subfolder>]` | Add a content item to an instance's layer stack; `--from` deploys only that subfolder of the item. An archive with a FOMOD installer is not added: that is an expected outcome, not an error, so the command prints what to run next (`agora games content fomod install <item> --instance <instance>`, with `--defaults` or `--choose`) without an `Error:` prefix, and exits `71` (the decision code). With `--json` it prints one envelope on standard error (`status` `needs_installer`, `installer_command`, `exitCode` `71`) |
| `agora games instance content list <instance>` | List content layers on an instance in priority order |
| `agora games instance content remove <instance> <item>` | Remove a content layer from an instance |
| `agora games instance content enable <instance> <item>` | Enable a content layer on an instance |
| `agora games instance content disable <instance> <item>` | Disable a content layer on an instance |
| `agora games instance content move <instance> <item> <position>` | Move a content layer to a 1-based position |
| `agora games instance content own-copy <instance> <item> on|off` | Configure whether all files deployed by a content layer are copies rather than links |
| `agora games instance deploy <instance> [--copies\|--deployment virtual\|links\|copies]` | Deploy an instance's content layers to its game folder, as the instance's chosen rung, else the mode a launch would use |
| `agora games instance undeploy <instance>` | Harvest game writes and tear down an instance deployment |
| `agora games instance plugins <instance> [--json]` | List the instance's load order: each plugin's position, state, master (`M`) and light (`L`) flags, lock, whether Agora manages it (a content layer deploys it) or you wrote the line, and the findings. Always-loaded plugins are marked `always` |
| `agora games instance plugins enable <instance> <plugin>` | Activate a plugin in the instance's plugin list |
| `agora games instance plugins disable <instance> <plugin>` | Deactivate a plugin; it stays listed and a redeploy leaves it that way |
| `agora games instance plugins sort <instance> [--dry-run]` | Sort the list with LOOT's masterlist when one is saved, otherwise by masters alone. Locked and always-loaded plugins do not move, and free plugins only trade places among free slots; `--dry-run` prints the moves and writes nothing. With no saved masterlist it says it sorted by masters only |
| `agora games instance plugins masterlist <instance>` | Download and verify LOOT's masterlist for the game (through GitHub, checked against the git blob SHA-1). Nothing is downloaded unless you run this, and a failed check leaves the saved copy alone |
| `agora games instance plugins move <instance> <plugin> (--to N \| --before P \| --after P) [--break-rule]` | Move a plugin to a 1-based position, or just before or after another plugin. Locked plugins do not move. A move that breaks a load-after rule is refused and names the rule and the command that removes it; `--break-rule` removes that rule instead |
| `agora games instance plugins lock <instance> <plugin>` | Lock a plugin's place, so sort and move leave it where it is |
| `agora games instance plugins unlock <instance> <plugin>` | Unlock a plugin's place |
| `agora games instance plugins check <instance> [--verbose] [--json]` | Print the load order findings (a master that does not come first, a master loop, an unreadable header, a plugin listed twice, too many plugins), the overlaps summary, and LOOT's checks. When a masterlist is saved it says how many plugins LOOT would move and names `plugins sort`; `--verbose` names each move, and `--json` carries them in the finding. LOOT's own warnings and errors are printed at their level (its `say` notes stay in the plugin view), along with an active plugin LOOT marks incompatible with another active plugin, an active plugin LOOT says requires a missing file, and LOOT's cleaning counts (deleted navmeshes warn, the rest are info) with the cleaning guide. Without a saved masterlist it says how to get one, naming `network_plugins_enabled` when plugin downloads are off. Exits 1 only when a launch would refuse (a missing or inactive master, a master loop, too many plugins); warnings exit 0 |
| `agora games instance plugins conflicts <instance> [--plugin <name>] [--json]` | List the record changes one active plugin's copy replaces in another's, grouped by the pair. Each pair names the winner and loser, how many changes, and example record labels. Records the plugins agree on, records the winner patches (it lists the other plugin as a master), and records a load-after rule decides are counted rather than warned about. `--plugin` shows only the pairs that name that plugin and what it wins and loses, including a change a load-after rule decides: it says the plugin loses those changes to the winner by your choice. Exits 1 when the game declares no overlaps reader or the reader fails, because conflicts then could not be checked |
| `agora games instance plugins conflicts <instance> --let-win <loser> --over <winner>` | Let the plugin that currently loses win: add the `--let-win` rule and move that plugin after the winner now. The move is refused if it breaks another rule |
| `agora games instance plugins conflicts <instance> --keep <winner> --over <loser>` | Keep the current order: add the rule that makes the winner load after the loser, so a later sort keeps this pair as it is |
| `agora games instance plugins rule add <instance> <item> --after <other>` | Make `<item>` load after `<other>`. A rule that contradicts a master, or that would make a loop with the rules and masters already in place, is refused with the reason |
| `agora games instance plugins rule list <instance>` | List the instance's load-after rules, and say when a rule is unused because one of its plugins is no longer in the list |
| `agora games instance plugins rule remove <instance> <item> --after <other>` | Remove a load-after rule, including an unused one |
| `agora games instance ini <id> [<file> [<section> [<key> [<value>]]]] [--unset]` | Read and change the game's INI files as the instance keeps them. With no file, lists the game's per-user files for the instance's store, whether the instance has its own copy, and the game's own file. A file is named as the instance keeps it (`user/Skyrim.ini`), or by its bare name (`Skyrim.ini`) when exactly one of the instance's files has it; an ambiguous bare name lists every match. With a file, lists its keys and where they came from: the instance's copy, the game's file while the instance has no copy yet, or nothing. With a section and key, prints the value and exits 1 when it is not set; with a value, sets it; with `--unset`, removes the key. An edit writes only the instance's copy, which is swapped in for the next launch; when the instance has no copy yet it starts from the game's file. The game's own file is never edited. Lines Agora does not change are kept as they are: comments, spacing around `=`, the spelling of keys and sections, line endings, a byte order mark, and lines it cannot parse. Names are case-insensitive. Refused while the game's session is running, or while an earlier session's files have not been put back (`games user-files restore`). `--json` prints the result |
| `agora games instance saves <id> [own\|shared] [--json]` | Show or change the instance's choice of save folder. The game package declares the folders and how to recognise saves. An unknown save format has an unknown count (`null` in JSON). Games with an INI save setting, such as Skyrim, use `own` to point the setting at private saves and `shared` to restore its previous value. A setting edited by hand is kept. Switching leaves save files in place. Refused while the game's session is running |
| `agora games instance save-backup create <id> [--session <id>] [--allow-while-running]` | Copy the instance's save folder into a new backup and say how large it is. Shared backups exclude private saves. Close the game first, or pass `--allow-while-running` to copy even though a live game may change the files. `--session` also records the backup for a crash-test session |
| `agora games instance save-backup list <id> [--session <id>]` | List the instance's save backups, newest first. With `--session`, show only the backup recorded for that session |
| `agora games instance save-backup restore <id> <backup>` | Restore into the original folder if it matches the game's shared folder or this instance's private folder. Older shared backups skip private entries. A current save with the same name is kept beside it as `.before-restore`, and saves made since stay |
| `agora games instance mod-ini list <id>` | List the INI files the instance's enabled mods ship, with your own copies, and say when the game has changed one |
| `agora games instance mod-ini get <id> <path>` | Show one mod INI file's keys: your copy when you have one, otherwise the mod's file |
| `agora games instance mod-ini set <id> <path> <section> <key> [<value>] [--unset]` | Set a key in your copy of a mod's INI file, or remove it with `--unset`. The first edit makes your copy from the mod's file; the mod itself is never changed |
| `agora games instance mod-ini keep-game <id> <path>` | The game changed the file: keep the game's version as your copy |
| `agora games instance mod-ini restore-mine <id> <path>` | The game changed the file: go back to your version and set the game's change aside |
| `agora games instance mod-ini reset <id> <path>` | Remove your copy, so the mod's own file is used again |
| `agora games instance tools list <instance> [--json]` | List each tool the game declares (for example `nemesis` for Skyrim SE), with its current and previous output, and whether that output is `current`, `stale` (an input changed since it was built) or `unknown` (it was imported, so nobody recorded its inputs). A tool with no output says `not built`. A launch and `games instance check` warn about stale and unknown output; they never refuse for it, and say which command rebuilds it |
| `agora games instance tools run <instance> <tool> [--capture auto\|vfs\|links\|swap] [--yes] [--json]` | Run a tool in the instance and capture what it writes, so it never lands in a mod or the game's own writable layer. A tool that works on the real install (Skyrim's `nemesis` and `bodyslide`, which read the install path from the game) cannot be captured by the virtual file system or by links, because it writes where the game reads. On a linked or copied instance, `auto` refuses such a tool before anything is deployed: in a terminal it explains the swap in two lines and asks `[y/N]`; with `--json` or no terminal it refuses and prints the `--capture swap` command. `swap` is the way such a tool runs, on a real, writable copy of the instance's `Data` that is swapped in for the length of the run. The copy is made from the instance's plan as real files, never as links. On the same volume as the install it is kept in the instance's swap folder (`deployments\<instance>\swap\Data`) between runs: each run copies only what changed, and the first run copies everything. On another volume a copy is made on the install's volume for the run and deleted afterwards, so every run copies everything in and the changes back out. For the length of the run the real `Data` is renamed to `Data.agora-aside-<run>`, the copy is renamed into its place, and the tool runs from the install folder. When the tool exits the copy goes back, and the real `Data` comes back from the aside. Before anything is copied or moved, the swap shows what it costs: the disk space the copy needs and the free space on its volume; whether the first run copies everything and later runs only what changed, or every run copies everything in and back; that the real `Data` is renamed for the length of the run, so Steam must not update the game and the game must not be started; and a warning when Steam is running. In a terminal it asks `[y/N]`. With no terminal it needs `--yes`, which confirms the swap after the same costs are shown, and otherwise it prints the command and refuses. The journal is written before the copy is touched, and a crash is put back by the next Agora start (CLI, desktop or MCP all run the same startup recovery), by the next launch, deploy, tool run, or `games tools-swap restore`; recovery discards the copy's writes rather than capturing them. The changes are captured by comparing the copy with the record it held before the run, by the change detection link capture uses (size, then time, then the bytes against the source). `swap` refuses while a program runs from the install folder, when `Data` is already a link, when the aside name is taken, or when the copy's volume lacks the space for it. Steam and the game must not run during a swap. Every run also reports what it wrote to the real install folder outside what Agora captured (a report, never a refusal; a buffer overflow says changes may have been missed) | `--capture auto` (the default) on a virtual instance runs every tool under the virtual file system, a tool that works on the real install included. When the virtual file system cannot start (for example a 32-bit game, which `agora_vfs.dll` cannot load into), nothing runs: the run says what happened and what the next tier does, and names it. The next tier is link capture for a tool that uses its own folder, and the full-copy swap for one that works on the real install. In a terminal `auto` asks `[y/N]` before it captures from that tier, and says what failed first. `--yes` confirms it, as it does a swap, under `--json` too: the next tier then runs, and its outcome is printed as JSON, whose `capture_reason` names the virtual file system's failure. Without `--yes`, `--json` or no terminal prints the envelope (with the exact retry command for that tier, and any offers) and exits 1. A linked or copied instance (set with `set-deployment`) captures a tool that uses its own folder from links directly, with no attempt at the virtual file system. `vfs` runs under the virtual file system only: when it cannot start it refuses, names the next tier and prints its command, and exits 1. `links` captures from linked files: the tool runs in the game's folder, and what it changed is compared with the deployment record. New and changed files become the output and deletions become whiteouts; a linked file that the tool changes is refused, and the run is discarded and names it. The farm the run used is then taken away, so the game's next launch deploys the promoted output. The instance is deployed in the virtual file system first (or in links, for link capture), and the game's user files are swapped in for the run, as for a launch. Refused while a game session is recorded for the game (its per-user files are swapped in and not yet put back), and while a game process runs from the instance's folder. The run holds the instance's lock for its whole length, so a launch of the same instance waits for it (30 seconds at most) and then fails. A run that exits 0 becomes the tool's output (a generated layer above the mods and below the game's writes); the output it replaces is kept for rollback, and the one before that is deleted. Any other run (a non-zero exit, a cancel, a crash) is discarded and kept as `failed-<run>` (the last three are kept), and the output in effect stays. It lists what the run wrote and deleted, says which capture it used, and exits 1 when the output was not promoted. A tool that fails twice in a row carries offers, which are shown and never applied. A run the virtual file system refused to start counts as a failure, as a discarded run does. The offers: the full-copy swap, with its costs (asked `[y/N]` in a terminal, or printed as the command), when a failing run was not a swap; and switching the instance to linked files, printed as the `set-deployment` command, only when the virtual file system failed to start or ended a program the failing run started, and never for a tool that works on the real install. The count is kept in the tool's folder (`failures.json`); a promoted run resets it, and a missing or unreadable count counts as none |
| `agora games instance tools rollback <instance> <tool> [--json]` | Make the previous output current again, so the game reads it on its next launch. Nothing is deleted, and running it again swaps back |
| `agora games instance tools remove <instance> <tool> [--json]` | Remove a tool's output from the instance: its layer goes, and so do its generations and discarded runs. The mods are not touched |
| `agora games instance tools diff <instance> <tool> [--json]` | Show the files added, changed and removed between the tool's previous and current output. It needs a previous output, so it fails until a second successful run |
| `agora games instance tools outputs <instance> <tool>` | List a mod-writing tool's earlier outputs, newest last |
| `agora games instance tools use-output <instance> <tool> <item>` | Use one of a tool's earlier outputs again. The game reads it on its next launch |
| `agora games instance tools set-input <instance> <tool> <input> [<value>]` | Save a value for one of a tool's inputs. Leave the value out (or empty) to clear it, so the input's default applies again |
| `agora games instance user-tools list <instance>` | List the programs you added to the instance, and say which are missing now (their mod is off or removed, or the file moved) |
| `agora games instance user-tools find <instance>` | Look through the instance's enabled mods for programs you could add. Adds nothing |
| `agora games instance user-tools add <instance> <name> <exe> [--output-is-mod] [--output-folder <dir>] [--keeps-output] [-- <args>...]` | Add a program: a path inside the game folder, as `find` shows it, or a full path outside it. `--output-is-mod` says it writes a whole mod (added after a successful run, into `--output-folder`); `--keeps-output` says it builds files the game reads, kept like a game tool's output. Its own arguments go after `--` |
| `agora games instance user-tools remove <instance> <id>` | Remove a program from the instance's list. No files are deleted |
| `agora games instance user-tools run <instance> <id> [--capture auto\|vfs\|links\|swap] [--yes] [--move-aside]` | Run one of your programs the same way `tools run` runs a game's tool, with the same questions before a swap or a fallback. When a mod-writing program's folder already has files, nothing runs unless `--move-aside` moves them aside first |
| `agora games instance user-tools outputs <instance> <id>` | List a mod-writing program's earlier outputs, newest last |
| `agora games instance user-tools use-output <instance> <id> <item>` | Use one of its earlier outputs again |
Every `agora games ...` command accepts `--while-running`. It is the explicit override for a game that is already running: without it, a command that edits an instance (deploy, undeploy, set-deployment, content add/remove/enable/move/own-copy, plugin enable/disable/sort/move/lock/unlock, INI and mod-INI edits, tool runs/outputs/inputs, load-after rules and user-tool edits) refuses at once with "the game is running in <instance>". The instance lock is still taken with the override, so two Agora operations never change one instance at once; the running game keeps the files it already opened, and may overwrite the change when it saves. A launch never takes the override.
| `agora games user-files status [<game>]` | List per-user file swap sessions in progress |
| `agora games user-files restore <game> <store> [--force]` | Restore original per-user files from a finished session. When both the game's own file and the instance's copy changed since the swap, the restore refuses and keeps the game file's current bytes as a clearly named conflict copy (`<name>.agora-conflict-<hash>`) next to the instance's copy; `--force` is the explicit override for that one conflict — the conflict copy is still written first, then the files are put back. Automatic startup recovery never forces |
| `agora games tools-swap status [--json]` | List the tool swaps in progress: the game, store, instance and tool, whether a tool run is still going, and what the real `Data` folder and its aside folder are now. Nothing is changed |
| `agora games tools-swap restore` | Put back every interrupted swap whose tool run is no longer going: the copy in place of `Data` is moved home (only after its marker is checked), the aside folder is renamed back to `Data`, the copy is deleted (only when its marker matches the journal), and the journal is deleted last. Nothing is merged or overwritten. A case that needs a hand (both folders exist, or a folder at the copy's path is not Agora's copy) is reported with the steps, and nothing is changed. A journal from the old junction swap (version 1) is refused by name. The same check runs at every CLI start and before any launch, deploy, tool run or user-files session for the game |
| `agora games tools-swap remove-copy <instance> [--json]` | Delete the instance's kept copy of its `Data` (`deployments\<instance>\swap\Data`) to free its space. Only a copy whose marker names this instance is deleted; any other folder there is refused and nothing is deleted. The instance's next swap copies `Data` again. Deleting the instance removes its swap folder too |
| `agora games import mo2 scan [--json]` | List Mod Organizer 2 setups on this machine: the instance folders under `%LOCALAPPDATA%\ModOrganizer`, and a shallow scan of each fixed drive for `ModOrganizer.ini` (to depth 3, skipping `Windows`, `Program Files*` and `$Recycle.Bin`). Shows each setup's path, game, profiles and mod count. The mods are not read |
| `agora games import mo2 <ModOrganizer.ini> --profile <name> [--name N] [--install ID] [--dry-run] [--copy-saves] [--json]` | Import one profile of an MO2 setup as a new game instance (MASTER_SPEC §26.10). Every listed mod is stored as it is on disk, enabled and disabled, so local edits survive and nothing is downloaded. Content layers keep MO2's priority. `overwrite` is split: the files a declared tool produces (Nemesis's behaviour files and its engine folder) become that tool's generated layer with its inputs unknown, and the rest become one "MO2 overwrite" layer above the mods. The plugin list keeps MO2's load order, activation and locks; the profile's own INIs and saves become the instance's own. Mod `meta.ini` claims are recorded as provenance, not verified. The setup is only read. Refused when the same setup and profile were imported before, unless `--name` gives a new instance. `--dry-run` prints the plan and stores nothing. Saves are copied (never moved) under Documents only with `--copy-saves`. The install is the one at the setup's `gamePath`; when the game is not there but Agora found it elsewhere, `--install <id>` (an id from `agora games list`) imports onto that install instead, and an install of another game is refused |
| `agora games import vortex scan [--store DIR] [--json]` | List what a Vortex state store holds: each Vortex game, whether Agora supports it (matched through the game folder Vortex discovered), its staging folder, deployment-manifest and plugin-list status, and every profile with its enabled and total mod counts. The store defaults to `%APPDATA%\Vortex\state.v2`. Nothing is written; a game Agora does not support is listed as not supported |
| `agora games import vortex --game <id> --profile <name> [--store DIR] [--name N] [--install ID] [--add-folder] [--copy-saves] [--all-mods] [--verbose] [--dry-run] [--json]` | Import one Vortex profile as a new game instance (MASTER_SPEC §26.10). `<id>` is the id `scan` lists (its Vortex id, such as `skyrimse`, or the Agora game id). Vortex must be closed: Agora refuses while Vortex holds the store's `LOCK`, and says so. Agora reads a **copy** of `state.v2`, never the live store. By default only the mods this profile enables are stored, because Vortex shares one staging folder across all of a game's profiles; disabled mods are counted as left out, and `--verbose` lists them. `--all-mods` brings them too, turned off. Every imported mod is stored as it is on disk; Nexus mod and file ids are recorded as provenance, not verified. The plugin list comes from the profile's `plugins.txt` / `loadorder.txt` (the game definition selects the format. An unmarked entry stays inactive in a `*`-format list). The profile's LOOT `userlist.yaml` `after` entries become the instance's load-after rules; a rule that names a plugin not in the list, itself, or closes a cycle is refused with the reason, and `req`/`inc` entries are reported as not imported because Agora has no equivalent. Vortex orders mods by rules, not a list, so layer order comes from `vortex.deployment.json` (the mod that supplied a file wins it) with Vortex's `before`/`after` rules as the tie-breaker; every file the manifest and the rules disagree on is reported, and the preview and report name files whose winning mod the imported layers cannot reproduce. For a missing or unreadable manifest, the plan shows where it looked and which winners it could verify. A profile with `local_game_settings` brings its own INI copies; a profile with `local_saves` uses its own saves, copied under Documents only with `--copy-saves`. Downloads are not copied. The staging folder and the store are only read. When Vortex's game folder is a real install Agora does not know, the scan says so; `--add-folder` adds it (the `games folder add` path) before importing. `--dry-run` prints the plan and stores nothing; `--name` is needed to import the same profile again; `--install <id>` imports onto another install of the same game |
| `agora games import mo2 <ModOrganizer.ini> --profiles <a,b,...\|all> --as merged\|separate [--name N] [--dry-run] [--json]` | Import several MO2 profiles (MASTER_SPEC §26.10). `merged` makes one instance holding the union of the chosen profiles' enabled mods and a loadout per profile, named after it, restoring its own mods, layer order, plugin list, INIs and scoped save folder; the first profile is applied. `separate` makes one instance per profile (with that profile's listed mods) and returns a per-profile report of created instances, any errors, and files written outside the instance (copied saves) so a late failure does not hide earlier instances. Before copying, the plan states the exact bytes the choice copies (mods, overwrite, and saves when `--copy-saves` is given, without double-counting shared content) and the free space on the content store's drive, and the import is refused only when they cannot fit. `--name` names the merged instance (or the prefix of a separate instance's name). `--dry-run` prints the plan and writes nothing |
| `agora games import mo2 <ModOrganizer.ini> --suggest [--json]` | Print Agora's grouping of the setup's profiles: profiles sharing more than half of their enabled mods are suggested for one instance, and every pair's shared and combined counts are shown |
| `agora games import vortex --game <id> --profiles <a,b,...\|all> --as merged\|separate [--store DIR] [--name N] [--copy-saves] [--all-mods] [--dry-run] [--json]` | Import several Vortex profiles. In merged mode each loadout restores its own layer order, plugin list, INIs, LOOT `after` rules and save choice. The preflight counts each shared staging mod once per batch, includes disabled mods with `--all-mods`, and includes profile INIs and any saves requested with `--copy-saves`. Separate mode reports each profile's instance ID or error and any saves written outside the instance. If any profile fails, the command prints the report and exits with an error |
| `agora games import vortex --game <id> --suggest [--store DIR] [--json]` | Print Agora's grouping of the game's Vortex profiles and the overlap numbers, and stop |

**Deployment on Linux.** The virtual rung is an overlay mounted over the game folder inside a
private user and mount namespace, so the install folder is unchanged outside the namespace and the
game's writes go to the instance's writable layer. The lower layer is the instance's link deployment
(each mod file a link to the content store, over the pinned base); the upper layer is the instance's
writable layer. Kernel overlayfs with `userxattr` is chosen by a real probe mount, never by kernel
version; when it cannot mount, `fuse-overlayfs` is used if it is on `PATH`; when neither can mount,
Agora runs from linked files (and, if links cannot work, copies) and says why, through the same rung
chain as Windows. Per-user files are bind-mounted from the instance's copies inside the namespace,
so the real files are never swapped or changed. Steam starts the game on Linux, so the overlay is
entered by `agora games wrap %command%` in the game's Steam launch options. `agora games instance
launch` asks Steam to start it (with its `steam` command, or `flatpak run com.valvesoftware.Steam`
when only a Flatpak Steam is installed) and prints the exact text and the files Agora read when the
wrapper is missing; Agora only reads `localconfig.vdf`, never edits Steam's files. The game's Windows
`Documents` and `LocalAppData` files, and its crash sources, resolve inside the Proton prefix's
`drive_c/users/steamuser` folders. Launching through Proton has not been tested on real hardware
yet, and the CLI says so on every Linux launch (MASTER_SPEC §26.5).

Every `games instance plugins` command takes `--list <id>` to name one of a game's load orders, when its plugin declares more than one. Without it, the game's first list is used.

For games that keep a plugin list (Skyrim's `Plugins.txt`), deploying an instance adds the
plugins its content layers provide to the instance's own copy of the list, active, and removes
the lines of layers you take away. A plugin you deactivated stays deactivated, and lines you
wrote yourself are never removed. The game's own masters are never listed. `--wait` follows
every process running from the instance's game folder, so a loader that starts the game and
exits does not end the wait early.

For Skyrim Special Edition the list also follows the Creation Engine's load order rules. The
game always loads its own masters and the Creation Club plugins named in `Skyrim.ccc` first, so
those are shown as `always` and are never written into your list. A plugin that needs a master
must load after it, and a plugin whose header cannot be read is reported by name. `plugins check`
prints these findings, then the overlaps summary and LOOT's own checks (the plugins it would move,
its warnings and errors, incompatibilities between active plugins, missing required files, and its
cleaning counts with the cleaning guide). A LOOT message that carries a link prints it as
`text (address)`, never as raw markup. The same lines appear in `games instance check` and before a
launch, from the same code. `plugins sort` uses LOOT's masterlist when one is saved, otherwise it moves
masters up with as few moves as it can. Either way it never moves a locked or always-loaded
plugin, and free plugins only trade places among free slots. A plugin that loads above a locked
master it cannot get past is reported as blocked: the only fix is to move that plugin down below
the master, which sort does not do. `plugins masterlist` downloads the masterlist, and only when
you ask for it.

Two plugins that change the same record cannot both win: the one that loads later replaces the
other's change. `plugins conflicts` shows the pairs where that happens, with how many records and
an example for each. A record's definer never loses, because overriding it is what a mod is for.
A record the plugins make identically is not a conflict, nor is one the winner patches on purpose
(the other plugin is one of its masters, directly or through another master), nor one covered by
a rule. A pair between the game's own always-loaded files is counted in the summary and never
listed or warned about, because nobody can reorder it. The two actions add the rule that records
your decision, so it survives every later sort:
`--keep <winner> --over <loser>` keeps the plugin that currently wins, and
`--let-win <loser> --over <winner>` also moves the losing plugin after the winner now. Rules are
kept per instance and list by `plugins rule`, and a rule whose plugin leaves the list is kept and
shown as unused, so turning a mod off and on does not lose the decision. `games instance check`
counts the conflicts, and warns when a pair is new since the last launch because a plugin was
added, turned on or moved.

### Downloads

Agora does not use the Nexus Mods or CurseForge APIs. It can notice the mod archives you
download in a browser, but it recognises them by what is inside the archive, never by the file
name. This command makes one classification pass for a folder and prints what Agora would offer
in the desktop app's Library. Nothing is imported, and no file is moved, changed or deleted.

| Command | Purpose |
| --- | --- |
| `agora downloads scan [--folder <path>] [--json]` | Classify the common mod archives (`.zip`, `.7z`, `.rar`, `.jar`, `.mrpack`) in a folder and print each one's game, kind and the reason it was recognised. Browser partials are skipped, and an unrelated archive is left out. An archive Agora reads and **refuses** — a name that leaves it, two entries with one name, a password, a damaged archive — is listed with kind `refused` and the reader's reason; a file that is not an archive at all is left out. Anything already in the content store is marked `already in your library`. The default folder is the one Agora watches, else your Downloads folder |
| `agora downloads list [--json]` | List the new downloads Agora noticed while download watching was on in the app, as the Library shows them. Files that no longer exist are dropped. A download the archive reader refused is listed with kind `refused` and the reader's reason |
| `agora downloads add <ID \| FILE_NAME> [--json]` | Add a noticed download to your library through its game's own flow. `ID` is the start of its SHA-256 as `downloads list` prints it. The file is checked against the hash Agora saw and is only read, never moved or changed. An archive with an installer is stored and named, ready for `games content fomod` |
| `agora downloads ignore <ID \| FILE_NAME>` | Stop offering a download. It is remembered by its contents, so downloading it again does not bring it back |

### Catalog

| Command | Purpose |
| --- | --- |
| `agora registry status` | Inspect the cached catalog and its active state |
| `agora registry sync` | Download and verify the latest signed catalog |
| `agora registry install-bundle <FILE>` | Install a signed catalog bundle from a local file |
| `agora registry item <ITEM>` | Show one catalog item |
| `agora sync` | Convenience alias for catalog synchronization |

Use `--registry-repo` only for an intentional development or sandbox catalog. A different repository changes the trust and governance boundary.

`agora registry install-bundle <FILE>` installs a local `registry.bundle` through the same path a downloaded one takes: the compiled-in keys plus the remembered `registry.trust` state verify the signature and key roles, the signed `built_at` fails closed when it cannot be read and is compared as a UTC instant (an equal stamp with different signed bytes is refused), and an expired or unsupported-schema bundle is refused. There is no override for an older bundle, because the installed stamp is signed. A supplied bundle is judged by its own verified stamp against the floor alone: a missing or unusable staged file, or an installed catalog that no longer verifies, never blocks the repair. A local file is just another source of bytes: the install takes the same cross-process `RegistryUpdate` lock as `agora registry sync`, so the rollback check and the one-rename commit cannot race a concurrent sync. Before any trust statement is written, the verified maximum of the existing floor and the installed bundle's stamp is checkpointed to `registry.floor`; an unreadable or malformed floor refuses the install rather than being treated as no floor. As soon as a candidate verifies, its revocations and keys are recorded in `registry.trust` (the whole log is replaced atomically, one record per bundle) before any bundle file is touched; a failed write refuses the install without touching any bundle file, so no revocation can be lost by a later failure. The order after that is: the verified bytes are staged durably as `registry.bundle.staged`, the previous copy is kept, `registry.floor` is raised to the candidate's stamp, and the staged bytes are published with one rename. The floor therefore never names a catalog whose bytes are not already on disk. An install interrupted after the bytes are staged — at the previous-copy write, the floor write or the rename — is finished by the next open from the staged bytes, never by opening the catalog it replaced. An install interrupted before that point, after its trust record or while staging, has no staged bytes to finish, so a catalog on disk that the record retired cannot be opened: that state needs `agora registry sync` or the bundle file installed again, and the error says which. A staged bundle is published when it verifies under the same trust snapshot and its signed stamp is at or above the floor, which is raised to it first as in the install; one older than the floor is removed as abandoned. A staged file that cannot be used is kept for diagnosis and refuses the open with `ERR_REGISTRY_FLOOR` only when nothing usable at or above the floor is installed; when the installed catalog verifies and satisfies the floor, that catalog is opened and the unusable staged file is reported once as a warning naming it, why it was not used and how to clear it. Finishing or removing a staged file happens under the `RegistryUpdate` lock, taken without waiting, so an open that finds an install running leaves the staged file to it and decides from what is on disk. A catalog the floor is above with no staged bytes to finish it refuses the open and says to sync or install the bundle file again; it is never shown as browsable. If an install fails after a trust append, the trust update is kept and the error says so; if nothing was appended, it says the trust state did not change. A record is either entirely present or entirely absent, and a log that is unreadable, damaged or holds a statement that does not authenticate refuses the install, the open and the status with `ERR_REGISTRY_TRUST_STATE` (remove `registry.trust` to start over). The previous bundle is kept as `registry.bundle.prev`. The highest signed build stamp this client has accepted is persisted in one monotonic `registry.floor`, and a replacement must be newer than the higher of that floor and the installed bundle's stamp when it verifies. A cached bundle below the floor is not opened for use: an open first finishes an interrupted publication from `registry.bundle.staged` when those bytes verify and are at or above the floor, and otherwise refuses with `ERR_REGISTRY_FLOOR`; `agora registry status` shows the same stale state as an expired catalog until then. An installed bundle that no longer verifies (for example its signer was revoked) contributes no trust statements and no stamp, but the floor still applies. Recovery from `registry.bundle.prev` is allowed only when it verifies against compiled and remembered trust and is not below the floor (compared as UTC instants); a below-floor backup is quarantined rather than restored, and an unreadable trust state blocks recovery. A client with an older `registry.trust` and no floor initialises it from the installed bundle when that bundle verifies. The tag named by the bundle's signed header is kept for display in `agora registry status`, but it never decides whether a later sync has an update: that is decided from the signed build stamp, so a future-dated or malformed tag cannot suppress or loop an update. The stored online release asset id is cleared, so the next sync re-checks the remote rather than treating it as a same-day match. With `--json` it prints the tag, schema version, expiry and any catalog warnings. This is meant for offline machines and test setups; only install a bundle whose signature you have a reason to trust.

`agora registry status` reports a cached catalog as ready only when the readable `registry.db` is present and hashes to exactly what the installed signed bundle names. A bundle whose database has not been extracted, or a database that was replaced, shows as no catalog found rather than as a usable catalog; `agora registry sync` reports that same real state even when an identical download skipped extraction. `agora registry sync` does not re-download an asset it already has just because the release tag is a day ahead of the signed build stamp (a release published across UTC midnight); a candidate with the same signed stamp and identical bytes is reported up to date and its asset id is recorded instead of being reinstalled as an update.

### Registry review and governance

These commands only read, except `registry vote`, which changes only your own vote.

| Command | Purpose |
| --- | --- |
| `agora registry under-review` | List registry items under review, lowest score first |
| `agora registry reviews <ITEM>` | Show the curator reviews recorded for one item |
| `agora registry audit-log [--limit N]` | Show recent audit log entries, newest first (1-1000, default 200) |
| `agora registry resolutions [--limit N]` | Show recent triage resolutions (1-1000, default 50) |
| `agora registry vote <ITEM> [up \| down \| clear]` | Show your vote on a curated item, or vote up or down, or take your vote back. A vote is your own reaction on the item's GitHub voting issue, so it needs `agora auth github login`. Curator-protected items do not take votes. Covered by Lockdown Mode |
| `agora governance summary <ITEM>` | Show the vote counts and status for one item |
| `agora governance events [--item <ITEM>] [--limit N]` | List governance events, newest first (1-1000, default 100) |
| `agora governance config` | Show the governance repository and environment this build uses |
| `agora governance diagnostics` | Run the governance setup checks, local and GitHub |

These queries read the installed registry, so they fail with an error when none is installed; run `agora registry sync` first. A registry database that cannot be read is an error too, never an empty list. An empty list means there is genuinely nothing to show. `governance diagnostics` prints one line per check and exits with status 1 when any check fails. Its GitHub checks are read-only requests. When GitHub is unreachable or you are not signed in, those checks report their own failure rather than passing.

### Instances

```text
agora instance create
agora instance clone
agora instance rename
agora instance lock
agora instance unlock
agora instance repair-loader
agora instance recommend-memory
agora instance delete
agora instance deleted
agora instance restore
agora instance purge
agora instance health <INSTANCE_ID> | --all
agora instance revert <INSTANCE_ID>
agora instance loader-version <INSTANCE_ID> <VERSION> [--yes]
agora instance java <INSTANCE_ID> [--path <FILE> | --clear-path] [--memory <MB>] [--jvm-args <ARGS>]
agora instance wrapper <INSTANCE_ID> [<COMMAND> | --clear]
agora instance window <INSTANCE_ID> [<WIDTH>x<HEIGHT> | --clear]
agora instance hooks <INSTANCE_ID> [--pre <COMMAND>] [--post <COMMAND>] [--env NAME=VALUE ...] [--clear]
agora instance notes <INSTANCE_ID> [<TEXT> | --clear]
agora instance group <INSTANCE_ID> [<NAME> | --clear]
agora instance share <INSTANCE_ID> [--options on|off] [--servers on|off]
agora instance migrate <INSTANCE_ID> <MC_VERSION> [--dry-run] [--accept-blockers]
agora instance history <INSTANCE_ID>
agora instance stop <INSTANCE_ID>
agora instance snapshot-versions
```

Examples:

```bash
agora instance recommend-memory <INSTANCE_ID>
agora instance lock <INSTANCE_ID>
agora instance clone <SOURCE_ID> "Debug Copy" --no-saves
agora instance health --all
agora instance java <INSTANCE_ID> --memory 6144 --jvm-args "-Dexample.flag=true"
agora instance wrapper <INSTANCE_ID> "mangohud --dlsym"
```

`instance create` can set Java up front: `--jvm-memory-mb <MB>` gives the heap a fixed size and makes the memory choice manual, `--jvm-gc` picks `auto` (the default), `low_latency`, `high_efficiency` or `manual`, `--jvm-custom-args "<ARGS>"` adds raw JVM flags, and `--jvm-always-pre-touch true|false` switches heap pre-touching. `--template <TEMPLATE_ID>` starts the instance from a saved template's files and JVM settings (see [Templates](#templates)).

Fabric and Quilt instance creation checks the generated client profile and verifies every library against its Maven repository's strongest published checksum. Run `agora instance repair-loader <ID>` to update an older installation that still has a profile-byte receipt. Both commands accept `--allow-unverified-loader` when a repository publishes no checksum for a library. Agora names each such file on stderr. A mismatch or invalid profile stops the operation. Forge and NeoForge installs also pin the jars their installers write — Forge's patched client jar, and NeoForge's, which its profile never lists — and a launch checks them against the receipt, refusing a truncated or altered file by name. That pin is the file as the installer left it, so a jar already truncated when the receipt is written becomes the baseline, and an existing install gains the check only after a reinstall or `agora instance repair-loader` rewrites its receipt.

Clone flags can omit saves (`--no-saves`), mods (`--no-mods`), resource packs (`--no-resource-packs`), shader packs (`--no-shader-packs`), screenshots (`--no-screenshots`), configuration (`--no-config`), `servers.dat` (`--no-servers`), or options (`--no-options`). `--hard-links` and `--sym-links` change filesystem behavior and should be used only when the destination and source relationship is understood.

Locking prevents ordinary content mutation. Unlock explicitly before applying intended changes.

Deleted instances wait in Recently deleted for 30 days, or for the time set in the desktop app's Settings > General > Storage (the `games_recently_deleted_days` setting, shared by every game). `instance deleted` lists them with their entry ID (the ID to pass to `restore` and `purge`, which is not the instance ID), name, deletion time, and size. `instance restore` brings one back with its files and records; if its original ID has been taken, it returns under a new one and says so. `instance purge` sends one to the OS trash now and asks first unless you pass `--yes`; Agora cannot restore it afterwards, though the OS trash may still hold the files. Agora also runs the same cleanup when an instance command starts, so expired entries reach the trash without the desktop app open.

```bash
agora --json instance deleted
agora instance restore <ENTRY_ID>
agora instance purge <ENTRY_ID> --yes
```

`instance health` runs the same cached health check the desktop app uses. With `--all` it checks every instance, prints the whole table, and then exits nonzero if any instance is red or cannot be read. `instance revert` returns an unlocked instance to the snapshot taken when it was last locked, and takes an undo point first. It refuses an instance that was never locked.

`instance loader-version` shows the current loader, the compatible versions for the enabled mods, and the recommendation, then asks before switching. A version outside the compatible list is refused before anything changes. `--yes` skips the question.

`instance java` with no options shows the current settings. `--path` must name an existing file, and `--clear-path` goes back to the global Java setting. `--memory` is in MB and is kept as given; only zero, a negative value, or one the JVM cannot represent is refused. Agora warns below 2048 MB (Minecraft will likely run out of memory) and above the machine's memory. Setting memory makes the choice manual, as creation does. `--jvm-args` replaces the instance's arguments; a heap flag there (`-Xmx`, `-Xms`, or a RAM percentage) replaces Agora's matching memory flag.

`instance wrapper` sets the command the game is launched under, for example a frame-timing overlay. It is checked the way the launcher reads it, so an unmatched quote is refused. An empty command or `--clear` removes it.

`instance window` sets the size the game window opens at, for example `1280x720`; each side must be between 200 and 16384. `--clear` goes back to the global default (`agora_window_size` in settings), and without either the game picks its own size. It applies to direct launch; the official launcher keeps the resolution set on its own installation.

`instance hooks` sets a command run in the instance folder before a direct launch (if it fails, the game is not started), a command run after the game exits, and environment variables for the game and both commands. Commands are split on spaces with quotes respected; no shell is used. Both get `INST_NAME`, `INST_ID`, `INST_DIR`, `INST_MC_DIR` and `INST_JAVA`, the same variables Prism Launcher provides. An option replaces only what it names; `--env` replaces all variables; `--clear` removes everything.

`instance migrate` moves an instance to another Minecraft version: each mod is swapped for a build that fits, and the loader is reinstalled for the new version. `--dry-run` shows the plan and changes nothing. Mods with no fitting build stay at their current version; the command names them and stops unless you pass `--accept-blockers`. It takes a snapshot first and rolls back if anything fails.

`instance history` lists the instance's recent launches (when, how each ended, how long it ran, how many mods were on) with a run and crash count. `instance stop` stops the game an Agora launch is running for the instance, from any terminal: it finds the game through the record the launch keeps, and checks the process is still that game before stopping it, so a reused process number is never stopped. A launch that never finished starting, or one handed to the official launcher, has no such record and cannot be stopped this way. `instance snapshot-versions` lists the Minecraft snapshot and pre-release versions a vanilla instance can use (it reads Mojang's version list).

`instance notes` keeps free-form text with the instance, in plain text in its manifest. `instance group` files the instance under a group on My Instances. `instance share` keeps game settings (`options.txt`, except resource packs and other per-instance keys) and the server list the same across every instance that shares them: the shared copy is written in before each launch and taken back when the game closes. The first instance to share provides the shared copy.

### Worlds and servers

```text
agora world list <INSTANCE_ID>
agora world rename <INSTANCE_ID> <WORLD_FOLDER> <NAME> [--while-running]
agora world copy <INSTANCE_ID> <WORLD_FOLDER> [--while-running]
agora world delete <INSTANCE_ID> <WORLD_FOLDER> [--yes] [--while-running]
agora world export <INSTANCE_ID> <WORLD_FOLDER> --out <FILE.zip> [--while-running]
agora world import <INSTANCE_ID> <FILE.zip> [--while-running]
agora server list <INSTANCE_ID> [--ping]
agora server add <INSTANCE_ID> <ADDRESS> [--name <NAME>] [--while-running]
agora server remove <INSTANCE_ID> <POSITION> [--while-running]
agora server edit <INSTANCE_ID> <POSITION | NAME> [--name <NAME>] [--address <ADDRESS>] [--while-running]
agora server move <INSTANCE_ID> <POSITION | NAME> up|down [--while-running]
agora server ping <ADDRESS>
```

A world is named by its folder under `saves/`, which `world list` shows. `world rename` changes the name shown in game and keeps the folder, so data pack choices and `launch --world` keep pointing at it. `world delete` moves the world to the system recycle bin and asks first unless you pass `--yes`. `world import` accepts a zip holding the world at its root or inside one folder, and picks a free folder name. A world action is refused while the instance's game is running or starting, including a delegated session through the official launcher; `--while-running` is the explicit override, and the instance lock is still taken. The running game may overwrite the world when it saves.

`server` edits the game's own multiplayer list (`servers.dat`), keeping the icons and other details the game stored. Positions start at 1, in the order `server list` prints. `--ping` also asks each server whether it is online, with its player count and latency, using the game's own status protocol (Minecraft 1.7 and later). Servers reachable only through a DNS `SRV` record show as offline. Pinging is covered by Lockdown Mode and the Server status switch on the Privacy page.

`server edit` renames a server or changes its address; whatever you leave out stays as it is. `server move` swaps a server with the one above or below it. `server ping` checks any address without adding it to a list.

### Logs, proxy and skins

```text
agora log list <INSTANCE_ID>
agora log share <INSTANCE_ID> [--path logs/latest.log] [--preview] [--yes]
agora proxy show
agora proxy system
agora proxy none
agora proxy set <URL> [--username <NAME>]
agora skin show
agora skin wear <FILE.png> [--slim]
agora skin reset
agora skin cape <NAME | none>
agora skin library list
agora skin library add <FILE.png> [--name <NAME>] [--slim]
agora skin library remove <ID>
```

`log share` uploads the latest game log or a recent crash report to mclo.gs and prints the link. User folder names, anything shaped like a token and the signed-in account's own token are removed first; `--preview` prints exactly what would be sent. It asks before uploading unless `--yes`, and is covered by Lockdown Mode and the Log sharing switch.

`proxy` sets how Agora's own requests reach the internet: `system` uses the `HTTPS_PROXY`, `HTTP_PROXY` and `ALL_PROXY` environment variables (the default), `none` connects directly, and `set` takes an `http`, `https`, `socks5` or `socks5h` URL. A password is read from the `AGORA_PROXY_PASSWORD` environment variable and kept in the operating-system credential store. The change applies the next time Agora starts. Minecraft itself does not use this proxy.

`skin` changes the active Microsoft account's skin and cape, as changing them on minecraft.net would. A skin must be a 64×64 (or 64×32) PNG.

`skin library` keeps skins on this computer, the same list as the app's skin page; it needs no sign-in and uploads nothing. `add` names the skin after its file unless you pass `--name`. In `--json`, each saved skin includes the image as a data URL.

All of these refuse to run while the game has the instance open, because Minecraft rewrites both files when it closes.

### Mods and content

```text
agora mod list <INSTANCE>
agora mod search <QUERY>
agora mod install <PROJECT> <INSTANCE>
agora mod remove <PROJECT> <INSTANCE>
agora mod update <INSTANCE> <ITEM>
agora mod update-all <INSTANCE>
agora mod pin <INSTANCE> <FILE>
agora mod unpin <INSTANCE> <FILE>
agora mod changelog <INSTANCE> <FILE>
agora mod enable <INSTANCE> <FILE> [--with-deps]
agora mod disable <INSTANCE> <FILE>
agora mod worlds <INSTANCE> <FILE> --worlds <A,B> | --all
agora mod sync-datapacks <INSTANCE>
agora mod graph <INSTANCE>
agora mod orphans <INSTANCE>
agora mod why <INSTANCE> <FILE>
agora mod disable-plan <INSTANCE> <FILE>
agora mod versions <ITEM> [--instance <INSTANCE>] [--limit N] [--offset N]
agora mod versions <PROJECT_ID> --modrinth [--project-type <TYPE>] [--instance <INSTANCE>]
agora mod add-stored <INSTANCE> <ITEM_ID>
```

Examples:

```bash
agora mod graph <INSTANCE_ID>
agora mod why <INSTANCE_ID> <FILE>
agora mod enable <INSTANCE_ID> <FILE> --with-deps
```

Common planning options:

| Option | Meaning |
| --- | --- |
| `--dry-run` | Resolve and print the plan without executing |
| `--include-optional <A,B>` | Include named optional dependencies |
| `--exclude-optional` | Exclude all optional dependencies |
| `--replace-conflicts` | Choose replacement for every resolvable conflict |
| `--abort-conflicts` | Abort when any conflict remains |
| `--remove-anyway` | `mod remove` only: remove the file even though an installed mod still requires it |
| `--allow-replace` | Permit replacement of existing files |
| `--skip-health-scan` | Skip the post-operation health gate |
| `--while-running` | `mod install`, `mod remove`, `mod update`, `mod update-all` and `instance updates --apply`: change the instance even while its game is running. The running game keeps the files it already opened; these commands still take the instance lock |

`--replace-conflicts` is broad. Review a dry-run first, especially on an established instance.

`mod remove` stops when the file is a required dependency of another installed mod. `--remove-anyway` accepts that for the file being removed (and nothing else), prints a warning naming the mods that lose their dependency, and leaves the instance with a health alert until the dependency is restored. It cannot be combined with `--abort-conflicts`.

The default source is Agora's curated strategy. Use `--source modrinth` only when the optional Modrinth integration and its network permissions are enabled.

`mod enable` and `mod disable` fail with a clear error and a nonzero exit when the named file does not exist. Confirm the filename with `mod list` if the error is unexpected.

`mod graph` lists every dependency between installed mods that are switched on. `mod orphans` lists dependency-installed mods that no user-installed mod still needs. `mod why` traces one file back to the mod that asked for it. `mod disable-plan` shows which installed mods declare a dependency on a file, so you can see what switching it off would affect; it changes nothing. These four only read the instance.

`mod enable --with-deps` also re-enables the required dependencies of the file that are switched off. It is best-effort, as in the desktop app: a dependency that cannot be re-enabled is reported as a warning, and the file itself is still enabled.

`mod versions` lists a catalog item's versions, newest first, 25 at a time (`--offset` pages on). With `--instance` each version says whether it fits that instance. With `--modrinth`, the argument is a Modrinth project ID and Modrinth is asked directly; that needs the Modrinth integration turned on. `mod add-stored` adds an item already in the content store (see `agora games content list`) to an instance without downloading anything.

Minecraft only loads data packs from inside a world, so Agora copies enabled data packs into the instance's worlds before each launch and after installs and removals. `mod worlds` limits one data pack to the named world folders (or `--all` to go back to every world), and `mod sync-datapacks` runs the copy now, for example for a world created since the last launch. Agora only removes or replaces data pack files it placed itself.

### Packs, import, and export

| Command | Purpose |
| --- | --- |
| `agora import <PATH>` | Import a supported local pack |
| `agora import --url <MRPACK_URL>` | Download and import a Modrinth pack URL |
| `agora pack install <PATH> <INSTANCE>` | Install an Agora pack manifest into an existing instance |
| `agora pack versions <PACK>` | List a curated registry pack's locked releases, newest first |
| `agora pack curated <PACK> <INSTANCE> --release <VERSION>` | Install a locked release (the instance must be on its Minecraft version and loader) |
| `agora pack curated <PACK> <INSTANCE>` | Install the pack's flexible recipe against the instance's own Minecraft version |
| `agora pack export <INSTANCE> --format mrpack\|agora\|lockfile --out <FILE>` | Export an instance as a Modrinth pack, an Agora pack, or a lockfile |
| `agora pack review <FILE>` | Show what importing a `.mrpack` or `.agora-pack.json` would do and stage the reviewed copy; prints a review id |
| `agora pack import <REVIEW_ID> [--name <NAME>] [--import-anyway] [--install-anyway]` | Import exactly the copy `pack review` staged |
| `agora pack discard <REVIEW_ID>` | Throw away the copy `pack review` staged, when you decide not to import it |
| `agora pack retry <INSTANCE> <PACK_FILE> [--entry <FILENAME>]... [--allow-modrinth-once] [--install-anyway]` | Try an Agora pack's mods again on the instance its import made (after turning Modrinth on, say); mods already installed are skipped |
| `agora pack mods <PACK>` | List the mods in a catalog pack |
| `agora pack extract-overrides <INSTANCE> <ZIP> [--without-unwritable-names]` | Extract a pack's overrides zip into an instance with the import's safety checks. An entry whose name a disk cannot hold stops the extraction and the refusal names each one and why: a Windows reserved name (`config/aux.dds`), a trailing dot or space, a `:`, or two entries a case-insensitive disk stores as one file. `--without-unwritable-names` is the explicit answer to that refusal: the entries that can be written are, and the output lists every entry that was not, with the reason. Nothing is ever dropped without its name and reason |
| `agora pack install-catalog <ITEM> [--name <NAME>] [--accept-changed] [--allow-unverified]` | Install a catalog entry that pins a provider's modpack, as a new instance |
| `agora import launchers [--root <DIR>]` | List Prism, CurseForge and Modrinth instances that can be imported |
| `agora import plan <KEY>... [--root <DIR>] [--name <NAME>] [--no-settings]` | Show what importing launcher instances would copy; prints a plan fingerprint |
| `agora import run <KEY>... [--root <DIR>] [--name <NAME>] [--no-settings] [--fingerprint <HASH>]` | Import launcher instances (items commit independently) |
| `agora backup export <INSTANCE> --out <DIR> [--snapshot <ID>] [--with-bytes]` | Write a snapshot (a new one unless `--snapshot` is given) to a folder as one zip. For a game instance the backup always holds the instance's state and each content item's hash and source; `--with-bytes` also includes the mod bytes, and prints their size first |
| `agora backup import <FILE> [--instance <INSTANCE>] [--fetch-missing]` | Read a backup zip into an instance as a restorable snapshot. Every hash is checked before anything is written; a game backup without its mod bytes is refused, naming every missing item, unless those objects already exist. `--fetch-missing` fetches them first through the same hash-checked catalog path an install uses, after printing the item count and total size; no raw HTTP request is made, and the network settings and Lockdown Mode apply. If any item cannot be fetched the whole import refuses before anything is applied, and fetched objects stay in the store for a retry. A game backup imported into an empty data folder creates the instance's row and manifest through the normal core registration path |
| `agora backup prune <INSTANCE> [--keep <N>] [--yes]` | Delete snapshots beyond the newest N (default 10) |
| `agora export <INSTANCE> <DEST> [--world <FOLDER>] [--allow-unverified-loader]` | Export a standalone, ready-to-run server folder |
| `agora pack update-preview <INSTANCE> <MRPACK>` | Show what a newer .mrpack would change. Changes nothing |
| `agora pack update <INSTANCE> <MRPACK> [--while-running]` | Apply a newer .mrpack; asks for `--yes` and an answer to every conflict |
| `agora migrate-data --from <PATH>` | Plan migration from an older CLI data root |
| `agora migrate-data --from <PATH> --yes` | Execute the migration |

`pack export` warns when an Agora pack would leave out mods that have no catalog or Modrinth source (a `.mrpack` bundles them). It asks first, or pass `--yes` to export anyway. `pack import` refuses a pack with hard problems whatever the flags; files that cannot be fetched stop it unless `--import-anyway`, which leaves exactly those out. An Agora pack lists a SHA-256 per mod. A download is checked against the hash its source published first; when the source published none, the pack's hash is used instead and the result reads "Matches the hash in the modpack", never "verified" by the source. The catalog's own hash for a mod (its curator pin) is held the same way when the source published nothing, and is labelled as the curator's. A download that differs from either, or a pack hash entry that is malformed, is not installed: `pack import` prints the file, the expected hash and the downloaded hash, and keeps the reviewed copy. `--install-anyway` installs those mods regardless (a hash the source itself published is still enforced); run it with `--name` to make a second instance, since the first one exists. When more than one expectation is unmet for a file (the pack's hash and the catalog's pin), the output names every one, because `--install-anyway` overrides all of them. A `.mrpack` is checked against both the SHA-512 and the SHA-1 it lists for each file; a file with a wrong, malformed or missing hash stops the whole import with every such file named, and `pack import <id> --install-anyway` imports it as it is. The desktop app offers Install anyway for the same files. A launcher key that two launchers share is written `prism:KEY`, `curseforge:KEY` or `modrinth:KEY`. A backup import never changes an existing instance's live files; restore it with `snapshots restore`. Importing a game backup into an empty data folder registers the instance (its row and manifest) so it can be listed and opened, but still leaves applying the captured state to `snapshots restore`. `backup prune` deletes snapshots and, without a terminal, needs `--yes`.

`pack retry` reads the mods from the `.agora-pack.json` file again, so keep the file. `--allow-modrinth-once` lets just these mods come from Modrinth without turning the Modrinth setting on, and `--install-anyway` installs mods that differ from the hash the pack lists, after you have checked them. `pack install-catalog` refuses when the provider's pack no longer matches what the catalog pinned; `--accept-changed` installs it as it is now. `--allow-unverified` works as it does for `pack install --provider`.

`agora export` writes a folder the player can start with `start.bat` (Windows) or `./start.sh` (Linux/macOS). It copies the mods whose own metadata does not declare them client-only, `config/`, and one world. A mod's side is read from its own declaration: Fabric's `fabric.mod.json` `environment`, Quilt's `quilt.mod.json` environment, and the mod's own `side` in Forge's `mods.toml` or NeoForge's `neoforge.mods.toml`; a dependency's `side` never speaks for the mod. A mod whose side cannot be read is kept and named. The world defaults to the most recently played; `--world` names one, and an unknown name is an error. The loader for the instance's exact Minecraft and loader version comes from the pinned loader catalog and is checked against its hash. Forge and NeoForge installers are verified and run with `--installServer` on first start. Fabric and Quilt download their official installer from the loader's Maven repository (latest `<release>` in its `maven-metadata.xml`), checked against the `.sha512` that repository publishes beside it, and the start script runs that installer in server mode on first start, then starts the launcher jar it wrote. `--allow-unverified-loader` is only for the case where no hash file is published at all; it never overrides a mismatch. The scripts use the instance's memory setting and name the required Java version, and `eula.txt` is written with `eula=false` — Agora never accepts the Minecraft EULA for the player.

The installer commands are the installers' own documented CLI. Fabric: `server -dir . -mcversion <MC> -loader <LOADER> -downloadMinecraft` (`https://github.com/FabricMC/fabric-installer`, `ServerHandler.java` lines 69-94; the handler name is matched in `Main.java` lines 86-89; it writes `fabric-server-launch.jar`, `ServerInstaller.java` line 61). Quilt: `install server <MC> <LOADER> --install-dir=. --download-server` (`https://github.com/QuiltMC/quilt-installer`, `CliInstaller.java` lines 193-275; it writes `quilt-server-launch.jar`, `InstallServer.java` lines 67-70, 109 and 126-129).

`migrate-data` is a dry-run unless `--yes` is supplied. Read every reported conflict before executing it.

`pack curated` plans first and installs nothing if a mod marked `required` has no build for the target; recommended and optional mods without one are listed as left out. Add `--dry-run` to print the plan without installing. Optional dependencies of the pack's mods are not added, because a CLI run cannot answer that prompt.

`--symlink-saves` makes imported saves depend on the original path. It is not a copy or backup.

### Snapshots

```text
agora snapshots list <INSTANCE>
agora snapshots create <INSTANCE> --label "Before update"
agora snapshots restore <INSTANCE> <SNAPSHOT_ID> [--force]
agora snapshots delete <INSTANCE> <SNAPSHOT_ID>
agora snapshots drift <INSTANCE> <SNAPSHOT_ID>
agora snapshots lkg <INSTANCE>
agora snapshots lkg-diff <INSTANCE>
agora snapshots restore-lkg <INSTANCE> [--force]
```

`snapshots drift` lists the files added, removed and changed since a snapshot, and changes nothing.

`snapshots delete` removes the snapshot's own records and then runs one collection over what nothing names any more. It prints what that collection reclaimed, or that nothing was reclaimed and why (an object written less than an hour ago is kept); `--json` carries the same answer in `collection` (`summary`, `reclaimed`, `refused`), beside `status`, `instanceId` and `snapshotId`.

Snapshot deletion and content-store removal check both live and Recently deleted instances before
removing data. A user-requested snapshot delete waits a few seconds for an instance move or scan to
finish; if the lock is still busy, it refuses and you can retry. Automatic retention skips its round
instead of reporting an error when the lock is busy, and deletes nothing that round.
Instance delete, restore, and purge can also report that the lock is busy.

A restore replaces tracked instance state. Preserve valuable worlds separately and verify the intended snapshot ID before restoring. Every restore takes a pre-restore snapshot first, so the restore itself can be undone by restoring that `pre-restore-…` entry.

A snapshot owns what it captured. When a snapshot covers a folder the game's package no longer declares as snapshot state, `snapshots restore` and `snapshots restore-lkg` restore it anyway — by the same mechanism as any tracked folder, so the folder comes back as the snapshot recorded it — and print a `Note:` line naming the folder and the game, so state that came back from outside the current scope is never silent. Files the game created in that folder since the snapshot are replaced like in any tracked folder and are kept in the pre-restore snapshot the restore takes first, so undoing the restore brings them back. The desktop app's Recovery tab, the instance editor and the Crash Doctor say the same thing after a restore, and `--output json` carries the lines in the result's `notices` array. What the snapshot captured is compared with what the package declares today, kind by kind and by path components with the platform's case rule: a captured folder is still tracked when a declared folder is it or holds it (a path mapped inside it does not count, so a captured `user` is reported when only `user/Plugins.txt` is declared), and a captured file is still tracked when the declaration names exactly that file, never because the folder around it is declared. A game instance's snapshot therefore records a declared folder as a folder and a file the game definition maps (`user/Plugins.txt`) as a file, so mapping one file no longer reports the folder around it, and the undo snapshot the restore takes first covers exactly that record — files beside a mapped one are left alone. A restored file is written through the instance's own folder and the launcher refuses to write through a folder a link has replaced since the capture.

Agora's automatic pre-launch recovery snapshot is optimized for mod, configuration, and layout recovery and does not include `saves/`. Full manual and transactional snapshots use a broader scope, but no local snapshot system should be the only backup for an irreplaceable world.

`snapshots lkg` prints the snapshot that last launched well and how the last launch ended. `snapshots lkg-diff` lists the files that changed since it (a reordered plugin list and a changed INI each appear as a modified state file) and, for a game instance, the content items and load-after lists that changed. `snapshots restore-lkg` restores that snapshot through the normal guarded restore, taking an undo snapshot first. A launch promotes its pre-launch snapshot only when the session ends successfully — a clean exit after the minimum runtime with no crash evidence. A crash, a cancelled run, an unknown ending, or a run too short to count never promotes.

The same commands work for a game instance (§26). Its snapshot holds the instance's own state — the manifest (content layers, save choice), the plugin lists and INI copies the game definition declares, the `.agora_loadouts` store, and the load-after rules — while mod files stay once in the content store, so a snapshot is small. A restore writes those files atomically, restores every list's rules in one database transaction, and marks the deployed folder for a rebuild at the next launch. It refuses while the game is running or cannot be proven idle; `--force` restores anyway, after the warning that the running game may overwrite it. A content item the snapshot names but the store no longer holds refuses the restore, naming the item, before anything changes. Content-store removal refuses while a game-instance snapshot still names the item; the refusal names the snapshot (instance, label and time) and the exact `agora snapshots delete <instance> <id>` command. An unreadable snapshot refuses too, naming it and its command; `agora games content remove --ignore-unreadable-snapshots` is the explicit override, and it names each snapshot it ignored on stderr. `snapshots list` shows a damaged snapshot as unreadable with the reason instead of hiding it, and keeps it out of automatic retention so it can be inspected or deleted by hand.

A game-instance launch takes its pre-launch snapshot after deployment has settled the state and skips it when the state's content hash is unchanged since the last snapshot. When the game closes, the session ledger's verdict is mapped to the LaunchOutcome classes in §23.2 and only a success promotes the snapshot to Last Known Good. Retention is the existing policy, so the current Last Known Good is evicted last.

### Loadouts

```text
agora loadout create <INSTANCE> <NAME>
agora loadout list <INSTANCE>
agora loadout apply <INSTANCE> <NAME> [--others keep|off] [--while-running]
agora loadout update <INSTANCE> <NAME> [--yes]
agora loadout delete <INSTANCE> <NAME>
agora loadout unlisted <INSTANCE> <NAME>
```

`loadout unlisted` lists the mods that are on but that the loadout does not mention: the ones `loadout apply --others` decides about.

`loadout list` lists every record it can read and names every record it cannot, on an `Unreadable:` line (on stderr in `--json` mode, where stdout stays an array of profiles). A record is read only when its `schema_version` is exactly the integer this build writes; a version that is a string, a different number, a missing required field, a record cut short, or a section filed under another game is a damaged record. It stays on disk, is never listed as an empty loadout, and is refused for `apply` and `update` — an empty one would read as "turn everything off". Records written before the version field existed keep reading leniently.

A loadout records the on/off state of mods, resource packs, shader packs, data packs and worlds. Applying one sets each item it records to that state. An enabled mod it does not list (one added since it was saved, say) is a choice: `--others keep` leaves it on and `--others off` switches it off. When there are such mods and no `--others` is given, apply refuses and names them, so nothing changes. Packs and worlds it does not list are left as they are. Apply refuses a locked instance, and an instance whose game is running or starting, and changes nothing in either case. `--while-running` is the explicit override for the running-game check; the instance lock is still taken, and the running game may overwrite settings when it exits. Before switching, apply saves an undo point labelled `before-loadout` (only the newest is kept). `loadout update` saves a profile again from the instance's current setup, so a mod added since it was saved stops being unlisted. It lists what would change (added, turned on or off, or dropped because it is gone) and asks first on an interactive terminal. Without a terminal it changes nothing and names the changes; pass `--yes` once the person using this computer has agreed. It does not touch the instance, so a locked instance or a running game does not block it.

For a game instance, the same commands work. Its loadout records the content items that are on, their layer order, and, where the game has them, the plugin list (active set and order), the instance's INI copies, the load-after rules and the save choice (with its scoped save folder). Applying it restores all of them after saving the setup it replaced as the undo point, and is refused while the game runs unless `--while-running` says otherwise. A content item the profile does not name is switched off, as an unlisted mod is on Minecraft. Apply prints that the undo point was saved. For a game instance, applying also takes a `before-loadout` snapshot, which appears in `agora snapshots list` alongside the restore points; for a Minecraft instance the undo point is that snapshot.

A loadout does not revert mod versions or configuration files.

### Templates

```text
agora template list
agora template create <NAME> [--from <INSTANCE> --include <FILE>...] [--memory <MB>] [--gc <GC>] [--java-path <FILE>] [--jvm-args <ARGS>] [--description <TEXT>]
agora template apply <TEMPLATE_ID> <INSTANCE>
agora template update <TEMPLATE_ID> [--name <NAME>] [--description <TEXT> | --clear-description] [--clear-jvm] [--memory <MB>] [--gc <GC>] [--java-path <FILE>] [--jvm-args <ARGS>]
agora template delete <TEMPLATE_ID> [--yes]
```

Examples:

```bash
agora template create "Sodium options" --from <INSTANCE_ID> --include config/sodium-options.json
agora template create "Big heap" --memory 8192 --gc low_latency
agora template apply <TEMPLATE_ID> <INSTANCE_ID>
```

A template keeps chosen configuration files and JVM settings so you can copy them to another instance. Only the files named with `--include` are captured, by path relative to the source instance; `--include` needs `--from`. A template with no files and no JVM setting is refused. Applying a template copies its files over the same paths in the target instance, and takes an undo point first when it copies any. A JVM setting the template does not name is left alone. `template update` changes only what you pass; its JVM flags change the settings they name and keep the others, and `--clear-jvm` drops them all first. It does not change the captured files. Deleting a template asks first; without a terminal, pass `--yes`.

### Mod bisect

```text
agora bisect start <INSTANCE> [--suspect <FILENAME>]
agora bisect status <INSTANCE>
agora bisect apply <INSTANCE>
agora bisect good <INSTANCE>
agora bisect bad <INSTANCE>
agora bisect back <INSTANCE>
agora bisect cancel <INSTANCE>
agora bisect run <INSTANCE> [--suspect <FILENAME>]
```

Bisect finds the mod that breaks a launch by turning half of the enabled mods off for each round. `apply` switches the round's mods off and leaves the others on. Mods that depend on a switched-off mod go off with it, so a missing library is not mistaken for the culprit. You launch the game yourself, then `good` (the problem did not happen) or `bad` (it did) narrows the suspects. `back` undoes the last answer and tries the other half. `cancel` ends the bisect and puts every mod back the way it was, at any point. `--suspect` names a mod the crash report implicated and tests it first; it only applies when starting. `run` walks through the same rounds with prompts and needs an interactive terminal. A bisect does not start the game itself; you launch it. The session is kept in the instance folder, so it survives closing the terminal. `start`, `status`, `good`, `bad` and `back` do not move any mod files; `apply` and `cancel` do. `start` and `apply` refuse a locked instance, and `start`, `apply` and `cancel` refuse a game that is running or starting, changing nothing. `cancel` still restores a locked instance, because putting the baseline back is the way out. If `run` is stopped with Ctrl+C, the mods stay as the current round left them until `agora bisect cancel` is run.


### Reproduction lockfiles

```text
agora lockfile export <INSTANCE> --out instance.lock.json
agora lockfile verify instance.lock.json
agora lockfile repair <INSTANCE> --out repaired.lock.json
agora lockfile import instance.lock.json <INSTANCE>
```

A lockfile describes reproducible artifact state. It is not a backup of private world or configuration contents.

Review a lockfile's source before importing it. Hashes prove byte identity, not safety or licensing.

A lockfile exported from a vanilla instance passes `lockfile verify` immediately; export-then-verify is a valid round trip for vanilla and modded instances alike.

### Java runtimes

```text
agora runtime list
agora runtime inspect <JAVA_PATH>
agora runtime ensure <MAJOR>
agora runtime remove-unused
```

Examples:

```bash
agora runtime ensure 21
agora runtime inspect "C:\Program Files\Java\jdk-21\bin\java.exe"
```

Use `instance recommend-memory` to inspect Agora's memory estimate without changing settings.

### Loader profiles

```text
agora loader list
agora loader list --mc-version 1.21.1
agora loader install <LOADER> <MC_VERSION> <LOADER_VERSION>
```

Loader installation is restricted to Agora's pinned catalog. `--force` reinstalls a verified profile.

Instance health can identify enabled mods whose loader-version requirements are not satisfied. The desktop app provides the richest interactive candidate-selection flow; the CLI should be used with explicit versions and a follow-up health check.

### Community plugins

```text
agora plugin list
agora plugin status                         # alias for list
agora plugin preview package <PATH>
agora plugin preview development <PATH>
agora plugin validate <PATH>
agora plugin install package <PATH> [--yes] [--no-code]
agora plugin install development <PATH> [--yes] [--no-code]
agora plugin scripts <PLUGIN_ID> allow|deny
agora plugin stop
agora plugin resume
agora plugin enable <PLUGIN_ID>
agora plugin disable <PLUGIN_ID>
agora plugin remove <PLUGIN_ID>
agora plugin remove <PLUGIN_ID> --purge-data
agora plugin restore-data <PLUGIN_ID> [--yes]
agora plugin recover [PLUGIN_ID]
agora plugin recover <PLUGIN_ID> --restore --yes
agora plugin recover <PLUGIN_ID> --discard --yes
agora plugin recover <PLUGIN_ID> --keep-aside --yes
agora plugin log <PLUGIN_ID> --lines 200
agora plugin disable-all
agora plugin run <PLUGIN_ID> <COMMAND> [--args <JSON>]
agora plugin diagnose <PLUGIN_ID> <DIAGNOSTIC> <INSTANCE_ID>
agora plugin repair <PLUGIN_ID> <DIAGNOSTIC> <INSTANCE_ID> <FIX> [--yes]
agora plugin launch-checks <INSTANCE_ID>
agora plugin settings <PLUGIN_ID>
agora plugin set <PLUGIN_ID> <KEY> <VALUE>
agora plugin check-update [PLUGIN_ID]
agora plugin update <PLUGIN_ID>
agora plugin update <PLUGIN_ID> --yes
agora plugin update <PLUGIN_ID> --allow-scripts
agora plugin keygen --out <PATH> --key-id <ID>
agora plugin sign <DOCUMENT> --key <PATH> --key-id <ID>
```

`plugin settings` prints the settings a plugin declares, with their types and current values. `plugin set` writes one of them. The value is read as JSON (`true`, `42`, `"text"`); text that is not valid JSON is taken as a plain string. Core checks the value against the type the plugin declared and refuses a wrong type or an undeclared key, so nothing is stored in that case.

`plugin run`, `plugin diagnose`, `plugin repair` and `plugin launch-checks` do what the app does when you use a plugin's command, check or fix. They run plugin code, so the same rules apply: the plugin must be enabled, its code allowed, and the emergency stop released, and it can only use the permissions it was granted. `COMMAND` and `DIAGNOSTIC` are export names from the plugin's manifest. `plugin diagnose` prints what a check found and the fixes it offers. `plugin repair` runs the check again, shows exactly what the chosen fix will change, and asks first (`--yes` skips the question); Agora re-checks every step against the instance as it is now, and reports anything it skipped or could not do. `plugin launch-checks` runs the checks a launch would run, without launching.

`plugin list` reads the core-resolved install state without starting plugins. Its status column explains why an installed plugin is not runnable, including disabled plugins, incompatible host APIs, dependency problems, cycles, and recorded activation failures. `plugin status` is a convenience alias.

`plugin preview` validates a package or development folder and prints the same install preview used by the install flow. It does not install or activate anything. A package is copied into Agora's managed plugin directory; a development folder is registered in place so edits remain visible to the author. In JSON mode the preview carries `packageDigest`, the SHA-256 of the package (or of the development folder's manifest and files). `plugin install` previews, asks, and passes that digest with the consent; core refuses an install whose bytes no longer match and says "the plugin changed since you approved it", so an answer given for one package is never applied to another. Preview again to install the new bytes.

`plugin validate` checks a development folder's manifest, every crash pack it contributes and every game package it contributes, without installing or running anything. For crash packs, each problem names the pack file and, for a rule, the rule's id; a pack path that escapes the folder, a missing or unparsable pack, and a pack written for another crash contract version are all reported. For game packages, each is parsed, its `parents` and `extends` are resolved as far as the folder allows, and the same registration checks the app runs are applied. A parent that lives in another plugin is a note, not an error. Script function names are checked against the entrypoint when its exports can be read without running it; when they cannot, the command says they were not checked. Each problem names the file and, when it can, the field. The command exits non-zero when there is any problem. It runs no code, so it is safe on an untrusted folder.

An install that requests capabilities prints each required or optional capability, its user-facing explanation, and whether it can change data, then asks for consent. Answer `y` or `yes` to continue. `--yes` skips this prompt for a deliberately scripted invocation:

```bash
agora plugin install package ./example-plugin.zip --yes
agora plugin install development ./example-plugin --yes
```

A plugin that has code (an `entrypoint`) also gets one more question, asked once with the install: `<name> runs code. Allow it? [y/N]`. `--yes` answers yes to both, `--no-code` installs it without allowing its code to run (its permissions are still granted), and with no terminal and no `--yes` the answer is no. The answer is per plugin and changeable later: `plugin scripts <ID> allow` or `deny` (deny also stops it now). `plugin list` shows each plugin's `Code` as `none` (data only), `allowed` or `not allowed`. `plugin stop` engages the emergency stop, under which no plugin code runs, official plugins included, whatever each was allowed; `plugin resume` releases it. The stop is the `plugins_emergency_stop` setting, not engaged by default. Data-only plugins (game packages) run no code and are not affected by it. The old `plugins_enabled` setting is ignored.

Without `--yes`, a non-interactive invocation cannot grant capabilities. The CLI shows the preview and passes a negative consent answer to core, so `PluginService` refuses the install. This keeps scripting fail-closed; do not use `--yes` unless the package's preview is trusted. In JSON mode, a pending interactive preview and prompt are written to standard error; the completed plugin summary is the single JSON value on standard output.

### Official plugins

```text
agora plugin official list
agora plugin official install <PLUGIN_ID> [--yes] [--no-code] [--file <ZIP>]
agora plugin official update <PLUGIN_ID> [--yes]
agora plugin official rollback <PLUGIN_ID> [--yes]
```

An official plugin is an ordinary plugin package whose id, version and package SHA-256 are listed in the `official_plugins` table of Agora's Ed25519-signed catalog (`registry.db`). Nothing in the plugin's own manifest can make it official; the `publisher` part of its id is only a namespace. `list` shows what the catalog offers and where each one stands (not installed, installed, update available, or an installed copy that is not the official package). If the catalog is missing, unsigned or fails its signature check, `list` says so and offers nothing, and `install` / `update` / `rollback` refuse.

`install`, `update` and `rollback` download the package from the URL the catalog names (HTTPS only, pinned to that host), refuse it unless its size and SHA-256 match the catalog entry **before anything is unpacked**, and then use the same installer as `plugin install package`, so the same capability consent applies: without `--yes` the command prints what the package asks for and stops. `--no-code` installs the target and any dependency that has scripts without allowing their code to run; each package that has code keeps its data features but loses the ones its code provides. The consent text names each such package, says what it loses, and says how to allow the code later (`agora plugin scripts <PLUGIN_ID> allow`). `install` and `update` also resolve the package's official dependencies: a missing one is installed and an outdated one is updated from the same signed catalog, dependencies first, under the same consent. The prompt lists each extra package (also in `--json`, as `alsoInstalls`). A dependency the catalog does not supply, or a version that does not satisfy the requirement, refuses the whole install before anything is written. `install` is for a plugin that is not installed, `update` moves an installed one to the catalog's current version (and replaces a same-id community copy with the official package), and `rollback` returns an official install to the previous version the catalog keeps. An id is matched exactly: an empty id, a path, or a different capitalisation matches nothing.

`--file <ZIP>` installs a local package file instead of downloading it, for offline machines and test setups. The signed catalog entry for that id is still the trust anchor: the file's size and SHA-256 must match the pin, and the refusal names both the file's hash and the pinned hash. There is no override — a file that does not match is never installed. Everything after the bytes are obtained is the same code the download path uses, including dependency planning, the file-tree digest check, the same installer and the same capability and script consent. A dependency that is already installed at a satisfying version is left alone; one that would need a download cannot be fetched offline, so the install is refused before anything is written and the refusal names the missing dependency. A local package may replace an already installed copy (it must still match the pin); the output says `Updated` instead of `Installed` in that case, and `--json` carries the same distinction in `action` (`Installed` or `Updated`).

An installed plugin counts as official only while its id, version and installed package hash still match a signed catalog entry and its files are unchanged since they were installed. Its files are checked against the tree digest the signed entry pins, not against anything stored in the local database. If it stops matching (the files were edited, the catalog no longer lists that version or pins a different package, the catalog cannot be verified), it is **blocked**: its game packages do not load and its scripts do not run, whatever the emergency stop says, and the reason appears in `plugin list --json` (`official.reason`, and a `Blocked` status) and in the startup warnings. The same applies to a plugin installed through `plugin install package` that uses an official plugin's id without being that plugin. Official plugins register their games before community plugins, so a community plugin can never claim an official game id or store product id. Official status does not change who may run code: a scripted official plugin is asked about at install (`--yes` allows it, `--no-code` does not) and can be stopped like any other.

### Content providers

```text
agora provider list
agora provider enable <PROVIDER_ID>
agora provider disable <PROVIDER_ID>
agora provider search <PROVIDER_ID> [QUERY] --content-type pack
agora provider plan-digest <PROVIDER_ID>:<PROJECT_ID>@<VERSION_ID>
agora provider show <PROVIDER_ID>:<PROJECT_ID>
agora provider versions <PROVIDER_ID>:<PROJECT_ID> [--instance <INSTANCE>]
agora provider preview <PROVIDER_ID>:<PROJECT_ID> [--version <VERSION_ID>] [--instance <INSTANCE>]
```

`provider show` prints a project's details and `provider versions` its versions (only those that fit an instance with `--instance`). `provider preview` shows what an install would download, from which hosts, and any file without integrity information, before anything is downloaded.

`provider list` shows the official providers (Modrinth, Technic) and every plugin provider, with why one is unusable right now. Enabling or disabling a plugin's provider enables or disables that plugin. `plan-digest` resolves one pack version and prints the digest a curator pins as `sha256` for a `provider_pack` catalog entry (see `REGISTRY_CURATION_REFERENCE.md`).

```text
agora pack install --provider <PROVIDER_ID> --project <PROJECT_ID>
agora pack install --provider <PROVIDER_ID> --project <PROJECT_ID> --version <VERSION_ID> --name <NAME>
agora pack install --provider <PROVIDER_ID> --project <PROJECT_ID> --allow-unverified
agora technic search <QUERY> --limit 20
agora technic install <SLUG>
agora technic install <SLUG> --build <BUILD> --name <NAME>
agora technic install <SLUG> --minecraft <VERSION> --loader <LOADER> --loader-version <VERSION> --sha256 <HEX>
agora technic install <SLUG> --minecraft <VERSION> --loader <LOADER> --loader-version <VERSION> --allow-unverified
```

`pack install --provider` installs a provider's modpack as a **new** instance, through the same registry the desktop uses. It cannot be combined with a manifest path and instance. `--version` is optional; without it the provider chooses.

**Files with no integrity information.** A provider file that publishes no digest at all, or a Technic zip pack without `--sha256`, is refused with `ERR_LOW_SECURITY_DISABLED`, naming each file's host. That is the same question the desktop app asks before such an install. Pass `--allow-unverified` to `pack install --provider`, `technic install` or `mod update` to answer yes for that command. Turning on `allow_unverified_packs` (*Allow low security downloads* in the desktop app) answers yes for every install. Agora cannot tell whether such a file was changed on the way, so only do this for a source you trust.

Technic consent is off by default. It is stored as the `technic_enabled` setting, and a zip pack without `--sha256` also needs `--allow-unverified` or the `allow_unverified_packs` setting; `agora settings set technic_enabled true` changes it. `technic search` and `technic install` are refused while the setting is off. Solder packs install at their recommended build unless `--build` names one. Zip packs carry no metadata, so you must give their Minecraft version, loader, and loader version. Pass `--sha256 <HEX>` (64 hex digits) to check the zip download against a published SHA-256; the download fails if it does not match. Without `--sha256` the CLI warns that the zip is installed unverified, and it does so before the install starts. `--sha256` is refused for Solder packs. Core checks the settings and the confirmation on every call.

### Updates

```text
agora instance updates <INSTANCE>
agora instance updates <INSTANCE> --apply
agora instance updates <INSTANCE> --apply --yes
agora mod pin <INSTANCE> <FILE>
agora mod unpin <INSTANCE> <FILE>
agora mod changelog <INSTANCE> <FILE>
agora pack update-preview <INSTANCE> <MRPACK>
agora pack update <INSTANCE> <MRPACK> --yes
```

`instance updates` checks every installed item against its source and lists what is newer, the same check the desktop's update panel runs, and it stores the result for the instance badge. Rows that are only notes (a newer build for another Minecraft version) or whose release date is unknown are listed but never applied. `--apply` plans the applicable updates as one batch with one snapshot and full rollback, and prints that plan. Nothing changes until you add `--yes`.

`mod pin` keeps one installed file out of update checks, and `mod unpin` lets it be offered again. Pins are stored in the instance manifest. `mod changelog` prints what changed between the installed file and its pending update, from the signed registry. It reports when there is no newer version or when the source published no changelog.

`pack update-preview` compares an .mrpack against the instance and lists the changes, the downloads, and every conflict, without changing anything. `pack update` applies the same merge. It refuses unless every conflict has an answer: `--keep-ours <KEY>` keeps the instance's file and `--take-theirs <KEY>` adopts the pack's file, and each key must be one the preview listed. It also refuses without `--yes`.

`plugin check-update` fetches a plugin's signed update document and reports what it means. It downloads only the metadata, never a package. With no plugin id it checks everything installed, and reports plugins with no update source rather than skipping them silently. The result distinguishes cases that a single "up to date" would blur together: a newer release that needs a newer Agora is reported as exactly that, and an installed version ahead of what the publisher lists is reported rather than downgraded.

`plugin update` applies what the publisher is offering. It re-checks rather than trusting an earlier result, downloads the package, verifies it against the size and SHA-256 in the signed document, and then goes through the same install path a manual install uses. A release asking for capabilities or network hosts the installed version was not granted stops and prints what is new; `--yes` accepts that. Development folders are never updated.

`--yes` confirms capabilities and the release, but never allows code. If the new version brings new or changed scripts, its code is installed but does not run until you allow it separately: re-run with `--allow-scripts`, or use `plugin scripts <PLUGIN_ID> allow`. An update that is only about code can be applied with `--yes` alone; the command tells you which flag allows the scripts. Consent is bound to the code you were shown, so an edit to a script on disk also stops it until you allow the new code.

Both require the plugin system to be on. Nothing contacts a publisher until you ask it to, or until you turn on automatic checking, which is off by default.

### Publishing

`plugin keygen` and `plugin sign` are author tooling. The launcher never holds a private key.

```bash
agora plugin keygen --out ./signing.key --key-id 2026-09
```

writes an Ed25519 private key and prints the public half as a ready-to-paste `agora-plugin-update.json`. It refuses to overwrite an existing file, because a signing key cannot be regenerated and overwriting one destroys the only copy. Keep it out of your repository: losing it means you cannot ship updates to existing installs, and nothing can restore that.

```bash
agora plugin sign ./updates.json --key ./signing.key --key-id 2026-09
```

signs an update document in place. The document does not need a `signatures` block beforehand — requiring a valid signature in order to produce one is a requirement nobody can meet — but everything else about it is validated first, so a bad release fails here rather than at every user who later fetches it.

See `docs/plugins/publishing.md` for what the signature does and does not prove.

`plugin restore-data <ID>` puts a plugin's saved data back to the copy Agora kept before its last data-shape change. It always asks first (`--yes` skips the question), and it discards anything the plugin wrote since that copy, so it is a deliberate recovery step, not an undo for an ordinary update.

`plugin recover [ID]` resolves an install or uninstall that a crash cut short (MASTER_SPEC §32.11). The plugin record and the journal commit together in SQLite. Startup completes committed changes and restores the previous package for uncommitted changes. During an uninstall, the package is moved aside before the commit. `plugin recover` runs before automatic startup recovery and accepts these explicit directions for one plugin.

- `--restore --yes` puts the verified stash back and moves the replacement to a separate folder. After a commit, the old plugin record and the restore decision commit together before the files move.
- `--discard --yes` keeps the committed package and deletes its verified stash. This removes the copy used to go back to the previous version.
- `--keep-aside --yes` preserves both copies. For a committed install or a legacy stash, it names the archive where the stash was moved. For an install interrupted before commit, it puts the verified previous package back and keeps the replacement aside.

All three exit non-zero without `--yes`, having changed nothing.

An install or update takes the plugin-install lock and checks for an earlier journal before starting. It logs any recovery it performs. An unresolved journal or legacy stash blocks removal. A journal whose folder identities cannot be verified is kept for inspection.

`plugin enable` and `plugin disable` change only the installed plugin's enabled state. `plugin remove` removes the plugin and its log while keeping plugin data by default; add `--purge-data` only when that separate data-removal decision is intended. Removing a development plugin never deletes its source folder. `plugin log` reads the requested number of recent lines, defaulting to 200. `plugin disable-all` is the recovery path: it stops and disables every currently enabled plugin without removing installations or stored data.

### Launching

```bash
agora launch <INSTANCE>
agora launch <INSTANCE> --timings
agora launch <INSTANCE> --yes
agora launch <INSTANCE> --world <WORLD_FOLDER>
agora launch <INSTANCE> --server <HOST[:PORT]>
```

`--world` opens a world as soon as the game starts (Minecraft 1.20 or newer), and `--server` joins a server (any version that can join servers from the command line). They cannot be combined.

The CLI writes the total session duration to standard error. `--timings` adds individual launch-phase durations there as well.

`--yes` bypasses the interactive health confirmation. It does not make an unhealthy instance safe and should not be used as a generic automation default.

The CLI launches directly through Agora core. Microsoft authentication is used for the normal online identity.

If Microsoft cannot be reached (no connection, a timeout, a 5xx from its servers), Lockdown Mode is on, or the Microsoft sign-in switch is off, the launch continues with the stored profile (name and UUID) and a placeholder offline token. It prints `Playing offline: online servers and Realms will not accept this session.` followed by the reason. Single-player still works; online servers and Realms will refuse the session. This fallback is never used when Microsoft says the session is invalid (`invalid_grant`, or another 4xx refusal): sign in again with `agora auth login`.

### Microsoft authentication

```text
agora auth login
agora auth login --no-browser
agora auth status
agora auth list
agora auth switch <USERNAME_OR_UUID>
agora auth logout [--account <USERNAME_OR_UUID>]
agora auth repair
agora auth refresh
agora auth github login [--no-browser]
agora auth github status
agora auth github logout
```

These commands manage the Microsoft/Xbox/Minecraft identity used for direct launch, except `auth github`, which manages the separate GitHub account used for voting (`registry vote`).

`auth refresh` renews the active Microsoft account's sign-in now instead of at the next launch.

`auth github login` works like the app's GitHub sign-in: it prints a code and a github.com link, opens the link unless `--no-browser`, and waits until you approve (Ctrl-C cancels). The sign-in is stored exactly as the app stores it (the operating-system credential store, or Agora's encrypted file), so with the same data folder, signing in or out here does the same in the app. `auth github status` only reads that store and does not go online.

`auth login` uses the Microsoft device-code flow: it prints a short code and a verification URL, opens the URL in your browser unless `--no-browser` is given, and then waits until you finish signing in. Press Ctrl-C to cancel. The device code and every token stay inside Agora; nothing is pasted back into the terminal.

Sessions created before Agora moved to its own Microsoft application cannot be renewed. `auth status` reports those as `sign_in_required`; run `auth login` once to restore direct launch. Instances and other accounts are unaffected.

More than one account can be signed in. `auth login` with a different account adds it and makes it the active one; the previous account stays signed in. `auth list` marks the active account with `*`, and `auth switch` changes which one launches use. `auth logout` signs out the active account, and the next signed-in account becomes active; `--account` signs out a particular one.

On Windows the sign-in is stored in a file under the selected data root: DPAPI-protected for the normal root, device-key encrypted for a relocated or portable one. `--data-dir` therefore gives a disposable root its own sign-in. On macOS and Linux authentication is not scoped by `--data-dir`: `auth logout` removes the shared operating-system credential entry and can sign the desktop app out even when the CLI uses a disposable data root.

More than one signed-in account is kept in one encrypted record, replaced in a single write, with a recovery copy. A refresh that finishes while you switch accounts updates the account it refreshed instead of making it active again.

If the saved account list cannot be read, launching still uses the active account and the command prints one warning. The unreadable data is never deleted: it is renamed aside with a timestamp. Switching accounts, signing in another account, signing out, and removing an account are refused with the reason until you run `agora auth repair`, which rebuilds the list from the accounts it can still read. It names any entry it could not recover (that account needs a fresh sign-in) and where the unreadable copy was kept. `auth status` also reports the repair state. The desktop Accounts page shows the same notice with a **Repair accounts** button.

### Settings

```text
agora settings list
agora settings get <KEY>
agora settings set <KEY> <VALUE>
```

`settings set` parses the value as JSON and falls back to a string, then checks it against the setting's declared type in core's registry. An undeclared key is refused, and so is a value of the wrong type; the reason is printed and nothing is stored. Legacy boolean encodings (`0`, `1`, `"true"`, `"false"`) are accepted and stored as genuine booleans. Quote strings explicitly in scripts when the distinction matters:

```bash
agora settings set launch_mode '"direct"'
agora settings set always_pre_touch false
agora settings set network_modrinth_enabled 0   # stored as false
```

Every network and privacy switch fails closed: a stored value that cannot be read (for example a truncated string) is treated as off, and the network gate denies the request. A malformed Lockdown Mode value stays engaged. An invalid stored value is reported once and read as its fail direction; the registry migrates legacy encodings when the database is opened.

Setting keys are declared in `crates/agora-core/src/settings_registry.rs`. Prefer dedicated commands when one exists.

### Crash investigation

```text
agora crash list <INSTANCE>
agora crash inspect <INSTANCE> <FILE>
agora crash last <INSTANCE>
agora crash show <SESSION>
agora crash investigate <INSTANCE>
agora crash investigate <INSTANCE> --file extra-log.txt
agora crash investigate <INSTANCE> --since 2h
agora crash analyze <FILE>... [--each] [--instance <ID>] [--plugin-dir <FOLDER>]... [--game-version <V>] [--allow-code] [--json [--facts]]
agora crash analyze --paste [--each] ...
agora crash patterns <INSTANCE>
agora crash suspect-file <INSTANCE> <OWNER_KEY>
agora crash still-crashing <INSTANCE> <RULED_OUT_OWNER_KEY>
agora crash fixed <BUCKET> <OWNER_KEY>
agora crash report <INSTANCE> [--log <FILE>] [--out <FILE>]
```

These follow the desktop's "test it" flow. `crash suspect-file` prints the installed file a suspect from the stored answer provides (its owner key is `item:` followed by the suspect's `owner.key` in `crash last --json`; `crash still-crashing` prints them too). Turn it off with `mod disable`, launch, and turn it back on with `mod enable`. If the game still crashes, `crash still-crashing` rules that suspect out and lists the next ones. If turning it off fixed the crash, `crash fixed` tells the Crash Doctor, so a later crash of the same kind counts that suspect a little more strongly; the bucket is `record.bucket` in `crash last --json`. `crash report` builds the redacted report the desktop copies for an assistant or the community Discord: the stored answer, the instance's mod list, and a crash log you name with `--log`. Agora does not send it anywhere.

`--file` may be repeated. Review logs for local paths, usernames, server addresses, tokens, or private chat before sharing output.

`crash list` lists the crash answers stored for an instance: the session id, when it was diagnosed, and the answer's headline (or why there is none). `crash inspect` still reads one raw crash report file from `crash-reports/`; it is a file view, not a diagnosis. `crash last` and `crash show` print a stored answer. `crash last` also says which session the answer belongs to, by when it ended and how it ended, and says first when the newest session has no answer yet (a launch that was not diagnosed, such as one still running). Its `--json` keeps the stored answer's fields and adds `newest_session`, `answer_session` and `newest_session_has_answer`. `crash investigate` collects the instance's recent crash evidence and runs the same crash engine as `crash analyze` against it. The search is the same one the live diagnosis runs: it follows the crash packs' declared sources, for every supported game, within a time window. Files named with `--file` are read too. `--since` overrides the window: an RFC 3339 time (`2026-01-01T12:00:00Z`) or a duration before now (`90m`, `2h`, `1h30m`, `1d`); without it the newest recorded session's window is used, or the last hour when the instance has no session. Its JSON is `{ "answer": <CrashAnswer>, "files": [<per-file report>] }`, and it exits non-zero when no evidence could be read; a source that could not be looked at is reported with the plugin that named it, never as "nothing found".

`crash analyze` reads crash files that came from somewhere else, such as a friend's log or one attached to a forum post. Without `--instance` it needs no instance and no launch record. The files are matched to parsers by their content using the crash packs of installed, enabled plugins; `--plugin-dir` adds a development folder, read the way `plugin validate` reads one. A script parser or inspector in a development folder runs code, so it is refused unless `--allow-code` is given; script code in an installed plugin runs only if the user already allowed that plugin's code. With no instance, a name in the log is reported as itself (`ExampleMod.dll`) unless a pack's `roles` mark it as the system, the game, or a framework. A parser that fails is shown under `Could not check` and names its plugin; it is never reported as "nothing found". `--json --facts` adds a `parsed` list holding what each parser found in each file (its facts and what it looked for), for someone writing or checking a crash pack's rules.

`--paste` reads the text from stdin as a pasted fragment. Only pasted text may be read by a pack's `snippet` parsers, which handle a fragment whose whole-file header is missing; it still needs a recognisable shape (an exception line, a stack frame), and the answer says Agora read a snippet, caps its confidence at `possible` and tells the user to paste or pick the whole log for a better result. Files named on the command line are whole files and never use the fallback. `--paste` can be combined with files, and with `--each` each input is its own crash.

`--instance` names the instance the crash came from. Only that game's crash packs, plus packs for every game, take part; a ref is attributed to the mod that provides the file in the instance's deployed view; and a rule may ask an inspector to open one file (for example, an SKSE plugin DLL) and report pass, fail or unknown. The instance's own game version is used unless `--game-version` overrides it. An unknown instance is an error. Without `--instance` nothing is inspected and the loose-file answer is unchanged.

`crash patterns` prints the pattern across the crashes Agora already knows for one instance. If no older log has been stored yet, it sweeps the folders the instance game's crash packs' `sources` name first. The output is the same cross-crash summary `--each` prints, plus what the sweep found and any source it could not read. An instance with no logs and no stored session answers says so; that is not a clean result.

With `--instance`, `crash analyze` sweeps those same sources for older crash logs, analyses each once (keyed by the file's content hash), and adds the cross-crash count to the answer and to `--each`'s summary. A failed sweep or a source folder that could not be read is listed under `Could not check`, never treated as no earlier crashes. The sweep is bounded, and older logs it left for a later run are named. With `--json` a result for an instance also carries `stored_history` and `sweep`.

`--each` analyzes every file as its own crash, one `analyze` call per file, and then prints a summary across them. The summary counts how often each suspect owner was named, how often it was the top suspect, and at how many distinct crash places it came up; it also counts crashes Agora could not read and crashes that named nobody. It is a count, not a verdict. Framework and hook-framework owners are listed apart, because they are always running and appear in many crashes for that reason. With `--json`, the output is `{ "crashes": [ { "file", "answer", "files" } ], "summary": <CrashHistory> }` (each crash also gets `parsed` with `--facts`). With `--instance`, each suspect that is an installed item also carries a `save_risk` label: `safe` when every file is an asset, `risky` when a file can add records or run code, and `unknown` when Agora cannot tell. Use it to warn the user: testing a risky mod on an existing save can damage it, so test on a new game or back the save up first.

The text output is the headline, `How sure:`, then `Notes:` (a warning about the evidence, such as the pasted-snippet note), `Suspects:` (with a save-risk label when `--instance` gives one), `Findings:`, `Inspections:` (each deeper check, its target, result and detail), `Could not check:` and `Files:` (each file is `read by <plugin>` or `not recognised by any installed crash pack`). Empty sections are omitted. `--json` prints exactly `{ "answer": <CrashAnswer>, "files": [<per-file report>] }`, and an inspection's detail and facts are part of the answer. With `--each`, the text is each crash's headline and suspects, then the cross-crash summary. The command exits 0 when at least one file was read, and non-zero otherwise; when no installed plugin contributes a crash pack it says so and exits non-zero.

### Guide for AI agents

```bash
agora skill
```

Prints, in the SKILL.md format agent tools read, how an AI agent should use this CLI: Agora's vocabulary, house rules (ask before changing anything, snapshot and dry-run first, never pass `--yes` on the person's behalf), the crash-diagnosis workflow, and what the exit codes mean. The text is compiled into the binary, so it matches that binary's commands, and printing it opens no Agora data. Save the output as `SKILL.md` in an agent's skills folder to keep it loaded, or ask the agent to run the command itself. The player-facing walkthrough is the [AI assistant task guide](https://agoramc.com/docs/guides/ai-assistant).

Commands that stop for consent in a non-interactive session (`launch` on a red health check, plugin capability grants, plugin data restores) say that `--yes` should be passed only once the person using the computer has agreed.

### MCP over standard input/output

```bash
agora mcp serve --stdio
```

The CLI MCP transport uses standard input/output. It is different from the desktop application's optional authenticated localhost server.

When an external client launches this command:

- Reserve standard input and output for JSON-RPC.
- Do not wrap it in a shell that injects banners.
- Keep the process local.
- Use Agora's approval and instance-locking controls for modifying tools.

The stdio transport reads one JSON-RPC request per line and writes one response per line. Notifications do not receive a response. Diagnostics remain on standard error so standard output stays protocol-only.

## Structured output

Examples:

```bash
agora --output json paths
agora --output json health <INSTANCE_ID>
agora --output json registry status
```

Do not parse human tables. For automation:

1. Select JSON explicitly.
2. Check the process exit code.
3. Capture standard error separately.
4. Tolerate additive JSON fields.
5. Never treat a printed success message as sufficient when the exit code is nonzero.

On a command failure in JSON mode, standard error contains an envelope shaped like:

```json
{
  "error": "Instance 'missing' not found",
  "exitCode": 1
}
```

Standard output remains reserved for a successful result. Planning failures can add a structured status envelope; always compare the embedded exit code with the process exit code.

## Exit codes

The CLI maps many core failures to stable semantic ranges.

| Code or range | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Generic or unclassified failure |
| `2` | Command-line usage error |
| `7` | Minecraft process classified as a crash |
| `10` | Local-state or database failure |
| `11` | Instance locked or profile missing/corrupt |
| `12` | Instance creation failure |
| `13` | Catalog missing, invalid, or unsupported |
| `20` | Offline |
| `21` | Download or catalog-download failure |
| `30–34` | Integrity, trust, archive, or disk failure |
| `40` | Authentication required or expired |
| `50` | Feature disabled or unavailable |
| `60–62` | Official-launcher, version/loader, or Java failure |
| `70–72` | Dependency, decision, or migration failure |
| `80–82` | MCP rate limit, denial, or authentication failure |
| `90–94` | Network-policy denial |
| `100+` | Process identity or process-state failure |

Scripts should still inspect structured error output because several distinct errors share a range.

The exit code follows the error's machine-readable code, not the shape it is carried in. An error that is wrapped to add context keeps the exit code of the code it carries: `agora loader install` for a profile whose adoption failed and whose heal is refused by the loader-download setting still exits `92` (the reinstall reason is in the message), rather than falling back to `1`. A code the table does not classify is `1`.

Every machine code, its exit code and its area are declared once, in `agora-core`'s `error_code` module; this page and the CLI read that declaration rather than keeping a table of their own.

`agora doctor` prints its whole report (human or `--json`) and exits `0` when the local database is healthy or not created yet. It exits `10` when it finds damage, a store it cannot read, a database written by a newer Agora, a saved account list that needs `agora auth repair`, or a recovery journal that needs a hand. The JSON report always includes `problems: true` or `false`, so a script can read the reason without parsing the message. The recovery report (see [The recovery report](#the-recovery-report)) is carried in the same output; `problems` covers it, and `recovery.entries[].outcome` says which journal.

Most ordinary lookups for a nonexistent instance currently return generic exit code `1`, not `11`.

## Support checklist

When reporting a CLI problem, include:

```text
Agora version:
Operating system:
Exact command with secrets removed:
Exit code:
Standard output:
Standard error:
Output of `agora paths` with private usernames redacted:
Instance Minecraft and loader versions:
Whether the desktop app was open:
```

Do not attach the whole data directory. Start with the smallest relevant output.
