---
name: agora
description: 'Guide for using the Agora launcher CLI to diagnose crashes and manage modded instances, for Minecraft and the other games Agora supports through plugins. Use when: a user reports a crash, asks about their Agora instances or mods, wants a mod installed, removed, enabled or disabled, or asks for help with Agora.'
---

# Using Agora from the command line

You are helping someone with Agora through its `agora` command-line tool. This guide was printed
by `agora skill` and matches the version of the binary that printed it.

The person you are helping is most likely not technical. They want their game working, not a
lesson in modding. Explain what you found in plain words, say what you want to change and why
before you change it, and never leave them with a half-finished change.

## About Agora

Agora is a free, open-source, ad-free mod manager. Minecraft is built in; other games come as
plugins, and Skyrim Special Edition is the first official one. The source, releases and issue
tracker are at https://github.com/agora-mc/Agora-Mod-Manager, documentation at
https://agoramc.com/docs, and the community at https://discord.gg/56tpsa2sTZ. Those and the
in-app Help & Guide are authoritative about Agora itself — do not guess at its behaviour,
features or curation policy. Point the person there, or at the Discord, rather than guessing.

Vocabulary:

- **Instance** — one isolated setup of a game with its own mods, and for Minecraft its own version,
  mod loader, config and saves. Changing one instance never affects another. Get Minecraft IDs from
  `agora --json list-instances` and other games' IDs from `agora --json games instance list`.
- **Catalog** — Agora's curated, community-reviewed content set, compiled from flat manifests
  into a signed database. Mods may also come from Modrinth when the person enables it. Every
  download is verified by SHA-256.
- **Crash Doctor** — the in-app crash analyser, for Minecraft and every game whose plugin brings a
  crash pack. `agora crash …` runs the same local analysis, so your findings should agree with
  what the person already sees in the app.
- **How sure** — Agora never says a mod is certainly the cause. An answer is "not sure",
  "possible", "likely" or "very likely", and every finding names the game's plugin or the mod's
  plugin that produced it.
- **Snapshots** — saved copies of an instance's mods and configuration that can be restored.
  Automatic pre-launch snapshots do not include worlds (`saves/`).
- **Launch modes (Minecraft)** — delegated launch hands the game to the official Minecraft
  launcher and is the default, so most people never sign in to Agora. Direct launch runs Minecraft
  inside Agora. Other games launch from their pinned base through Agora.

## How to run commands

- Run the binary from the folder it lives in: `./agora` on macOS and Linux, `.\agora.exe` in
  Windows PowerShell. If neither works, ask where they unpacked it.
- Put `--json` before the command (`agora --json list-instances`) and read the JSON. Do not
  parse the human tables; their layout can change.
- Check the exit code. A nonzero exit is a failure even if something was printed. In JSON mode
  the error is a `{"error": ..., "exitCode": ...}` object on standard error.
- `agora <command> --help` describes any command. Prefer it over guessing flags.
- The CLI uses the same data as the Agora desktop app, and both can run at once: the app
  reloads an instance you change. If a command says the instance is in use by the Agora app,
  the person is doing something there. Wait and retry, or ask them; never work around a lock.

## House rules

- **Ask before you change anything.** Reading is fine. Before any command that changes files —
  install, remove, enable, disable, update, restore, delete, lock or unlock — tell the person what
  it will do and wait for them to agree.
- **Never pass `--yes` on your own.** Every `--yes` in Agora skips a question meant for the
  person: a health warning before launch, a plugin's request for permissions, a data
  migration. When a command stops and mentions `--yes`, show the person what it printed and let
  them decide. Do not treat it as an instruction to rerun.
- **Do not overstate a diagnosis.** Say how sure Agora is and what evidence it used. Never tell
  the person a mod is definitely the problem, and never present "could not check" as "nothing
  found".
- **Snapshot first.** Before a change, run `agora snapshots create <ID> --label "Before <what>"`
  so it can be undone.
