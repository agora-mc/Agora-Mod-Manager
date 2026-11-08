# Data, logs, and support evidence

This page is for CLI users. It shows how to find Agora's data, identify a build, and collect the smallest useful evidence for a bug report without copying a whole profile. Using the desktop app? See [Troubleshooting Agora](./DESKTOP_TROUBLESHOOTING.md).

## Find the data root

Ask the CLI where everything is, rather than relying on a platform path copied from documentation:

```bash
agora paths
agora --output json paths
```

The output lists the root, instances, catalog database and signature, Java runtimes, loader cache, snapshots, staging area, locks, and local state database.

The CLI resolves the root the same way the desktop app does, so by default they share it. `AGORA_DATA_DIR` and a `portable.txt` marker beside the executable apply to both. If you used `--data-dir` or `AGORA_DATA_DIR` when the problem happened, run `agora paths` with the same override.

### Other games and plugins

`agora paths` prints the shared root; the rest of this section is relative to it:

- A game instance is a folder under `instances/<ID>`, like a Minecraft instance, with its own `instance_manifest.json`.
- The content store is under `content/`, with one manifest per item under `content/items/` and the file bytes under `content/objects/`.
- Pinned bases are listed by `agora games base list`, with their state. A base record lives under `bases/manifests/`; `bases/roots.json` names base folders Agora keeps outside the data root.
- Plugin packages are under `plugins/installed/`, and each plugin's log is `plugins/logs/<PLUGIN-ID>.log` (`agora plugin log <ID>` reads it). A plugin's saved data lives in `local_state.db`, not in its package folder.

`agora games list` and `agora games discover` show which games and installs were found. `agora plugin list` shows each plugin's status, and `agora plugin official list` shows the official ones and whether an installed copy still verifies. `agora games instance check <INSTANCE>` is the cheapest summary of a game instance's framework and load-order findings.

## Identify the build

```bash
agora --version
```

Also record which `v*` release the binary came from, or the Git commit for a source build. A CLI bundled with the desktop app has the same version as the app.

## Know which log is which

| Evidence | Where to find it | What it covers |
| --- | --- | --- |
| CLI diagnostics | Standard error, or a file you choose with `--log-file <PATH>` | Command parsing, catalog, planning, launch, and host errors |
| Command result | Standard output, preferably with `--output json` | The result itself. In JSON mode an error envelope goes to standard error instead |
| Game log | The instance's `logs/latest.log` and, if present, `logs/debug.log` | Minecraft, loader, and mod startup or runtime errors |
| Crash report | The instance's `crash-reports/`, or a JVM `hs_err_pid*.log` at the instance root | Game crashes and fatal JVM failures |
| Plugin log | `<root>/plugins/logs/<PLUGIN-ID>.log`, or `agora plugin log <ID>` | What one plugin did, and its errors |

The game-log and crash-report rows describe Minecraft. `agora log` and `agora crash` cover Minecraft instances; for another game, include the log or crash file the game itself writes, and the output of `agora games instance check <INSTANCE>`.

List an instance's logs and crash reports:

```bash
agora log list <INSTANCE>
agora crash list <INSTANCE>
```

`crash investigate` reads a bounded set of recent crash reports, game logs, and JVM fatal-error logs. That doesn't make every file in the instance safe to share.

## Collect a minimal support bundle

Agora doesn't generate a support archive. Build a small folder by hand and review every file before compressing it:

1. Write a short note with the Agora version and release, operating system, the exact commands you ran, the exit code, the instance's Minecraft version, loader, and loader version, and how often the problem happens.
2. Capture context with commands that don't change anything:

   ```bash
   agora --output json registry status
   agora --output json health <INSTANCE>
   agora crash list <INSTANCE>
   ```

3. Add only the relevant `latest.log`, `debug.log`, crash report, or JVM fatal-error log, with personal data removed.
4. Say whether the problem also happens in a disposable clean instance, for example one created under `--data-dir ./tmp/agora-clean`.

To share a single log as a link instead, `agora log share <INSTANCE> --preview` prints exactly what would be uploaded to mclo.gs. Drop `--preview` to upload it.

Never include the whole data root, `local_state.db`, `mcp_token`, credential-store exports, saves, server lists, private configuration, or unrelated instances. `agora paths` prints local paths, so remove anything that identifies you before sharing it.

## Personal-data review

Logs and diagnostic output can contain:

- local account names and filesystem paths
- Minecraft or GitHub names
- server addresses and chat text
- instance and private pack names
- access tokens, authorization codes, or request headers
- JVM arguments and environment-derived values
- mod configuration or crash context that reveals private data

Remove secrets completely rather than masking part of them. If a token may have been exposed, revoke or regenerate it before sharing anything.

## Reset application data

Agora has no factory-reset command. Resetting is destructive, so rename rather than delete:

1. Close Agora and Minecraft, and stop any running `agora` processes.
2. Run `agora paths` with the same overrides and note the root.
3. Back up worlds and anything else in the profile you want to keep, for example with `agora world export`.
4. Rename the root folder.
5. Run `agora paths` and `agora registry sync` to confirm a clean profile is created, then remove the renamed copy once you no longer need it.

Microsoft credentials live in the operating-system credential store, so neither `--data-dir` nor `AGORA_DATA_DIR` isolates them. Renaming the root doesn't sign you out, and `agora auth logout` removes the shared entry the desktop app also uses. A portable copy is the exception: it keeps its credentials encrypted inside its own data folder.
