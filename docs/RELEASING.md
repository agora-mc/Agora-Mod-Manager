# Releasing Agora

Agora has three independent release streams:

| Stream | Typical tag | Contents |
| --- | --- | --- |
| Catalog | `registry-YYYY-MM-DD` | Signed catalog database and web export |
| Desktop | `vX.Y.Z` | Platform installers and application bundles |
| Official plugin | `official-<name>-v<version>` | One plugin package zip, such as Skyrim SE |

Do not treat a successful catalog release as proof that a desktop package was built correctly, or vice versa. Because all three streams publish releases to the same repository, neither the catalog sync nor the desktop updater uses GitHub's `/releases/latest` endpoint: catalog sync scans `/releases` for the newest `registry-YYYY-MM-DD` release carrying `registry.bundle`, and the desktop updater reads `latest.json` from the dedicated `updater` prerelease (`releases/download/updater/latest.json`).

## Official plugin releases

Skyrim SE is two official plugins (`packages/official/creation-engine/`, plugin id
`agora.creation-engine`, and `packages/official/skyrim-se/`, plugin id `agora.skyrim-se`), not part
of the app build. Like the desktop app, they are released by hand: the `Official plugins` workflow
([`.github/workflows/official-plugins.yml`](../.github/workflows/official-plugins.yml)) runs when a
tag `official-<name>-v<version>` is pushed (for example `official-skyrim-se-v0.1.0`), or when it is
run by hand from the Actions tab with a package folder name. Merging a change to
`packages/official/**` publishes nothing, so a half-finished update never reaches players. The
workflow builds that one package zip reproducibly, verifies that
`registry/official-plugins/<plugin-id>.json` pins those exact bytes, refuses a tag that does not
match the package's version, and attaches the zip to the `official-<name>-v<version>` release. If
that release already holds the same bytes it does nothing; if it holds different bytes it fails,
because a published package is never replaced.

That release URL is the one the signed registry names, so the asset must be published before a
catalog that lists it. The catalog compile enforces this: it downloads every `package_url` in
`registry/official-plugins/`. A missing, private or unreachable asset leaves that plugin entry out
of the catalog and logs a warning (shown as a GitHub Actions `::warning::` annotation in the
nightly run); the rest of the catalog still compiles. A package whose bytes do not match the pinned
`size` or `sha256` is a hard failure, because that means the registry entry itself is wrong.

To release a new plugin version:

1. bump `version` in the package's `agora-plugin.json`;
2. run `python scripts/build_official_plugin.py <name> --update-registry`, which rebuilds the zip,
   rotates the old `current` entry to `previous`, and rewrites the registry entry;
3. commit both files and merge them to master;
4. push the tag `official-<name>-v<version>` on that commit (or run the workflow by hand on
   master). A plugin whose parent changed too, such as `skyrim-se` on top of `creation-engine`,
   needs the parent released first;
5. the next catalog compile (nightly, or run it by hand) lists the new version.

`python scripts/build_official_plugin.py <name> --verify-registry` is the check to run before
committing: it rebuilds the zip and refuses a registry entry whose hash, size or URL does not match.

## Desktop release checklist

### Windows portable desktop

The Windows desktop job also publishes
`agora-desktop-vX.Y.Z-windows-x64-portable.zip` and its `.zip.sha256` checksum.
This is distinct from the standalone CLI ZIP. The archive contains the signed desktop
executable, `portable.txt`, and instructions; extract it into a writable folder and run
`Agora Mod Manager.exe`. WebView2 Runtime must already be installed. The normal MSI/NSIS
installers remain installed applications; portable mode is distributed separately.

Data and WebView preferences live in the adjacent `data` folder unless `AGORA_DATA_DIR`
overrides it. Installed data is not automatically moved into a portable copy. A portable
copy never uses the OS keyring: Microsoft and GitHub sign-ins are encrypted in the `data`
folder so they travel with it, which means anyone who can read that folder can read them.
Close any other running Agora copy
before starting the portable one (the application enforces a single instance).

