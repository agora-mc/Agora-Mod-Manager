# Crash doctor: architecture and design decisions

*Written 2026-11-09, after the first end-to-end run. It refines `MASTER_SPEC.md` §30; where the two
disagree, this is newer. Not committed: it is a working plan, to be folded into §30 as slices land.*

**The goal, in the owner's words:** diagnose as many crashes correctly as possible, including the
ones that are not a mod's fault. The benchmark is a measuring stick, not the target.

## 1. Where we are

Built and merged locally (not pushed):

| Piece | State |
|---|---|
| Engine: rules, consensus, answer, evidence sweep, attribution, launch ledger | Built earlier, reviewed |
| `agora crash analyze <file>` | Works end to end on a real log |
| Skyrim parsers (Crash Logger 1.0 to 1.25, Trainwreck) | 990 real logs parse, 0 failures. **No rules yet** |
| Minecraft pack, built into Agora | Parsers plus the old tool's signatures ported as rules |
| Benchmark: frozen key, strict and loose scoring, `predict` runner | Ready; first dev run was interrupted |

Not built: Skyrim rules, live wiring (a crash in a real session), the answer screen, checks that
open files in the instance, the Phostwood baseline.

## 2. One pipeline, two entry points

```
loose file  ─┐
             ├─> pick parsers ─> facts ─> rules ─> consensus ─> answer
live session ┘        ▲            ▲         ▲
                 packs (built-in   │     inspectors (open a file
                 and plugins)      │      in the instance and check it)
                              engine facts (what changed since the
                              last good launch, who owns what)
```

- **Loose file** (`crash analyze`, and a "Check a crash log" button): no instance. Names in the log
  stand for themselves. Checks that need the instance say "could not check".
- **Live session:** the same function, given three more things: the instance's resolver (a DLL or
  plugin file becomes the mod that installed it), engine facts, and real inspectors.

**Decision:** there is exactly one analyze function. The session path is that function with more
inputs, never a second pipeline. Anything a session can do that a loose file cannot must degrade to
"could not check", so the two never disagree, they only differ in how much they know.

## 3. Packs: three changes to the contract

The contract is not frozen, and today's slices exposed three limits. Fix them together, once.

**3a. More than one reader per file.** Today the first parser to read a file wins, so a plugin
cannot add its own parser for a log Agora already reads. Change:
- A parser declares `reads: "primary"` (the default) or `"extra"`.
- One primary per file. Order: built-in, then official, then the rest by plugin id. A second
  primary that claims the file is listed in the report as "also recognised by", and not run.
- Every extra runs. An extra may only emit facts in its own namespace (never `core.*`), which the
  linter enforces. This is what stops facts being counted twice.

**3b. Use a parser from a plugin you depend on.** Today a pack can only use its own parsers, so
Skyrim-only knowledge (where logs are written, which modules are the system) sits in the Creation
Engine pack. Change: a `source` may name `"<plugin id>:<parser id>"` when that plugin is a declared
dependency. Then move Skyrim's sources and roles into the Skyrim pack. A Fallout pack later reuses
the same parsers with its own paths.

**3c. Findings say what kind of cause they are.** Today a finding names a suspect or nothing. Add
`cause` to a finding:

| `cause` | Example | Default next step |
|---|---|---|
| `mod` | a DLL crashed | disable or update it |
| `version_mismatch` | plugin built for another game version | update it, or the named patch |
| `missing_requirement` | a missing master, a missing library mod | install it |
| `file` | one broken mesh or texture | replace or remove that file |
| `setting` | autosave setting, load order position | change it (shown exactly) |
| `save` | corrupted save | test on a new game |
| `memory` | page file too small, out of memory | the fix for that |
| `driver` | graphics driver at the top of the stack | update the driver |
| `game_install` | missing game files | verify the game |
| `other` | anything else, in the pack's words | pack-supplied text |

The list is closed in core (so the screen can show each kind well) with `other` as the open door.
A `cause` other than `mod` or `file` may have no suspect at all: "your page file is too small" is
a complete answer. The benchmark then scores the cause kind as well as the name, which is how the
91 non-mod answers in the key get graded instead of by hand.

## 4. Skyrim rules: the families, in the order to write them

Rules are data in the pack. Each family is one independent line of evidence; consensus rewards
agreement between families, never repetition inside one.

1. **Where it crashed.** The exception is inside a mod's DLL: that DLL, `likely`. Otherwise the
   first frame that is not the game, the system, the script extender or a logger: `possible`.
   Roles do the demoting, so `SkyrimSE.exe` and `KERNELBASE.dll` are never accused.