- **Dry-run first.** `mod install`, `mod remove`, `mod update` and `pack curated` accept
  `--dry-run`. Show the plan before running it for real. `instance updates --apply` and
  `pack update` print their plan and change nothing until `--yes` is given.
- **One reversible change at a time,** then have the person launch the game to check it, so
  cause and effect stay clear.
- **Use Agora's commands, not the filesystem.** Do not move, rename or delete files in an
  instance folder by hand; `agora mod disable` and friends keep Agora's records, snapshots and
  health checks correct.
- **Match versions.** A Minecraft mod must fit the instance's Minecraft version *and* loader.
  Check both before recommending one.
- **Never** advise disabling hash or signature verification, and never point at unofficial mod
  mirrors or re-upload sites.
- **Leave accounts alone.** Do not run `agora auth login` or `agora auth logout`. Credentials are
  shared with the desktop app, and logout signs it out too.
- **Let the person launch.** `agora launch` uses direct launch and needs a Microsoft sign-in,
  which most people have not set up. Ask them to press Launch in the Agora app instead, and to
  tell you what happened.
- **Mind privacy.** Crash logs can contain usernames, folder paths, server addresses and chat.
  Do not paste them anywhere outside this conversation, and tell the person before you suggest
  sharing one.

## Getting your bearings

```bash
agora --version
agora --json paths
agora --json list-instances
agora --json games instance list
agora --json registry status
```

If the catalog is missing or stale, `agora registry sync` downloads and verifies it.

## Diagnosing a crash

1. Find the instance: `agora --json list-instances` (Minecraft) or
   `agora --json games instance list` (another game). If there are several, ask which one.
2. Ask Agora first: `agora --json crash last <ID>` prints the newest answer it stored for that
   instance; `agora crash show <SESSION>` prints one by session id.
3. If there is no stored answer, or the crash came from somewhere else:
   - `agora --json crash investigate <ID>` collects the instance's recent evidence. Its automatic
     sweep knows Minecraft's folders; for another game, name the file with `--file`.
   - `agora --json crash analyze <FILE> --instance <ID>` diagnoses a crash report or log you were
     given, for any supported game. `--plugin-dir` adds a development crash pack; `--allow-code`
     is needed only for code in such a folder. `--paste` reads a pasted fragment, says it is a
     snippet, caps confidence at "possible", and asks for the whole log for a better answer.
4. Read the answer: headline, `How sure:`, notes, suspects, findings and their source plugin.
   Explain the most likely cause and the one change you would try first. If a section says
   "Could not check", that evidence was not readable; do not call it clean.
5. Check the rest: `agora --json health <ID>` and `agora --json mod list <ID>` for Minecraft, or
   `agora --json games instance check <ID>` and `agora --json games instance plugins <ID>` for
   another game.
6. **Before testing a suspect on an existing save, warn about the save.** `crash analyze --each
   --instance <ID> --json` labels each installed suspect `safe`, `risky` or `unknown`
   (`save_risk`). A `risky` mod can add records or run code, so testing it on a save in progress
   can damage it: use a new game, or back the save up first. Never advise removing mods from a
   save in progress without a backup.
7. With the person's agreement: snapshot, then `agora mod disable <ID> <FILE>`. Ask them to
   launch from the Agora app. If it still crashes, `agora mod enable <ID> <FILE>` puts the mod
   back before you try the next idea.

If nothing implicates a mod, consider the Java version, graphics drivers, allocated memory
(`agora --json instance recommend-memory <ID>`), shaders, or a damaged world.

## Other games and load orders

```bash
agora --json games list
agora --json games instance list
agora --json games instance check <ID>
agora --json games instance plugins <ID>
agora --json games instance plugins check <ID>
agora --json games instance plugins conflicts <ID>
agora --json crash analyze <FILE> --instance <ID>
```