To update, close Agora, replace the executable from the next portable ZIP, and keep
`portable.txt` and `data`. The portable update action explains this instead of running
an installer. Before publishing, smoke-test extraction, first launch, moving the folder,
preference persistence, and this update path on Windows. Verify the extracted executable's
Authenticode signature and the ZIP checksum.

Fresh appearance preferences and **Reset appearance** use **Civic Gold (default)**.
Existing saved appearance choices are preserved; the previous blue preset is **Agora Blue**.

### Windows VFS files

Games start under `agora_vfs.dll` (MASTER_SPEC §26.5). The Windows desktop installers and the
portable ZIP carry the 64-bit `agora_vfs.dll` and `agora-vfs-helper.exe` beside the application,
and the 32-bit pair in an `x86\` folder beside them, which 32-bit tools such as Nemesis need.
The Windows release job builds both pairs from `agora-vfs` (the second for
`i686-pc-windows-msvc`), signs each file, and stages them with `scripts/stage_vfs_files.py`,
which refuses a file of the wrong bitness. `desktop/src-tauri/tauri.bundle-vfs.conf.json` names
them as bundle resources; it is merged only into the Windows matrix entry. The Windows CLI
archive (`agora-cli-*-windows-x64.zip`) carries the same layout beside `agora.exe`, built and
signed in its own job, so a CLI-only install can start games. That archive now has `agora.exe`
at its root, as the Unix archives do; before, it held the `target\...\release` path. Placement:
Tauri's `resource_dir` on Windows is the executable's directory, and NSIS installs resources
under `$INSTDIR` at their target paths, so `x86\` sits beside the executable (see
[Tauri resources](https://v2.tauri.app/develop/resources/)). A 32-bit game started without its
x86 files is refused, and the reason names both expected paths. Check that on the packaged build
before publishing.

### Windows code signing

Windows releases use Azure Artifact Signing with GitHub OIDC. The shared setup is
in `.github/actions/windows-signing/action.yml`; the Azure identifiers there are
public configuration, not secrets. The signing profile is `Agora-MC-Launcher` in
account `Agora-MC-Launcher`, resource group `Agora`, at
`https://wus2.codesigning.azure.net` (West US 2).

The Entra application `Agora GitHub Release Signing` needs the **Artifact Signing
Certificate Profile Signer** role on this signing account. Its federated credential
uses issuer `https://token.actions.githubusercontent.com`, audience
`api://AzureADTokenExchange`, and this immutable GitHub subject:

```text
repo:agora-mc@309633368/Agora-Mod-Manager@1272711399:environment:windows-signing
```

Enable immutable subjects in the repository's GitHub Actions OIDC settings so the
issued token matches this credential. The `windows-signing` GitHub environment
allows selected **tags** matching `v*`. Both release build jobs use this environment;
only their Windows matrix entries authenticate to Azure and invoke signing. For
manual dispatch, select the actual release tag as the workflow ref, not `master`.

No client secret or exported code-signing private key is required. The setup downloads
a version- and SHA-256-pinned Microsoft signing client and verifies its NuGet signature.
It uses the signed Windows SDK SignTool installed on the hosted Windows runner.

Immediately after Azure login, the shared action obtains an access token for
`https://codesigning.azure.net/.default` and leaves it in Azure CLI's runner-local
cache, with token output suppressed. This exchange must happen while the GitHub
OIDC assertion is still valid: waiting until the first signing call after Rust
compilation caused `AADSTS700024` because the assertion had already expired.
The signing hook's `AzureCliCredential` then uses the cached service access token.
That access token also has a finite lifetime; if a much longer build outlasts it,
authenticate again and acquire a fresh service token immediately before packaging.