2. **What it was touching.** The registers and stack sections name objects. A form with a
   "modified by" chain points at the **last** plugin in the chain, `possible`, rising to `likely`
   when family 1 agrees. A mesh or texture path points at that file (`cause: file`).
3. **Built for another version.** The second biggest cause in the key (SkyClimb, QuickLootRE and
   friends). Do **not** ship a table of mod versions: it rots. Every SKSE plugin declares which
   game versions it supports inside the DLL itself. An inspector reads that declaration and
   compares it with the running game. In a live session this is a checked fact, so it may say
   `very_likely`. On a loose file it can only say "could not check", plus `possible` when the
   crashing DLL is also the top frame at startup.
4. **Crash at startup, missing master or newer plugin format.** The `0198090` / `05E1F22`
   signature. Key it on the Address Library id, with the raw address only for exact game builds.
   On its own: `possible`, with both explanations named. In a live session Agora reads the plugin
   headers itself: a master that is not in the load order, or a newer-format plugin on an older
   game without the backport mod. A confirmed header problem is `very_likely` and names the plugin.
5. **The machine, not a mod.** Out-of-memory exception codes, a graphics driver module on top.
   These produce `memory` or `driver` findings and **cap** any mod accusation at `possible`,
   because a starved machine crashes anywhere. **Corrected by measurement (Opus, same day):
   memory in use near the total is NOT evidence.** The 14 fullest dev logs (92-98% RAM) all had a
   mod as the confirmed cause; a cap on it would have hidden right answers. Only decisive signals
   (an OOM exception, `std::bad_alloc`) count.
6. **The save.** Save-loading frames plus a crash while loading: `save`, suggest a new game to
   confirm. Never names a mod on this evidence alone.
7. **Could not tell.** About a fifth of the key has the cause nowhere in the log (wrong MO2
   profile, a tool run badly). The right answer there is to say so and offer bisect. A confident
   wrong name is worse than "not sure", and the scoring must keep punishing it.

**Level discipline** (one rule for every pack): `very_likely` needs either two independent
families agreeing or one inspector that checked a real file. One family alone tops out at `likely`.

**How rules get written:** from `FACTS.md` plus the dev set only. Run dev, read the misses with
`--json --facts`, write or adjust a rule, rerun. The test set is scored only at milestones, and
nobody reads its misses.

## 5. Inspectors: Agora's real advantage

A web analyzer sees one pasted log. Agora can open the files. Inspectors are how. Each is small,
reads one file, and answers pass, fail or unknown. First three, in order of value:

1. **SKSE plugin compatibility** (family 3). Reads the plugin's declared versions from the DLL.
2. **Plugin header** (family 4). Masters and format version of an `.esp/.esm/.esl`.
3. **Mesh header** (family 2). Is the `.nif` readable at all, and for which game.

They live in the Creation Engine plugin as scripts, using the byte reads the contract already
gives parsers. If the DLL read proves too slow or awkward in a script, it is the first candidate
for the plugin's WebAssembly module, which already exists for LOOT.

## 6. Engine facts: what only a live session knows

Core writes these; packs read them like any other fact. No game words.

- `core.change`: what differs from the last launch that ended normally (mod added, removed,
  updated, load order moved, setting changed). The launch ledger already stores both sides.
  This is evidence that only **boosts** an existing suspect; it never accuses on its own.
- `core.dependency_problem`: a requirement the instance already knows is unmet. This carries the
  two Minecraft signals the port could not (curated conflicts, reverse dependencies), and gives
  Skyrim missing masters for free.
- `core.ownership`: which mod provides a file, including "nothing Agora installed".

## 7. Minecraft: prove, then switch

- The built-in pack stays inside Agora. Plugins can add rules and extra parsers (§3a).
- The three parsers are Rust behind the same contract a script uses. That is acceptable only
  while a plugin script could do the same job through the public contract; the slice reported no
  exception, and the cross-pack test passes.
- **Switch by comparison.** Run the old tool and the new pack over the Minecraft dev set and the
  old tool's own fixtures. Every difference gets a one-line explanation. Switch the live path when
  the new one is no worse on any case with a known cause. Then delete `crash_service.rs`'s
  signature and scoring code, keeping only what the screen still needs.
- Class-to-mod mapping needs a jar index. `health::inventory` already scans jars; expose it as a
  `ref_map`, so a stack frame's class becomes a mod without a Minecraft-only code path.

## 8. Live wiring

When a session ends and the ledger says crashed, failed to start or froze:
1. Collect evidence from every pack's sources (copying at once, since loggers delete old logs),
   plus the sweep of files changed during the session.