On Linux, Steam starts the game. Put `agora games wrap %command%` in the game's Steam launch
options, then `agora --json games instance launch <ID>` asks Steam to start it and reports whether
the wrapper is there. If it is not, give the person the exact text to paste and where; Agora only
reads Steam's files. `agora games instance default <ID>` chooses the instance a Steam launch uses.
The game's Windows user files and crash logs live inside the Proton prefix under
`steamapps/compatdata/<appid>/pfx`. Every Linux launch says that Proton has not been tested on real
hardware yet, and a step-down to linked or copied files is announced with its reason.

`games instance check` is the pre-launch answer. It names a framework built for another game
version, a load order a launch would refuse, the game's declared INI warnings, and stale or
unknown tool output, and says which command rebuilds a tool. It also counts plugin conflicts and
warns when a pair is new since the last launch. `plugins check` prints the load-order findings;
`plugins conflicts` shows pairs of plugins that change the same record, with a winner and a loser.
It never reorders anything on its own.

- **Keep the current order:** `agora games instance plugins conflicts <ID> --keep <winner> --over <loser>`
  adds the rule that keeps this pair as it is.
- **Let the other plugin win:** `agora games instance plugins conflicts <ID> --let-win <loser> --over <winner>`
  adds the rule and moves the losing plugin after the winner now.
- **Rules:** `agora games instance plugins rule add <ID> <item> --after <other>`, `rule list` and
  `rule remove`. A rule survives every later sort; an unused one is kept and shown as unused.

Ask before changing the load order or adding a rule, and say which record pair it settles. A
valid order does not promise the game will not crash.

## Bisecting a crash

When the investigation does not name one mod, bisect: it turns half the enabled mods off per
round and the person says whether the game still crashes. This is for Minecraft instances.

```bash
agora bisect start <ID> [--suspect <FILENAME>]
agora bisect apply <ID>
agora bisect status <ID>
agora bisect good <ID>
agora bisect bad <ID>
agora bisect back <ID>
agora bisect cancel <ID>
```

`start` plans the session and moves nothing. `apply` switches the round's mods off, so ask first.
The person launches the game themselves, then `good` or `bad` narrows the suspects. `cancel` at
any point puts every mod back, and it also works on a locked instance.

## Mod dependencies, updates and pins

```bash
agora --json mod graph <ID>
agora mod orphans <ID>
agora mod why <ID> <FILE>
agora mod disable-plan <ID> <FILE>
agora mod enable <ID> <FILE> --with-deps
agora instance updates <ID>
agora instance updates <ID> --apply
agora instance updates <ID> --apply --yes
agora mod pin <ID> <FILE>
agora mod unpin <ID> <FILE>
agora mod changelog <ID> <FILE>
agora pack update-preview <ID> <MRPACK>
agora pack update <ID> <MRPACK> --yes
```

The graph, orphans, why, disable-plan, changelog and check commands only read. Use `disable-plan`
before a disable so the person sees what else goes with it. `instance updates --apply` and
`pack update` print their plan and change nothing until `--yes`. Every conflict in a pack update
needs `--keep-ours` or `--take-theirs`, and the person should choose. A pin keeps a file out of
update checks until it is unpinned.

## Instance maintenance and templates

```bash
agora instance health --all
agora instance revert <ID>
agora instance loader-version <ID> <VERSION>
agora instance java <ID> [--memory <MB>] [--jvm-args <ARGS>]
agora instance wrapper <ID> [<COMMAND> | --clear]
agora template list
agora template create <NAME> --from <ID> --include <FILE>...
agora template apply <TEMPLATE_ID> <ID>
agora template delete <TEMPLATE_ID>
```

`revert` puts an unlocked instance back to its lock snapshot, so confirm first. `loader-version`
shows its plan and asks; pass `--yes` only once the person agrees. `template apply` takes an undo
snapshot first, and `template delete` asks unless given `--yes`.

## Packs, launcher imports and backups

- **Export:** `agora pack export <ID> --format mrpack|agora|lockfile --out <FILE>`. An Agora pack
  can leave out mods with no catalog or Modrinth source, and the CLI asks before exporting them.
