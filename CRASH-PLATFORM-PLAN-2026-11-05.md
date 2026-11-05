# Open platform, crash doctor, Vortex and Cyberpunk: the plan

Written 2026-11-05 from three research reports (linked at the end). This is a draft for the owner's review. Once approved, it becomes a new `MASTER_SPEC.md` section.

## The goal

1. A community developer can add a game as well as we can.
2. Agora's crash doctor beats every Skyrim analyzer on a benchmark we can show, with Phostwood as a fallback button.
3. Cyberpunk 2077 and a Vortex importer prove the first two, because Cyberpunk is nothing like Skyrim.

## What the research changed

| I assumed | What we found |
|---|---|
| Community parity is nearly there | **It isn't.** A JSON Cyberpunk plugin loads and finds your real install, but it wouldn't give a working modded game. `MASTER_SPEC` §26.11 claims parity is "structural", and that isn't true yet |
| Phostwood is the bar | **Two bars:** Phostwood for knowledge, Tullius CTD Logger (GPL, active) for engineering |
| Someone has accuracy numbers | **Nobody does.** Our benchmark would be the first |
| BSA contents are out of reach | Easy: 52,002 files read from 115 archives in 0.6 s |
| Agora knows what changed since the last good launch | **Not yet.** Launches record no file list, so this must be built |
| The importer can purge Vortex's deployment (§26.10) | **Drop that.** We never write to a real install, and 1,594 files exist only in your game folder, including mod settings |

## Phases

Backend slices go to DeepSeek, then a failing-tests review. UI goes to Opus. The owner tests anything that needs a real game.

### Phase 1: open the platform

- **Load orders:** `load_orders: []` replaces the single closed `plugin_list`. Any number of them, in any location, with named rule sets.
- **Conditions:** one small condition language used everywhere: `{present}`, `{framework}`, `{version}`. First use is launch arguments (`-modded` only when REDmods exist).
- **Tools:** tools get inputs, scope and "must run before launch". Tool status only considers inputs that matter to it.
- **Content rules go live:** place a file by its type or marker file (`*.archive` to `archive/pc/mod/`).
- **Package inheritance:** make `parents` real, so a game package can build on a base.
- **Script reach:** plugin scripts get `games.*`, so they can read game instances. Unused `GamePackage` hooks get wired up or deleted.
- **Guide and rule:** a game-author guide with a decision tree, plus a check that official plugins use only what is documented.

### Phase 2: split the plugins

`agora.creation-engine` (base) and `agora.skyrim-se`. Installing Skyrim brings the base along. Everything keeps working, proven by the existing Skyrim tests.

### Phase 3: know what happened

- **Launch manifest:** record what each launch ran with, so a crash can be compared with the last good launch.
- **Crash detection:** Crash Logger exits with code 1 and logs its process id, so a log can be matched to a launch exactly. Agora's own Stop also exits 1, so Stop has to be recorded. The normal-quit code needs one real launch to confirm.
- **Keep the evidence:** copy each matched log into Agora's store at once, because Crash Logger deletes old ones.
- **Evidence sweep:** every text or log file changed during the session, compared with the last clean session, and attributed to its mod.
- **Archive index:** list BSA and BA2 contents, so an asset path can be traced to its mod.

### Phase 4: the crash doctor

- **Benchmark first.** Corpus manifest, a redaction pass, and Phostwood run unmodified in a local headless browser. This gives us its baseline before most of our analyzer exists.
- **Engine:** facts model, rule evaluator, independent evidence that must agree, and the ability to say "not sure".
- **Parsers:** the Crash Logger family, with a drift detector for format changes. Then Skyrim rules keyed on Address Library IDs, so they survive game updates. We write our own rules.
- **Answers:** a named mod, confidence, plain advice, and repairs that always ask first. Guided bisect when logs can't answer.
- **Phostwood button:** sends the crash log only, and says so. Test the hand-off first; I've only read the code.

### Phase 5: Vortex and Cyberpunk

- **Importer:** reads a copy of Vortex's data (never the original, because opening it changes it). Generic across Vortex games.
- **Clean base:** built by subtracting what Vortex deployed from your install, using GOG's file list. Stray files are reported, never silently lost. Vortex keeps working.
- **Cyberpunk plugin:** a JSON plugin written only from the public guide.
- **Testing:** a small 20-mod profile first, then your full 1,335 mods.
- **Archive conflicts:** real Cyberpunk conflicts live inside `.archive` files (only 7 of 9,925 paths conflict between mods), so the conflict view needs an archive scanner to be honest.

## Accuracy we can honestly expect

The research agent's judgement, not a measurement:

- First answer right 45-60% of the time, top three 65-80%.
- 20-30% of crashes can't be diagnosed from any log, so those go to bisect.
- 100 labelled cases can only prove a lead of 15 points or more. A 10-point lead needs about 235.
- **Bar for "better":** top-1 at least 55% and 10 points above Phostwood, top-3 at least 75%. Also measured: false alarms on unrelated logs, and how often the tool stays silent.
- **The button:** demote it when we pass that bar. Remove it only when local, opt-in "did this help?" answers and click counts back that up.

## Real-game tests needing the owner (a fresh yes each time)

1. **Skyrim:** one normal launch and quit, for the exit code.
2. **Skyrim:** Stop with SKSE, then a real crash under the VFS, using a test plugin that crashes on purpose.
3. **Cyberpunk:** vanilla under the VFS, then frameworks, archive order, `redMod`, and a crash.
4. **Vortex:** check it still works afterwards.

## Risks

- **Silent wrong results in Cyberpunk.** One missed rule could change which archive wins with no error. Hence the small profile and the swap test.
- **Corpus effort.** Reddit and Nexus were blocked for agents, so the gold-case yield is an estimate.
- **Label contamination.** Many "solved" threads were solved using Phostwood, which flatters it. Tracked per case.
- **Curation load.** Rules need upkeep as mods and game versions change, so rule data ships separately from the app.
- **Licences.** CLASSIC (CC BY-NC) and its sample logs must never enter the repo. The Address Library name database has no licence, so we match on IDs and don't ship its names.

## Reports

- `D:/tmp/oceval/crash/teardown.md`: what each analyzer does
- `D:/tmp/oceval/crash/formats-eval.md`: log formats, detection, sweep, benchmark
- `D:/tmp/oceval/crash/vortex-cyberpunk.md`: Vortex, Cyberpunk, schema gaps