2. Run analyze with the instance resolver, engine facts and inspectors.
3. Store the answer with the launch record, under the retention limit.
4. Show it.

One open risk carried from earlier: a game that dies between two watcher polls can lose its exit
code. Startup crashes are exactly family 4, so test that case before trusting the wiring.

## 9. The answer screen

A variant of the existing Minecraft crash screen, never a new page.

- **Top:** one sentence, the level in plain words, and the main next step as a button.
- **Suspects:** ranked, each with its level and the evidence line that put it there.
- **Other findings:** grouped by cause kind (§3c), each ending "from <plugin name>".
- **Could not check:** collapsed, each with its reason.
- **Second opinion:** "Check a crash log" for any file, and the Phostwood button (copies the log
  and opens their page, started by the user, says what it sends).
- Wording by Opus. It never says "definitely". "Not sure" is a normal, respectable answer.

## 10. Measuring

- **Dev loop:** `predict` then `score` on the dev key, strict and loose, per game and per cause.
- **Round 2 key:** add harvest round 6 (54 confirmed cases, half non-mod). Rows already in round
  1's test set stay in test; new rows are split the same deterministic way.
- **Still owed:** a blind second read of the gold labels (Haiku), and the Phostwood baseline from
  a local copy with the network off. How its prose becomes a ranked list gets written down before
  anyone sees its test results: names in the order its report lists them.
- **Report honestly:** correct, wrong, abstained and errored, never just a hit rate. Wrong answers
  matter most.

## 11. Slices, in order

| # | Slice | Who | Notes |
|---|---|---|---|
| 1 | Gates on what is merged locally, push, read CI; spec §30.11 update | Opus runs it | The wiring commit has only been built, not tested |
| 2 | Contract changes §3a, §3b, §3c together | DeepSeek, Opus reviews | One contract bump; update both packs and the linter |
| 3 | Skyrim rule families 1, 2, 5, 6, 7 from the dev set | **Opus writes rules** | Needs no inspector; gives the first real Skyrim number |
| 4 | Dev baseline, then Phostwood baseline | DeepSeek harness, Opus reads | After #3 so there is something to compare |
| 5 | Inspectors 1 and 2, then families 3 and 4 | DeepSeek builds, Opus writes rules | The biggest expected gain |
| 6 | Engine facts §6 and live wiring §8 | DeepSeek; Sonnet review needs approval | Stateful; touches the launch path |
| 7 | Answer screen §9 | **Opus** designs and words it, DeepSeek rolls out | Screenshot the result |
| 8 | Minecraft comparison and switch §7 | DeepSeek; Opus adjudicates differences | Old tool deleted only after the owner agrees |
| 9 | Round 2 key, label second read, test-set score | Haiku reads labels; Opus freezes | Milestone measurement |

Slices 3 and 2 can run side by side (rules do not need the contract change to start). Real game
launches still need a fresh yes each time.

## 12. Housekeeping rules that now apply to every slice

- `df -h /d` before any launch; under 30 GB free, stop and say so.
- Only the four warm slots and the one integration worktree keep a build folder.
- After a merge: delete the branch, release the slot, delete the task's scratch folder.
- The corpus, the keys and every real log stay outside every repository.

## 13. Removing mods can corrupt saves (owner, 2026-11-10)

**Users must be told this** (owner's point): every answer that suggests disabling or bisecting
says, in plain words, that removing a mod partway through a playthrough can damage the save, and
to back it up or test on a new game first. Built for the CLI answer text on 2026-11-10; the answer
screen (§9) carries it too.

Disabling a mod mid-playthrough can break the save (Skyrim especially; Minecraft worlds too), so
isolating a cause by removal can make things worse. Every next step and every bisect follows this:
- **Classify before suggesting.** Asset-only mods (textures, meshes, sounds) are safe to test on
  the real save. Mods with records or scripts (`.esp/.esm/.esl`, `.pex`, SKSE DLLs; in Minecraft,
  mods adding blocks, items, entities or dimensions) are not: suggest them only with a backup save,
  a new game, or a copy of the world.
- **Snapshot first.** Any experiment Agora runs backs up the saves and the instance, and restores
  both afterwards. The answer screen says this before the user starts.
- **Prefer evidence that needs no experiment:** pre-launch checks (missing masters, DLL declared
  versions, Oldrim meshes by `.nif` header), cross-crash history, and what changed since the last
  good launch (§6). These are also Agora's real edge over log-only analyzers.
- Reproducing on a fresh character at the crash cell (`coc`) is the later, safe way to test; it
  needs the owner's yes per launch.
