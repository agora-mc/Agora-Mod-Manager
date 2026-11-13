# Developing Agora

This page covers local builds and validation. Player instructions belong in the in-app guide and website documentation.

## Repository map

| Path | Purpose |
| --- | --- |
| `crates/agora-core/` | Shared, game-agnostic business logic |
| `crates/agora-game-minecraft/` | Minecraft support, as a game package that registers into core |
| `crates/agora-game-creation/` | Test helper that loads the Skyrim definition from its official plugin |
| `packages/official/` | Official plugin packages released separately from the app (Skyrim SE) |
| `crates/agora/` | Standalone CLI |
| `crates/agora-plugin-api/`, `crates/agora-plugin-host/` | Plugin contract, and the QuickJS host that runs plugin scripts |
| `crates/agora-vfs/`, `crates/agora-vfs-inject/` | Windows copy-on-write VFS injected into games, and the import-table injector it shares with the launcher |
| `crates/agora-game-api/` | Experimental game package/host contract; [manifest v3 design](architecture/game-api.md) |
| `sdk/`, `examples/plugins/`, `docs/plugins/` | Plugin author types, runnable examples (exercised by core's end-to-end tests), and the author guide |
| `desktop/` | Tauri desktop application and React UI |
| `web/` | Public static directory |
| `compiler/` | Catalog compiler |
| `registry/` | Curated catalog source manifests and governance data |
| `loader-manifests/` | Pinned loader catalog inputs |
| `scripts/` | Validation and maintenance helpers |
| `docs/` | User, developer, release, and operator reference |
| `.kilo/` | AI tooling configuration; `MASTER_SPEC.md` is the engineering blueprint |
| `BACKLOG.md` | Phase-by-phase task tracker |

Keep reusable behavior in `agora-core`, and game-specific behavior in that game's package. Desktop, CLI, and MCP hosts should adapt the same services rather than implement parallel business rules.

## Prerequisites

- current stable Rust toolchain
- Node.js and npm supported by the lockfile
- Python supported by the compiler requirements
- platform prerequisites required by Tauri
- Git

Use the repository lockfiles. Avoid replacing them merely to satisfy a local global-tool mismatch.

## Build the CLI

```bash
cargo build -p agora-cli
cargo test -p agora-cli
```

See [CLI.md](./CLI.md) for usage.

## Build the desktop application

```bash
cd desktop
npm install
npm run build
npm run tauri:dev
```

A production installer must be tested as a packaged release. Development mode does not prove that compile-time variables, updater metadata, resources, signing, or platform bundles are correct.

## Build the website

Generate the root `registry-web.json` through the local compiler procedure below before building. Static export needs catalog entries to generate the dynamic catalog routes.

```powershell
cd web
npm install
$env:NEXT_PUBLIC_GITHUB_REPOSITORY = "agora-mc/Agora-Mod-Manager"
npm run build
```

On bash-compatible shells, export the same variable before `npm run build`. The site module intentionally rejects an unset repository because release, issue, and source links cannot be constructed safely without it.

The website should remain useful without the desktop application installed. It owns pre-install guidance and shareable documentation, not the complete in-app learning curriculum.

## Compile the catalog locally

```bash
cd compiler
python -m venv .venv
```

Activate the environment, then:

```bash
pip install -r requirements.txt
python compile.py --skip-sign --skip-official-asset-check --no-governance-write --governance-mode off --out ../registry.db
python ../scripts/verify_db.py
```

Use `--skip-sign` only for local development. Production clients must not trust unsigned catalog output.
Use `--skip-official-asset-check` only for local development or tests: the release compile downloads
every official plugin package, leaves out an entry whose asset is missing and warns, and refuses an
entry whose bytes do not match its pinned size or sha256.

## Official plugin packages

Skyrim SE ships as two official plugins: the Creation Engine base in
`packages/official/creation-engine/` and Skyrim in `packages/official/skyrim-se/`. Neither is a
compiled package. The desktop and CLI register only Minecraft; Skyrim arrives from the installed
plugin, which names the base as a dependency. To use them locally, install both folders as
development plugins, base first, and turn community plugins on, as described in
[plugins/README.md](plugins/README.md#official-plugins-in-this-repository).

Build each release zip with `python scripts/build_official_plugin.py <folder>`, for example
`creation-engine` or `skyrim-se`. Entries are sorted, stored uncompressed and given a fixed
timestamp and mode, so the SHA-256 is the same across builds and checkouts. `--verify-registry`
checks that `registry/official-plugins/<plugin id>.json` pins those exact bytes;
`--update-registry` rewrites the entry from the build. The `Official plugins` workflow attaches each
zip to its own `official-<folder>-v<version>` release.

## Environment boundaries

### Compiler runtime environment

The Python compiler may load `.env` for local development. Copy `.env.example` to `.env` only when local compiler work needs it, and consult `.env.example` as the authoritative variable list.

Secret examples include:

- the Ed25519 private signing key
- GitHub tokens
- Discord webhook URLs

Never commit real values.

### Desktop compile-time configuration

The Rust desktop build embeds selected public configuration at compile time. A `.env` file used by the Python compiler is not automatically a Tauri build environment. Set these as shell variables for local Tauri builds and as repository Actions variables for releases:

| Variable | Purpose | Secret? |
| --- | --- | --- |
| `AGORA_OAUTH_CLIENT_ID` | Public GitHub application client ID used by the governance device flow | No |
| `AGORA_REGISTRY_PUBKEY` | Public Ed25519 key used to verify downloaded catalog signatures | No |
| `AGORA_REGISTRY_REPO` | Catalog repository in `owner/repo` form | No |
| `VITE_AGORA_REPOSITORY` | Repository used by desktop frontend links | No |

The matching `ED25519_PRIVATE_KEY`, GitHub tokens, updater signing key, and webhook URLs are secrets. They belong only in the intended protected environment and must never be copied into documentation or screenshots. GitHub device flow does not require embedding a client secret in the native app.

Release workflows must verify the packaged executable can:

- authenticate through intended public client configuration
- verify the signed catalog
- synchronize the catalog
- populate Browse
- load loader/runtime catalog data

Do not assume a successful source build proves packaged configuration is present.

## Core validation

These mirror `.github/workflows/ci-enforcement.yml`. Run the ones covering what you touched, and
all of them before calling a substantial change done.

Repository root, hermetic and fast:

```bash
python scripts/check_architecture.py && python scripts/check_docs.py && python scripts/check_package_docs.py && python scripts/check_tauri_bindings.py --check
```

Rust:

```bash
cargo fmt --all --check
cargo clippy -p agora-core -p agora-game-api -p agora-game-minecraft -p agora-game-creation -p agora-plugin-host -p agora-cli --all-targets --all-features -- -D warnings
cargo test -p agora-core --lib --tests
cargo test -p agora-game-api -p agora-game-minecraft -p agora-game-creation --lib --tests
cargo test -p agora-plugin-host --lib --tests
cargo test -p agora-cli
cargo check -p agora-desktop
```

On Windows, also build and lint the VFS crates. The ignored matrix and real-injection tests
run in the `vfs-injection` CI job; they need no administrator rights and start only Agora's
test fixtures, never a real game:

```bash
cargo clippy -p agora-vfs -p agora-vfs-inject --all-targets -- -D warnings
cargo test -p agora-vfs -p agora-vfs-inject --lib --tests
```

The commands for the ignored tests, and what each needs, are in
[`crates/agora-vfs/README.md`](../crates/agora-vfs/README.md#tests).

Desktop frontend (from `desktop/`): `npm run build && npm run typecheck:e2e && npm run test:unit`.
`npm run build` runs the boundary checks, `tsc` and `vite build`. `typecheck:e2e` checks the
Playwright specs against `e2e/tsconfig.json`: the main `tsc` covers `src/` only, and Playwright
strips types without checking them. End-to-end tests (`npx playwright test`) are slower; run them
when touching UI flows. They use the Vite dev server, which is fine for a few specs but can be
overloaded by a full parallel run; for the full suite use `npm run test:e2e:built`, which builds and
tests the bundle the app ships. `E2E_PORT=<port>` runs either on another port than 5173.

Compiler (from `compiler/`, in a venv with `requirements.txt`):
`python compile.py --skip-sign --no-governance-write --governance-mode off --out ../registry.db && python ../scripts/verify_db.py`.

A new or renamed Tauri command also needs `python scripts/check_tauri_bindings.py --generate`.

### Gotchas

- `cargo fmt --all --check` prints diffs to stdout and exits 1; piping it through `tail` hides
  the failure.
- CI's frontend job runs `tsc` and `vite build` directly and skips the boundary checks, so
  `npm run build` locally is the only thing that enforces them.
- CI uses the latest stable Rust. Clippy runs with `-D warnings`, so a lint newer than your local
  toolchain fails CI while passing locally; `rustup update` before trusting clippy on
  lint-sensitive work.
- Lockfiles change through their package manager, never by hand. If `npm ci` complains the lock
  is out of sync, regenerate it with the npm version CI uses (npm 11 / Node 24).
- A static export of `web/` built on Windows writes its segment-prefetch files into nested
  folders (`docs/__next.docs/__PAGE__.txt` instead of `docs/__next.docs.__PAGE__.txt`), so a
  local preview logs RSC prefetch 404s even though navigation still works. This is upstream
  bug vercel/next.js#85374. The deployed site builds on Linux and is unaffected.

## Working-copy notes

- `web/public/screenshots/*.png` are generated by `desktop/e2e/docs-screenshots.spec.ts`. A
  re-render on another OS changes their bytes; commit them only when the UI they show changed.
- History under `registry/archived/` is kept, not edited.
- `loader-manifests/` is written only by the loader-refresh workflow. The one hand edit is
  approving a quarantined loader file: set `"approved": true` on its record in
  `loader-manifests/quarantine.json` (see `MASTER_SPEC.md` §27).

## Documentation validation

For a user-visible change:

1. Update `desktop/src/data/guideContent.ts`.
2. Update CLI help and `docs/CLI.md` when applicable.
3. Update website docs when first-run behavior changes.
4. Search for obsolete labels and defaults.
5. Test all newly documented commands against a temporary data root.
6. Verify links.
7. Avoid copying secret-bearing operator commands into player documentation.

Run the hermetic documentation gates:

```bash
python scripts/check_docs.py && python scripts/check_package_docs.py
```

`check_docs.py` checks current internal Markdown links, website routes, guide-topic mappings, product spelling, and known stale claims. `check_package_docs.py` checks that every key in the official and tracer game packages, and every field on the package types, is documented in `docs/plugins/game-packages.md`. Neither makes network requests or rewrites files. CLI help-contract checks run under `cargo test -p agora-cli`.

### Documentation screenshots

Documentation screenshots are generated from the current React UI with fixed synthetic Tauri responses. The fixture contains no personal profile, credentials, tokens, private paths, or private packs:

```bash
cd desktop
npx playwright test e2e/docs-screenshots.spec.ts --project=chromium --workers=1
```

Review every generated image under `web/public/screenshots/` before publishing. The browser fixture verifies interface copy and layout; use a packaged build with a disposable profile for release smoke testing.

## Development data

Prefer a separate data directory for experiments:

```bash
agora --data-dir ./tmp/agora-dev paths
```

Use disposable instances whose purpose is obvious from their name. Never run destructive tests on a real world or the normal user profile.

For a disposable desktop profile, set `AGORA_DATA_DIR` before starting Tauri:

```powershell
$env:AGORA_DATA_DIR = Join-Path $env:TEMP "agora-desktop-docs"
cd desktop
npm run tauri:dev
```

Close Agora before removing that temporary directory. Microsoft credentials use the operating-system credential store and are not isolated by `AGORA_DATA_DIR`; do not sign in or sign out with a personal account during disposable-profile tests.

## Crash benchmark

`scripts/crash_benchmark.py` scores Crash Doctor answers against a frozen answer key. It is
game-agnostic: it never parses a crash log, only compares predicted culprit names with the key.

The corpus and its answer key stay outside the repository. Test only with synthetic rows in a
temporary folder, and keep `test-key.jsonl` away from anyone still writing rules: `score --detail`
refuses that file unless `--i-am-done-writing-rules` is given.

```bash
python scripts/crash_benchmark.py freeze --labels labels.jsonl --manifest manifest.jsonl --out ../crash-key --salt "$SALT"
python scripts/crash_benchmark.py score --key ../crash-key/dev-key.jsonl --predictions predictions.jsonl --tool crash-doctor
python scripts/crash_benchmark.py compare --key ../crash-key/dev-key.jsonl tool-a=a.jsonl tool-b=b.jsonl
python scripts/crash_benchmark.py predict --manifest manifest.jsonl --corpus-root ../crash-logs --ids ../crash-key/dev-key.jsonl --out predictions.jsonl --agora "target/debug/agora.exe"
```

`freeze` keeps gold rows with a culprit and no template risk, splits them into `dev-key.jsonl`
and `test-key.jsonl` by manifest hash (so a log posted twice stays on one side), and writes
`freeze.json` with the counts and the test key's SHA-256. A label row may carry `accept`, a list
of other spellings of the same culprit; `freeze` copies it into the key, and an `accept` that is
not a list of non-empty strings is an error naming the row.

Every hit rate is shown twice. **Strict** is an exact normalised match against the culprit or one
of its `accept` entries, and keeps the original field names (`top1_hits`, `top3_hits`). **Loose**
splits the culprit and each accepted spelling at ` and `, ` with `, `&`, `,`, `/`, `(`, `)` and
`;`, normalises each word, drops words under four characters, and counts a prediction that equals
or contains a word (`loose_top1_hits`, `loose_top3_hits`). Because answers are people's words while
tools predict file names, loose is usually the fairer reading, but it can over-credit: a short
answer word inside a longer unrelated name still counts. Treat it as a measuring aid, not a
verdict; strict stays the rule for a claim. `score` reports top-1, top-3, both matching modes,
abstained, wrong (not a strict hit), loose wrong (named someone who is not even a loose match),
missing and error rows overall and by game, log type, difficulty and whether the
culprit is in the log. An error row is a tool that failed, not one that abstained. `compare` adds
an `abstain-always` baseline so a tool that answers badly can rank below silence. Pass
`--expect-sha256` to refuse a changed key, `--json` for machine output, or `--detail` for per-row
results including which rule matched.

`predict` runs a tool over the corpus and writes the predictions file. It finds each id's `file`
in the manifest under `--corpus-root` and runs `<agora> crash analyze <file> --json` with any
`--plugin-dir` and `--allow-code` options. Arguments are passed as a list, never through a shell,
so a path with spaces or quotes is safe. The first named owner of each candidate becomes a suspect
in order; candidates whose owner has no name (the system, an unknown) are skipped and counted. Per
row it also records `files[0].read_by`, so a summary can say how many logs no parser recognised.
A non-zero exit with parsable JSON is an abstention, keeping the file report's reason; a timeout, a
non-zero exit with no JSON, or unparsable JSON is an `error` row, never an abstention. `--jobs`
runs several analyses at once and rows stay in id order; `--timeout` caps each log (120 s by
default). `--out` inside a git work tree is refused, and the file is written atomically.

The Phostwood adapter that produces prediction files comes later; for now tools emit the JSONL
format themselves or go through `predict`.

## Pull-request evidence

Include the smallest relevant evidence:

- tests run
- packaged or development build used
- screenshots for UI changes
- CLI help/output for CLI changes
- migration or rollback behavior for persistent-state changes
- timing methodology for performance changes
- documentation files reviewed