For desktop builds, a temporary Tauri configuration supplies a custom signing hook.
Tauri signs the application and installer executables during packaging, before
creating updater signatures. Tauri restores the original unsigned main executable
after bundling, so the workflow separately signs and verifies
`target/release/agora-desktop.exe` before creating the portable ZIP. The portable
packager still rejects invalid signatures or missing timestamps.
Each hook invocation verifies Authenticode and requires
a timestamp. The Windows CLI is signed and verified before its ZIP is created, so
the release checksums cover the signed binary. Signing or verification failure fails
the job; it never falls back to an unsigned Windows release. Local development builds
do not load this temporary signing configuration.

The existing `TAURI_SIGNING_PRIVATE_KEY` and optional password remain required for
Tauri updater signatures; Azure Authenticode signing does not replace them.

Offline hook regression checks: `pwsh -NoProfile -File scripts/test_windows_signing.ps1`.
These test error propagation and path handling with a fake signing tool, not Azure
authentication. The first tagged release must verify the live OIDC exchange and
signing permissions. Before publishing its draft, check the Windows installers and
extracted CLI with `Get-AuthenticodeSignature`; status must be `Valid`, the signer
must match the profile's validated identity, and a timestamp must be present. Also
test a packaged update from the prior release.

References: [Microsoft signing integration](https://learn.microsoft.com/en-us/azure/artifact-signing/how-to-signing-integrations),
[immutable OIDC subjects](https://learn.microsoft.com/en-us/entra/workload-id/workload-identities-github-immutable-subjects),
[Tauri custom signing hook](https://v2.tauri.app/distribute/sign/windows/#custom-sign-command).

### Before tagging

- [ ] Version metadata agrees across package files (`python scripts/set_release_version.py --check`; also enforced by the `version-metadata` CI job).
- [ ] Changelog or release notes describe user-visible changes.
- [ ] Required public build variables are present in the release workflow.
- [ ] Signing and updater configuration are available for intended platforms, including Apple Silicon and Intel macOS.
- [ ] The in-app guide and website documentation match current labels.
- [ ] CLI help and `docs/CLI.md` match current commands.
- [ ] Migration from the previous public release has been tested with disposable data.
- [ ] A clean installation has been tested.
- [ ] A packaged upgrade has been tested.

### Checks that gate a release build

No release artifact is built until these three workflows pass **for the commit being
released**. The `Release` workflow calls them with `uses:`, so they are the same
workflows that gate ordinary pushes and pull requests — not a second copy that can
drift — and they run against this run's checkout rather than a previous run's:

| Required check | Covers |
| --- | --- |
| [`CI Enforcement`](../.github/workflows/ci-enforcement.yml) | `cargo fmt`, clippy for core/CLI/desktop, Rust tests on Linux/Windows/macOS, compiler and script tests, TypeScript and Vite build, architecture boundaries, Tauri binding manifest, documentation gates, version-metadata agreement |
| [`Launch Planner CI`](../.github/workflows/launch-planner.yml) | Cross-platform launch-planner integration tests and the `launchMode` delegation-default assertion |
| [`E2E Tests`](../.github/workflows/e2e.yml) | The Playwright browser suite |

Two properties make "the checks passed for the released revision" a fact rather than
an inference:

- Because the checks are *called* rather than looked up, a stale green run on an
  earlier commit cannot stand in for this one, and a missing, pending, failed, or
  cancelled run leaves the build jobs unreachable. Path filters do not apply to a
  called workflow, so a change that no filter matches — a loader-only revision, for
  example — is still fully tested at release time.
- The `release-ref` job asserts that the release tag resolves to the commit this run
  is building. A tag push satisfies this by construction; a manual dispatch from a
  branch whose HEAD is not the tagged commit fails here instead of publishing
  artifacts that no check ever examined. Push the tag first, then dispatch against it.

`loader-manifests/`, `runtime-catalog/`, and `crash-signatures/` are `include_str!`
inputs to `agora-core`, so they are also in `CI Enforcement`'s push and pull-request
path filters. A loader-refresh commit therefore gets the full suite on `master`, not
only `Launch Planner CI`.

The `Web Build` and `Nightly Compiler` workflows are not release gates: they serve the
catalog and website streams on their own schedules.

### Build and inspect

The `Release` workflow in [`.github/workflows/release.yml`](../.github/workflows/release.yml) runs for a pushed `v*` tag or a manual dispatch with a tag. Its first build step rewrites the version from that tag into every file that carries one — the workspace `Cargo.toml`, `desktop/src-tauri/Cargo.toml`, `tauri.conf.json`, `desktop/package.json`, and the `Cargo.lock` workspace entries — via `scripts/set_release_version.py`. The tag is therefore the single source of truth for installer filenames, the version the application shows, and `agora --version`. It rewrites the ephemeral CI checkout only; nothing is committed back. After the required checks above pass, it builds native desktop bundles on Windows, macOS, and Linux with the CLI from the same build inside them (staged by `scripts/stage_cli_sidecar.py` and merged in through `desktop/src-tauri/tauri.bundle-cli.conf.json`, so ordinary builds do not need it), packages standalone CLI archives, normalizes every desktop bundle name (`scripts/finalize_release_assets.py`), generates a `SHA256SUMS` covering every artifact, and assembles a draft release. The workflow never publishes the release itself; it stays a draft until a maintainer smoke-tests the artifacts and publishes explicitly.

Public desktop build variables and secret boundaries are documented once in [DEVELOPMENT.md](./DEVELOPMENT.md). Confirm the workflow has every required public value and protected signing credential without copying their values into release notes or logs.

Do not rely on fixed installer filenames or package sizes in documentation. Tauri and platform tooling can change both.

The workflow leaves the release as a draft with all artifacts uploaded and `SHA256SUMS` generated. It never publishes automatically. Inspect the draft after the workflow finishes before publishing, and cancel or re-run the workflow if any of these are wrong:

- platform and architecture coverage
- both Apple Silicon and Intel macOS installers, with architecture-bearing bundle names
- asset names match the scheme in [Release asset names](#release-asset-names)
- version shown by the application
- installer identity
- `SHA256SUMS` covers every downloadable artifact and nothing else
- updater metadata contains signed `darwin-aarch64` and `darwin-x86_64` entries
- release notes
- accidental debug artifacts

### Release asset names

Every release publishes the same asset scheme:

`agora-<kind>-<tag>-<platform>-<arch>[-<variant>].<ext>`

| Kind | Assets (full names start with `agora-<kind>-<tag>-`) |
| --- | --- |
| `desktop` | `windows-x64-setup.exe` (NSIS), `windows-x64.msi`, `windows-x64-portable.zip` (+ `.sha256`), `macos-arm64.dmg`, `macos-x64.dmg`, `macos-arm64.app.tar.gz` and `macos-x64.app.tar.gz` (updater archives), `linux-x64.AppImage`, `linux-x64.deb`, `linux-x64.rpm` |
| `cli` | `windows-x64.zip`, `linux-x64.tar.gz`, `macos-arm64.tar.gz`, `macos-x64.tar.gz` |

Tauri names its bundles `Agora Mod Manager_<version>_<arch>.<ext>` after the product
name, and GitHub stores the spaces as dots, so the uploaded names start
`Agora.Mod.Manager`. (Drafts published before the rename start `Agora.Launcher`, and
the script still accepts them.) The names say nothing about desktop vs CLI and mix
`x64`, `x86_64`, and `amd64`. The `assemble-release` job post-processes the draft with
`scripts/finalize_release_assets.py` before verification:

- It renames each `Agora.Mod.Manager*` bundle in place and fails on a name or an
  embedded version it does not recognize.
- It deletes the updater `*.sig` sidecar assets — the signatures also live
  inline in `latest.json`, which is what the updater consumes.
- It rewrites every `latest.json` platform URL to the renamed asset and
  re-uploads `latest.json`.
- It writes and uploads `SHA256SUMS`, covering every downloadable artifact
  except `latest.json` and `SHA256SUMS` itself.

GitHub lists assets alphabetically (case-insensitively, so `agora-cli-*` sorts
above `agora-desktop-*`) and collapses long lists behind "Show all N assets".
After finalizing, `assemble-release` therefore rewrites the draft's release
body with a direct-download table — Windows `-setup.exe` first — instead of
relying on the asset list for discoverability. The updater notes in
`latest.json` keep tauri-action's short summary, because the in-app update
dialog shows them as plain text. Re-running the job is safe: the script leaves
an already-normalized release alone, and re-running it after fresh bundles were
uploaded replaces the stale renamed copies.

### Packaged smoke test

Everything in this section is **manual and not enforced by CI**. The required checks
above run against source; nothing in them installs or launches a packaged build, so a
release that compiles and tests clean can still fail on first run. The one automated
exception is the macOS launch check inside `build-desktop`, which starts the packaged
`.app` for a few seconds to catch bundle and startup failures; it is not a substitute
for the steps below, and there is no Windows or Linux equivalent.

Use the actual release artifact, not `tauri dev`.

Minimum test:

1. Install or run the packaged build.
   - On macOS, launch both the Apple Silicon and Intel `.app` packages on their matching hardware.
2. Complete first-run setup on a clean disposable profile.
3. Synchronize and verify the catalog.
4. Confirm Browse returns curated content.
5. Confirm loader and Java catalogs are available.
6. Create or import a disposable instance.
7. Run health.
8. Launch through the default delegated mode.
9. Launch directly with a test Microsoft account when that platform is supported.
10. Restart and verify settings and instances persist.
11. Exercise update detection from the previous release.
12. Open Settings → Services → AI assistant and confirm it shows the bundled CLI's path, then
    run that path with `--version` and `skill`. On Windows check both the installed copy and
    the portable ZIP's `agora.exe`; the workflow already checks the macOS app and the Linux
    `.deb`, but not the Windows installer.

The catalog public-key check is release-critical. A package that builds successfully but lacks the expected verification key can fail only after installation. Always test catalog synchronization in the packaged artifact.

### Publish

The workflow leaves the release as a draft after all build jobs succeed. The desktop and CLI artifacts and `SHA256SUMS` are already attached; publication is the one manual step a maintainer performs after a packaged smoke test.

After the packaged smoke test passes, publish the draft:

- click **Publish release** on the draft's GitHub page, or
- run `gh release edit "$TAG" --draft=false --repo agora-mc/Agora-Mod-Manager`.

After publishing:

- Confirm [`.github/workflows/desktop-updater.yml`](../.github/workflows/desktop-updater.yml) publishes the new `latest.json` to the dedicated `updater` prerelease (`releases/download/updater/latest.json`) without altering its installer URLs or minisign signatures. If needed (for example, if the release was published via an automation token), run that workflow manually with `tag: vX.Y.Z` or run `python scripts/publish_updater_manifest.py --repo agora-mc/Agora-Mod-Manager --tag "$TAG"`.
- Confirm the website download control selects the intended release.
- Perform one clean public download.
- Confirm update checks can see the release.
- Monitor support channels for migration, signing, and installer failures.

## Catalog signing, rotation and expiry

The launcher installs one file, `registry.bundle`. It holds a signed header and
the SQLite database as one unit, so a crash cannot leave a database and a
signature that describe different builds. The header names the signing key id,
the schema version, the build time, the issue and expiry times, and the
database length and SHA-256. `registry.db` and `registry.db.sig` are still
published for the web export, but the launcher does not accept that two-file
form: no signed expiry and no rotation live there. A pre-bundle cache is treated
as no catalog and the next sync downloads a bundle.

### Trusted keys

The client trusts a compiled-in list of Ed25519 public keys, each named by the
first 16 hex characters of its SHA-256 and each carrying a role:

- **primary** signs catalogs in CI;
- **recovery** is kept offline and signs certificates and revocation statements,
  never a catalog.

A primary key signs catalogs and nothing else: it cannot introduce another key
or revoke one. A recovery key is the only key that may change trust.

A release build must carry at least one of each, or
`python scripts/check_registry_keys.py` fails the release. The defaults live in
`crates/agora-core/registry-keys.json`. The recovery slot is a placeholder until
the owner generates the offline key:

```bash
python scripts/generate_registry_key.py --private-key-out D:/keys/agora-recovery.key
```

That prints only the public key and writes the private key to the named path,
which must be outside this repository. Put the printed public key in the recovery
slot (or pass both keys in `AGORA_REGISTRY_PUBKEYS` on the release build). Never
run that command into the repo or save the private half in CI.

A release build can override the compiled list at compile time:

| Variable | Meaning |
| --- | --- |
| `AGORA_REGISTRY_PUBKEY` | One primary public key, hex (the older single-key variable) |
| `AGORA_REGISTRY_PUBKEYS` | Comma-separated public keys, hex; the first is primary, the rest recovery (takes precedence) |

The compiler derives the key id from `ED25519_PRIVATE_KEY`; operators never keep
a separate id in sync.

### What the client remembers

Next to the cached bundle the client keeps `registry.trust`, a line log of
recovery-signed statements: certificates that introduced keys, and revocation
statements with their signatures. A new entry is written before a verified
bundle is installed, and the log only grows: a revocation is never removed.
Every remembered line is re-verified on load by one rule. The log is a series of
records, one per verified bundle (a `v1 record` line starts each). A statement
counts when its signature chain authenticates against the trust standing before
its record, whatever the order of its lines and whether or not the same record
revokes its signer, so a key retired by an earlier record cannot sign anything in
a later one (a
recovery key introduced, used to revoke, and itself revoked in one bundle keeps
its revocations). Authenticated revocations are all enforced, and a key survives
only if nothing in its certificate chain is revoked, so revoking a recovery key
also retires every key it certified. Every later verification starts from the
compiled keys plus surviving remembered keys minus remembered revocations. A
compiled-in key that was revoked stays revoked.

A missing or empty state file means nothing is remembered yet. A state file that cannot
be read, or holds a statement that does not authenticate, is an error
(`ERR_REGISTRY_TRUST_STATE`): the catalog is not opened and a good bundle is not
quarantined, rather than falling back to a weaker set of keys. The only
exception is a torn final line (no trailing newline) that does not parse or
authenticate; the next install removes it before appending its own record. To start over, remove `registry.trust` yourself; Agora then trusts
only its compiled keys.

### Rotating a key

Rotation is a trust statement, not a client update. The recovery key is the only
key that may certify or revoke:

1. Generate the new primary key pair offline. Put the public key in the next
   client's `AGORA_REGISTRY_PUBKEYS`, and keep the old key in that list for at
   least one release cycle.
2. Sign a certificate with the **recovery** private key. The script reads
   `AGORA_REGISTRY_OLD_PRIVATE_KEY` or `--signing-key`; keep the key offline:
   `python scripts/make_registry_key_certificate.py --new-public-key <hex> --role primary --out key-certificate.json`
3. Sign a revocation of the old primary with the same recovery key. The script
   reads `AGORA_REGISTRY_RECOVERY_PRIVATE_KEY` or `--signing-key`:
   `python scripts/make_registry_revocation.py --revoked-public-key <old hex> --out revocation.json`
4. Compile the catalog with the **new** private key in `ED25519_PRIVATE_KEY`,
   `AGORA_REGISTRY_KEY_CERTIFICATES=key-certificate.json`, and
   `AGORA_REGISTRY_REVOCATIONS=revocation.json`. The new primary signs the
   catalog; the recovery-signed statements inside it are what clients trust.
5. When every supported client knows the new primary, remove the old key from
   the compiled-in list and from `AGORA_REGISTRY_PUBKEYS`.

A **compromised primary** is retired by a recovery-signed revocation (step 3)
and a recovery-signed certificate for its replacement (step 2); a stolen primary
cannot revoke the recovery key or certify anything itself. The client remembers
the revocation in `registry.trust`, so a later bundle signed by the old primary
is refused even if it omits the revocation. A **compromised recovery key** cannot
be fixed in-band: ship a client release with a new compiled recovery key. A
recovery key may be revoked by another recovery key, and may never revoke
itself. Until every client has that build, an attacker holding an old key can
still sign a bundle those clients accept; a short expiry limits how long such a
bundle remains current. Never weaken or skip signature verification.

### Expiry and stale catalogs

The compiler sets `issued_at` and `expires_at`; the default validity is 90 days,
set with `AGORA_REGISTRY_VALIDITY_DAYS` (1–3650). `AGORA_REGISTRY_TAG` overrides
the display tag.

An expired catalog is still **readable**: Browse, item details, crash
signatures and governance history keep working, and the app says the catalog is
stale. Anything that downloads or replaces software from catalog-provided URLs
and hashes refuses it: curated mod and game installs, and official plugin
install/update/rollback. The update flow itself still works, because that is how
a fresh catalog arrives.

A bundle built before the installed one is refused, and a cached bundle whose
signed build stamp cannot be read refuses the update rather than being treated
as older. The previous verified bundle is kept as `registry.bundle.prev`; a
crash during replacement, or a damaged current bundle, restores it on the next
open.

## Catalog release checklist

- [ ] Curated manifests validate.
- [ ] Governance inputs validate.
- [ ] Loader/runtime catalog inputs are current and pinned.
- [ ] Compiler tests pass.
- [ ] The catalog is signed with the production primary key.
- [ ] Any certificate or revocation in the bundle is signed by the recovery key,
      not the primary.
- [ ] The public keys expected by released clients match the signing key, and
      `python scripts/check_registry_keys.py` passes (primary and recovery).
- [ ] `registry.bundle` is present and its header names the expected key id.
- [ ] Catalog database, bundle and web export signatures verify.
- [ ] The signed expiry is in the future and long enough for the release cadence.
- [ ] Release assets use the expected names.
- [ ] A current packaged desktop client can download and open the new catalog.

The primary private key belongs only in the protected CI environment. The
recovery private key belongs offline, on the owner's machine, never in CI.
Neither belongs in documentation, issue comments, artifacts, or local shell
history.

## Rollback

### Desktop

When a desktop release is broken:

1. Stop promoting the release.
2. Document the affected platforms and user-visible failure.
3. Preserve the failed artifacts for investigation.
4. Restore the `updater` prerelease's `latest.json` to a known-good `vX.Y.Z` release (`python scripts/publish_updater_manifest.py --repo agora-mc/Agora-Mod-Manager --tag vX.Y.Z --allow-downgrade`, or dispatch `.github/workflows/desktop-updater.yml` with `allow_downgrade: true`), or publish a new patched release.
5. Test migration from both the broken and previous good versions.
6. Explain whether users must take manual action.

### Catalog

When a catalog release is invalid:

1. Do not weaken signature verification.
2. Retain the invalid release for audit unless policy requires removal.
3. Restore or republish the Last Known Good signed catalog.
4. Verify client fallback behavior.
5. Correct source manifests or compiler logic through review.
6. Publish a new signed release.

## Release documentation

Release notes should lead with player impact:

- what changed
- whether migration is automatic
- known limitations
- recovery advice
- platform-specific concerns

Detailed compiler, signing, or governance internals belong in maintainer documents, not the first screen of player release notes.
