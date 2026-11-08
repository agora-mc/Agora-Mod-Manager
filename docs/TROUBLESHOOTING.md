# Troubleshooting the Agora CLI

This page is for people running `agora` from a terminal or a script. It assumes you already know your way around a shell, stack traces, Java versions, and mod loaders, so it sticks to which commands to run and how to read the results. Using the desktop app instead? See [Troubleshooting Agora](./DESKTOP_TROUBLESHOOTING.md).

## Start here

Get the build, the paths, and the current state:

```bash
agora --version
agora paths
agora --output json registry status
agora --output json health <INSTANCE>
```

Before you change anything, take a snapshot or work on a copy:

```bash
agora snapshots create <INSTANCE> --label "Before debugging"
agora instance clone <INSTANCE> "Debug Copy" --no-saves
```

To reproduce a problem away from your real data, add `--data-dir ./tmp/agora-debug`. This isolates files only: `auth login` and `auth logout` still change the credential entry the desktop app uses.

## Start from the exit code

The full list is in the [CLI reference](./CLI.md#exit-codes). Several errors share a range, so read the error on standard error as well. In JSON mode it is an envelope with `error` and `exitCode`.

| Exit code | What it means | Go to |
| --- | --- | --- |
| `7` | Minecraft process classified as a crash | [The game crashes](#the-game-crashes) |
| `11` | Instance locked, or its profile is missing or corrupt | [An instance is locked or its profile is damaged](#an-instance-is-locked-or-its-profile-is-damaged) |
| `13`, `30–34` | Catalog missing or invalid, or an integrity, trust, archive, or disk failure | [The catalog is missing or fails verification](#the-catalog-is-missing-or-fails-verification) |
| `20`, `21`, `90–94` | Offline, a failed download, or a network-policy denial | [Network requests fail](#network-requests-fail) |
| `40` | Authentication required or expired | [Sign-in fails](#sign-in-fails) |
| `50` | Feature disabled or unavailable | Check the feature's setting with `agora settings list`. For example, Technic needs `technic_enabled` |
| `60–62` | Official launcher, version or loader, or Java failure | [A loader requirement blocks launch](#a-loader-requirement-blocks-launch) or [Java is missing or rejected](#java-is-missing-or-rejected) |
| `70–72` | Dependency, decision, or migration failure | [An install plan stops](#an-install-plan-stops) |

## Common problems

### The catalog is missing or fails verification

```bash
agora registry status
agora registry sync
```

Never work around a signature failure by using an unsigned catalog. If only a newly packaged build fails, note the build and whether the previous release can sync. Use `--registry-repo` only for a deliberate development or sandbox catalog, because a different repository changes what you are trusting.

### Network requests fail

Lockdown Mode and the per-connection switches live under **Settings > Privacy** in the desktop app. They also apply to the CLI, including while it is already running, so a command can be denied even when your network is fine.

`proxy` controls how Agora's own requests connect. A change takes effect the next time Agora starts, and Minecraft does not use this proxy.

```bash
agora proxy show
agora proxy none
```

### A loader requirement blocks launch

```bash
agora health <INSTANCE>
agora instance loader-version <INSTANCE> <VERSION>
agora health <INSTANCE>
```

`instance loader-version` shows the current loader, the versions compatible with the enabled mods, and the recommendation, then asks before switching. It refuses a version outside the compatible list. Add `--yes` only in a script that has already checked the plan.

If no version satisfies every enabled mod, find the one that doesn't fit with `agora mod why <INSTANCE> <FILE>`, then update or disable it. Candidates Agora could not fully verify can only be chosen in the desktop instance editor.

`javafml` and `lowcodefml` are language providers supplied by Forge or NeoForge, not mod JARs you can install. If health reports them, the loader version is the problem.

To reinstall a damaged loader rather than change its version, run `agora instance repair-loader <INSTANCE>`.

### Java is missing or rejected

```bash
agora runtime list
agora runtime inspect <JAVA_PATH>
agora runtime ensure <MAJOR>
agora instance java <INSTANCE>
```

A newer major is not automatically compatible with a Minecraft version that expects an exact one. If you set a path for a test, clear it afterwards so automatic selection takes over again:

```bash
agora instance java <INSTANCE> --clear-path
```

### An install plan stops

Resolve the plan without executing it, then see how the mods relate:

```bash
agora mod install <PROJECT> <INSTANCE> --dry-run
agora mod graph <INSTANCE>
agora mod why <INSTANCE> <FILE>
agora mod disable-plan <INSTANCE> <FILE>
```

Check the plan for the target instance, the selected files, dependencies, what gets added, replaced, or removed, conflicts, and loader or Minecraft blockers. `--replace-conflicts` applies to every resolvable conflict, so don't combine it with a large batch you haven't reviewed.

### The game crashes

```bash
agora crash list <INSTANCE>
agora crash inspect <INSTANCE> <FILE>
agora crash investigate <INSTANCE>
agora crash investigate <INSTANCE> --file extra-log.txt
```

`crash investigate` ranks suspects from recent logs. If none stands out, bisect:

```bash
agora bisect start <INSTANCE>
agora bisect apply <INSTANCE>
agora bisect good <INSTANCE>
agora bisect bad <INSTANCE>
agora bisect cancel <INSTANCE>
```

Bisect doesn't start the game, so launch it yourself between rounds and answer with `good` or `bad`. `cancel` puts every mod back. A clean health check doesn't prove the game is stable at runtime or that a world is compatible.

### Launches are slow

```bash
agora launch <INSTANCE> --timings
```

Timings go to standard error. Note which phase is slow: health, resolution, materialization, snapshot, or process start. Compare these before drawing a conclusion from the total:

- first launch after a reboot
- immediate second launch
- first launch after restarting Agora
- small and large instances

### An instance is locked or its profile is damaged

An instance being changed by the desktop app or another `agora` process is locked for the duration. The second process waits up to 30 seconds, then names who holds the lock. This is separate from the lock you set yourself, which you remove with `agora instance unlock <INSTANCE>`.

If the profile looks damaged, restore an earlier state:

```bash
agora snapshots list <INSTANCE>
agora snapshots restore <INSTANCE> <SNAPSHOT_ID>
```

### Sign-in fails

```bash
agora auth status
agora auth login --no-browser
```

The CLI launches the game directly, so normal online play needs a Microsoft sign-in. `sign_in_required` means the session can't be renewed, and running `auth login` once fixes it. Sign-in isn't affected by `--data-dir`: `auth logout` can sign the desktop app out too.

### Offline launch fails

Offline readiness is per instance. While you are still online:

1. Run `agora registry sync`.
2. Launch the exact instance once, so its game, loader, and mod files are cached.
3. Confirm its Java is present with `agora runtime list`.
4. Turn on Lockdown Mode in **Settings > Privacy**, launch again, and fix anything missing before you disconnect.

Microsoft sign-in, updates, and uncached downloads cannot complete offline.

### A script gets unexpected results

Always use `--output json` rather than parsing human output, capture standard output and standard error separately, and check the exit code rather than any success message. Add `--log-file <PATH>` to keep the diagnostics. See [Structured output](./CLI.md#structured-output).

## What snapshots cover

- `snapshots create` makes a full snapshot with a broader scope.
- The automatic pre-launch snapshot covers mods, configuration, resources, shaders, data packs, options, and the manifest. It leaves out `saves/`.
- Loadouts only record what is switched on or off.
- Lockfiles describe mods and settings, not world or config contents.

No local snapshot, loadout, or lockfile should be the only copy of a world you can't replace. Use `agora world export <INSTANCE> <WORLD_FOLDER> --out <FILE.zip>` for your own backups.

## Reporting a problem

Fill in the [CLI support checklist](./CLI.md#support-checklist). [Data, logs, and support evidence](./SUPPORT.md) covers what to collect and how to reset data. It also helps to say:

- how often the problem happens
- whether it survives a restart
- whether it happens in a disposable clean instance

These commands gather the usual context without changing anything:

```bash
agora paths
agora --output json registry status
agora --output json health <INSTANCE>
agora crash list <INSTANCE>
agora log share <INSTANCE> --preview
```

Remove these before sharing:

- Microsoft or GitHub tokens
- authorization codes
- local usernames, when they aren't needed
- server addresses
- private chat
- webhook URLs
- filesystem paths outside the relevant instance

Ask usage questions on the project Discord, and file reproducible Agora bugs as GitHub issues.