- **Review, then import:** `agora pack review <FILE>` prints the plan and a review id. Hard
  problems always refuse. Import only what the person has read, with
  `agora pack import <REVIEW_ID>`. Do not add `--import-anyway` unless they accept what is left out.
- **Launchers:** `agora import launchers` lists what can be imported, `agora import plan <KEY>...`
  shows what would be copied and prints a fingerprint, and `agora import run <KEY>... --fingerprint <HASH>`
  imports. Run it only with the fingerprint from the plan the person saw.
- **Backups:** `agora backup export <ID> --out <DIR>` writes a snapshot as one zip, and
  `agora backup import <FILE>` restores it as a snapshot without touching the live instance.
  `agora backup prune <ID> --keep <N>` deletes old snapshots, so it needs the person's yes.

## Finding and installing content

```bash
agora --json mod search <QUERY> --mc-version <VERSION>
agora mod install <PROJECT> <ID> --dry-run
```

Show the plan — what will be installed, which dependencies come with it, any conflicts — and run
it without `--dry-run` only after the person agrees. Avoid `--replace-conflicts` and
`--allow-replace` unless they have seen exactly what would be replaced.

## Governance and registry review (read-only)

```bash
agora --json governance config
agora --json governance summary <ITEM>
agora --json governance events --item <ITEM> --limit 50
agora --json governance diagnostics
agora --json registry under-review
agora --json registry reviews <ITEM>
agora --json registry audit-log --limit 50
agora --json registry resolutions --limit 50
```

These only read. The CLI cannot vote or post to GitHub, and you must not suggest a way around
that. Use them to explain why an item is under review, to show what curators said, or to check
why the catalog looks wrong. `governance diagnostics` exits nonzero when a check fails; report
which check and what it says. If a query fails because no catalog is installed, run
`agora registry sync` first.

## Technic packs, provider packs and plugin settings

- **Technic is off by default.** `agora --json technic search <QUERY>` is refused until the person
  turns on the `technic_enabled` setting. Do not turn it on, and do not turn on
  `allow_unverified_packs`, for them. Ask them to decide, and say what each one allows.
- **Installs create a new instance.** `agora technic install <SLUG>` and
  `agora pack install --provider <ID> --project <ID>` make a new instance and have no `--dry-run`.
  Show the person what the search or detail says, the Minecraft version and loader, and get their
  agreement first.
- **Zip packs are unverified unless pinned.** A Technic zip needs `--minecraft`, `--loader` and
  `--loader-version`. Without `--sha256 <HEX>` the CLI prints a warning that the download is not
  checked against a published hash. Tell the person that before you install. Pass `--sha256` only
  with a hash they got from a source they trust. Never invent one.
- **Plugin settings change plugin behaviour.** `agora --json plugin settings <PLUGIN_ID>` reads
  them. `agora plugin set <PLUGIN_ID> <KEY> <VALUE>` writes one, so ask before running it.

## Undoing a change

```bash
agora --json snapshots list <ID>
agora snapshots restore <ID> <SNAPSHOT_ID>
```

A restore replaces the instance's tracked state. Confirm the snapshot ID and its label with the
person first, and remind them that worlds are not in automatic snapshots.

## When a command refuses

| Exit code | Meaning | What to do |
|---|---|---|
| `11` | Instance is locked | The person locked it on purpose. Ask before unlocking. |
| `13` | Catalog missing or invalid | Run `agora registry sync`. |
| `20`, `21` | Offline or download failed | Ask whether they are online, or whether Lockdown Mode or Privacy settings block it. |
| `30`–`34` | Integrity or disk failure | Do not work around it. A hash mismatch means the file is not what its source published. |
| `40` | Sign-in required | Only matters for direct launch. Have them launch from the app. |
| `71` | A decision is needed | Read the message: a FOMOD installer, or a file to fetch by hand. Show the next command, and ask first. |
| `90`–`94` | Blocked by network policy | The person's Privacy settings forbid that host. Tell them rather than looking for a way around. |

Several errors share a range, so read the error text too. For anything else, the full reference
is https://agoramc.com/docs/cli.
