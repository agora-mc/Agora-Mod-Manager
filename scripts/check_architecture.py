#!/usr/bin/env python3
"""Architecture boundary enforcement for the Agora monorepo.

No dependencies beyond Python 3 stdlib.

Checks:
1. Desktop uses no raw reqwest::Client::new/builder (must use agora_core::http_client)
2. Desktop uses no rusqlite::Connection/Connection::open (must use core db helpers)
3. CLI uses no rusqlite::Connection/Connection::open
4. Desktop modules don't duplicate core service logic (hard-fail for unknown dupes;
   documented thin adapters allowed, merely listed)
5. No orphaned source modules — every *.rs file under desktop/src-tauri/src/
   (except lib.rs, main.rs) must be declared as `pub mod` in lib.rs
6. No direct HTTP request building in mod_install/modrinth_raw/crash_investigator
7. Core crate has no tauri dependency
8. Tauri binding name-manifest check runs (typed signatures explicitly waived)
9. Update checks have one core implementation
10. Live instance reads use the canonical manifest loader
11. Game API depends only on serde, serde_json, semver, thiserror and regex (all dependency sections)
12. Core must not reference the future Minecraft package's modules or crate
13. Game packages' remaining agora-core references stay within a budget that only shrinks
14. Process-wide state (`static`, `thread_local!`, `OnceLock`, `LazyLock`, `task_local!`) in
    non-test code of core and the Minecraft package is on a checked-in allowlist
15. Fail-closed reads (error-read-as-absent sites) in recovery, trust, lease, GC and staging
    code stay within a budget that only shrinks (MASTER_SPEC §32.13)
"""

import json
import os
import re
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

DESKTOP_SRC = REPO_ROOT / "desktop" / "src-tauri" / "src"
CORE_SRC = REPO_ROOT / "crates" / "agora-core" / "src"
CLI_SRC = REPO_ROOT / "crates" / "agora" / "src"
# Game packages hold game logic that used to live in core; the rules that
# scanned core scan them too, or code moving out of core would escape them.
MC_SRC = REPO_ROOT / "crates" / "agora-game-minecraft" / "src"

CORE_CARGO = REPO_ROOT / "crates" / "agora-core" / "Cargo.toml"
GAME_API_CARGO = REPO_ROOT / "crates" / "agora-game-api" / "Cargo.toml"

# ---- Documented thin adapter modules ---------------------------------------
# These modules share a name with a crate/agora-core module but are explicitly
# allowed because they only re-export types and/or bridge AppHandle -> Ctx.
# They are NOT duplicates of core business logic.
THIN_ADAPTER_MODULES: set[str] = {
    "crash_export",
    "auth",
    "crash_diagnostics",
    "crash_investigator",
    "dependency_ops",
    "governance",
    "instances",
    "launcher_profiles",
    "loader_manifests",
    "modrinth_raw",
    "paths",
    "registry",
    "registry_sync",
    "technic",
    "version_cache",
}

# Modules that must not contain HTTP-request-building patterns.
# These should delegate all network access to core service methods.
HTTP_FREE_MODULES: set[str] = {
    "mod_install",
    "modrinth_raw",
    "crash_investigator",
}

EXIT_CODE = 0


def err(msg: str) -> None:
    global EXIT_CODE
    EXIT_CODE = 1
    print(f"ERROR: {msg}", file=sys.stderr)


def warn(msg: str) -> None:
    print(f"NOTICE: {msg}")


# ---------------------------------------------------------------------------
# 1. Raw reqwest::Client in desktop
# ---------------------------------------------------------------------------

def check_reqwest_desktop() -> None:
    pat = re.compile(r'reqwest::Client::(?:new|builder)\b')
    hits: list[str] = []
    for path in sorted(DESKTOP_SRC.rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        for lineno, line in enumerate(text.splitlines(), 1):
            if pat.search(line):
                rel = path.relative_to(REPO_ROOT)
                hits.append(f"  {rel}:{lineno}: {line.strip()}")
    if hits:
        err("Desktop uses raw reqwest::Client::new/builder — use agora_core::http_client checked helpers")
        for h in hits:
            print(h, file=sys.stderr)
    else:
        print("OK: No raw reqwest::Client in desktop code")


# ---------------------------------------------------------------------------
# 2. rusqlite::Connection in desktop
# ---------------------------------------------------------------------------

def check_rusqlite_desktop() -> None:
    pat = re.compile(r'(?:rusqlite::Connection|Connection::open(?:_with_flags)?)\b')
    hits: list[str] = []
    for path in sorted(DESKTOP_SRC.rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        for lineno, line in enumerate(text.splitlines(), 1):
            if pat.search(line):
                rel = path.relative_to(REPO_ROOT)
                hits.append(f"  {rel}:{lineno}: {line.strip()}")
    if hits:
        err("Desktop uses rusqlite::Connection directly — use core db helpers via core services")
        for h in hits:
            print(h, file=sys.stderr)
    else:
        print("OK: No direct rusqlite::Connection usage in desktop code")


# ---------------------------------------------------------------------------
# 3. rusqlite::Connection in CLI
# ---------------------------------------------------------------------------

def check_rusqlite_cli() -> None:
    if not CLI_SRC.exists():
        return
    pat = re.compile(r'(?:rusqlite::Connection|Connection::open(?:_with_flags)?)\b')
    hits: list[str] = []
    for path in sorted(CLI_SRC.rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="replace")
        for lineno, line in enumerate(text.splitlines(), 1):
            if pat.search(line):
                rel = path.relative_to(REPO_ROOT)
                hits.append(f"  {rel}:{lineno}: {line.strip()}")
    if hits:
        err("CLI uses rusqlite::Connection directly — CLI must use core services")
        for h in hits:
            print(h, file=sys.stderr)
    else:
        print("OK: No direct rusqlite::Connection usage in CLI code")


# ---------------------------------------------------------------------------
# 4. Duplicate module names between desktop and core
# ---------------------------------------------------------------------------

def check_duplicate_modules() -> None:
    desktop_mods: set[str] = set()
    lib_rs = DESKTOP_SRC / "lib.rs"
    if lib_rs.exists():
        text = lib_rs.read_text(encoding="utf-8")
        for m in re.finditer(r'^\s*pub\s+mod\s+(\w+)', text, re.MULTILINE):
            desktop_mods.add(m.group(1))

    core_files = {
        p.stem
        for root in (CORE_SRC, MC_SRC)
        if root.exists()
        for p in root.rglob("*.rs")
        if p.stem not in ("lib", "mod")
    }

    shared = desktop_mods & core_files
    unknown = shared - THIN_ADAPTER_MODULES

    if unknown:
        err(
            f"Desktop modules {sorted(unknown)} duplicate core modules "
            "but are NOT in the documented thin-adapter allow-list. "
            "Either migrate to a thin adapter pattern or add to THIN_ADAPTER_MODULES"
        )
    elif shared:
        known = shared & THIN_ADAPTER_MODULES
        warn(
            f"Known thin-adapter modules found in both desktop and core: "
            f"{sorted(known)} (allowed)"
        )
    else:
        print("OK: No unexpected module-name duplication between desktop and core")


# ---------------------------------------------------------------------------
# 5. Orphaned source modules (files not declared in lib.rs)
# ---------------------------------------------------------------------------

def check_orphaned_modules() -> None:
    lib_rs = DESKTOP_SRC / "lib.rs"
    declared: set[str] = set()
    if lib_rs.exists():
        text = lib_rs.read_text(encoding="utf-8")
        # Extract names from `pub mod <name>;` lines
        for m in re.finditer(r'^\s*pub\s+mod\s+(\w+)', text, re.MULTILINE):
            declared.add(m.group(1))
        # Extract names from grouped `pub use` re-exports, e.g.
        # `pub use agora_core::{download, error, loader_manifests, models};`
        for m in re.finditer(r'pub\s+use\s+(?:\w+::)*\{([^}]+)\}', text):
            for name in m.group(1).split(","):
                name = name.strip()
                if name:
                    declared.add(name)
        # ...and single-name re-exports, e.g.
        # `pub use agora_game_minecraft::loader_manifests;`
        for m in re.finditer(r'^\s*pub\s+use\s+(?:\w+::)+(\w+)\s*;', text, re.MULTILINE):
            declared.add(m.group(1))
    # Files in a subdirectory module (desktop/src-tauri/src/games/) are declared by that
    # directory's mod.rs rather than by lib.rs, so those declarations count too.
    for mod_rs in DESKTOP_SRC.rglob("mod.rs"):
        text = mod_rs.read_text(encoding="utf-8")
        for m in re.finditer(r'^\s*pub\s+mod\s+(\w+)', text, re.MULTILINE):
            declared.add(m.group(1))

    found_files: set[str] = set()
    for p in DESKTOP_SRC.rglob("*.rs"):
        if p.name in ("lib.rs", "main.rs", "mod.rs"):
            continue
        found_files.add(p.stem)

    orphaned = found_files - declared
    if orphaned:
        err(
            f"Source files present on disk but missing from lib.rs `pub mod` "
            f"or `pub use` declarations: {sorted(orphaned)}"
        )
    else:
        print("OK: All desktop source files are declared in lib.rs")


# ---------------------------------------------------------------------------
# 6. HTTP request building in known adapter modules
# ---------------------------------------------------------------------------

def check_http_free_modules() -> None:
    suspicious_pats = [
        re.compile(r'\.(?:get|post|put|delete|patch|head|options)\s*\(\s*"https?://'),
        re.compile(r'\.send\s*\(\s*\)'),
        re.compile(r'reqwest::'),
    ]
    hits: list[str] = []
    for mod_name in HTTP_FREE_MODULES:
        mod_path = DESKTOP_SRC / f"{mod_name}.rs"
        if not mod_path.exists():
            continue
        text = mod_path.read_text(encoding="utf-8", errors="replace")
        for lineno, line in enumerate(text.splitlines(), 1):
            stripped = line.strip()
            if stripped.startswith("//") or stripped.startswith("#["):
                continue
            for pat in suspicious_pats:
                if pat.search(stripped):
                    rel = mod_path.relative_to(REPO_ROOT)
                    hits.append(f"  {rel}:{lineno}: {stripped}")
                    break
    if hits:
        err(
            f"HTTP request-building patterns found in modules that should "
            f"only delegate to core ({', '.join(sorted(HTTP_FREE_MODULES))})"
        )
        for h in hits:
            print(h, file=sys.stderr)
    else:
        print(
            f"OK: No HTTP request-building in {', '.join(sorted(HTTP_FREE_MODULES))}"
        )


# ---------------------------------------------------------------------------
# 7. Core crate must not depend on Tauri
# ---------------------------------------------------------------------------

def check_core_no_tauri() -> None:
    if not CORE_CARGO.exists():
        warn("agora-core Cargo.toml not found — cannot check tauri dependency")
        return
    text = CORE_CARGO.read_text(encoding="utf-8")
    if re.search(r'^\s*tauri\b', text, re.MULTILINE):
        err("agora-core Cargo.toml lists 'tauri' as a dependency — core must remain host-independent")
    else:
        print("OK: agora-core has no tauri dependency")


# ---------------------------------------------------------------------------
# 8. Tauri binding manifest check (name-presence only; typed sigs waived)
# ---------------------------------------------------------------------------

def check_tauri_bindings_manifest() -> None:
    """Verify the checked-in tauri-commands.json exists and has the expected
    schema_version, and warn that typed-signature generation is explicitly
    waived (deferred).  Actual diff-checking is delegated to
    check_tauri_bindings.py --check , which runs as a separate CI step."""
    manifest_path = (
        REPO_ROOT / "desktop" / "src-tauri" / "gen" / "tauri-commands.json"
    )
    if not manifest_path.exists():
        err(
            f"Tauri binding manifest not found at {manifest_path.relative_to(REPO_ROOT)}. "
            "Run `python scripts/check_tauri_bindings.py --generate` to create it."
        )
        return
    try:
        data = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        err(f"Tauri binding manifest is not valid JSON: {exc}")
        return
    if data.get("schema_version") != 1:
        err(f"Unexpected tauri-commands schema version (got {data.get('schema_version')}, expected 1)")
    else:
        print(
            "OK: tauri-commands.json manifest exists (name-presence only; "
            "typed signatures are intentionally waived pending a proc-macro solution)"
        )


# ---------------------------------------------------------------------------
# 9. Single update-check implementation
# ---------------------------------------------------------------------------

# The one module allowed to decide "does this installed item have an update?".
UPDATE_CHECK_OWNER = "update_cache.rs"
UPDATE_CHECK_OWNER_DIR = MC_SRC

# Entry points that answer that question. An adapter reaching for either is
# building a second implementation.
UPDATE_CHECK_PATTERNS = [
    # Only core constructs the IPC payload; adapters pass it through.
    (re.compile(r"\bUpdateInfo\s*\{"), "constructs UpdateInfo"),
    # The bounded resolver call the duplicate used to make directly. Matches
    # the call, not resolver.rs's definition of the method.
    (
        re.compile(r"\.list_curated_versions_for_update\s*\("),
        "calls Resolver::list_curated_versions_for_update",
    ),
]


def check_single_update_check() -> None:
    """Fail if anything outside agora-core's update_cache.rs re-implements the
    update-check matching rules.

    The rules once existed twice: in core's background sweep (which drives the
    instance update badge) and again in the desktop `check_instance_updates`
    command (which drives the update panel). They were identical when written
    and nothing enforced it, so a fix to one would have silently drifted from
    the other and shown up to the user as the badge disagreeing with the panel.
    Both now route through
    `agora_game_minecraft::update_cache::check_single_instance_updates_with`.
    """
    search_roots = [DESKTOP_SRC, CLI_SRC, CORE_SRC, MC_SRC]
    hits: list[str] = []
    for root in search_roots:
        if not root.exists():
            continue
        for path in sorted(root.rglob("*.rs")):
            if path.parent == UPDATE_CHECK_OWNER_DIR and path.name == UPDATE_CHECK_OWNER:
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            for lineno, line in enumerate(text.splitlines(), 1):
                stripped = line.strip()
                if stripped.startswith("//") or stripped.startswith("///"):
                    continue
                for pat, what in UPDATE_CHECK_PATTERNS:
                    if pat.search(stripped):
                        rel = path.relative_to(REPO_ROOT)
                        hits.append(f"  {rel}:{lineno}: {what}: {stripped}")
                        break
    if hits:
        err(
            "Update-check logic found outside crates/agora-game-minecraft/src/"
            f"{UPDATE_CHECK_OWNER} — there must be exactly one implementation, "
            "or the update badge and the update panel can disagree. Call "
            "agora_game_minecraft::update_cache::check_single_instance_updates_with "
            "instead"
        )
        for h in hits:
            print(h, file=sys.stderr)
    else:
        print(
            "OK: update-check matching rules live only in "
            f"agora-game-minecraft/src/{UPDATE_CHECK_OWNER}"
        )


# ---------------------------------------------------------------------------
# 10. Single InstanceManifest loader (lazy backfill)
# ---------------------------------------------------------------------------

# Only helpers.rs may deserialize an InstanceManifest from disk. Every other
# site must go through helpers::read_manifest so that heal_pack_managed and
# the Unknown-origin synthesis happen exactly once, lazily, and without a
# startup mass-rewrite. See helpers.rs: read_manifest.
INSTANCE_MANIFEST_OWNER = "helpers.rs"
ALLOW_RAW_MANIFEST_FILES = {
    # helpers.rs itself is the shared loader
    CORE_SRC / "helpers.rs",
    # models.rs contains the struct definition and tests that deserialize
    # legacy JSON fixtures (e.g., legacy_manifest_json) — not instance files
    CORE_SRC / "models.rs",
    # snapshot.rs reads SnapshotManifest, not InstanceManifest; allow to avoid
    # false positives on similar names. If it ever reads InstanceManifest from
    # an archive, that is intentionally exempt (archived manifest, not live).
    CORE_SRC / "snapshot.rs",
}
# Pattern for raw deserialization of an InstanceManifest
INSTANCE_MANIFEST_PATTERNS = [
    re.compile(r"InstanceManifest"),
]


def check_instance_manifest_raw() -> None:
    """Fail if anything outside helpers.rs deserializes an InstanceManifest raw.

    The healed value (pack_managed, Unknown origin) must exist in memory after
    any read, but the file on disk must not be rewritten until the next write.
    That is only guaranteed if every live read goes through helpers::read_manifest.
    """
    search_roots = [DESKTOP_SRC, CLI_SRC, CORE_SRC, MC_SRC]
    hits: list[str] = []
    for root in search_roots:
        if not root.exists():
            continue
        for path in sorted(root.rglob("*.rs")):
            if path in ALLOW_RAW_MANIFEST_FILES:
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            lines = text.splitlines()
            for lineno, line in enumerate(lines, 1):
                stripped = line.strip()
                if stripped.startswith("//") or stripped.startswith("///"):
                    continue
                # Allow an explicit escape hatch for archived manifests. rustfmt
                # moves a trailing comment onto its own line inside a multi-line
                # call, so look at a small window rather than just this line —
                # otherwise the hatch silently stops working after a reformat.
                window = lines[max(0, lineno - 2) : lineno + 2]
                if any("allow-raw-instance-manifest" in entry for entry in window):
                    continue
                # Any mention of InstanceManifest near a serde_json
                # deserialization is raw. The deserialization is frequently on
                # the *next* line, because rustfmt breaks
                #   let mut manifest: InstanceManifest =
                #       serde_json::from_str(&text)...
                # across two lines. Matching only the annotation's own line
                # missed 28 real sites, so scan a small window.
                # A word match: `GameInstanceManifest` (generic instances) is a
                # different type with no pack backfill, and must not trip this.
                if not re.search(r"\bInstanceManifest\b", line):
                    continue
                window = chr(10).join(lines[lineno - 1 : lineno + 2])
                if "from_str" in window or "from_slice" in window or "from_reader" in window:
                    rel = path.relative_to(REPO_ROOT)
                    hits.append(f"  {rel}:{lineno}: {stripped}")
                    continue
    if hits:
        err(
            "Raw InstanceManifest deserialization found outside "
            f"{INSTANCE_MANIFEST_OWNER} — use helpers::read_manifest so that "
            "heal_pack_managed and the Unknown-origin synthesis happen lazily. "
            "If this is an archived manifest (e.g., snapshot restore), add "
            "`// allow-raw-instance-manifest` to the line."
        )
        for h in hits:
            print(h, file=sys.stderr)
    else:
        print(f"OK: InstanceManifest deserialization only in {INSTANCE_MANIFEST_OWNER} (and allowed tests)")


# ---------------------------------------------------------------------------
# 11–12. Game package dependency boundaries
# ---------------------------------------------------------------------------

def dependency_tables(manifest: dict):
    """Include development/build and target-specific dependencies, too."""
    for kind in ("dependencies", "dev-dependencies", "build-dependencies"):
        yield from manifest.get(kind, {}).items()
    for target in manifest.get("target", {}).values():
        yield from dependency_tables(target)


def resolved_dependency(name: str, specification, workspace: dict) -> tuple[str, dict]:
    if isinstance(specification, dict) and specification.get("workspace"):
        specification = workspace.get("workspace", {}).get("dependencies", {}).get(name, specification)
    details = specification if isinstance(specification, dict) else {}
    return details.get("package", name), details


def check_game_api_dependencies() -> None:
    # serde_json and regex are for the crash contract: a pack is hand-written JSON, and its linter
    # compiles rule regexes (MASTER_SPEC §30.11). Both are plain libraries, not a runtime.
    allowed = {"serde", "serde_json", "semver", "thiserror", "regex"}
    if not GAME_API_CARGO.exists():
        err("agora-game-api Cargo.toml missing")
        return
    try:
        manifest = tomllib.loads(GAME_API_CARGO.read_text(encoding="utf-8"))
        workspace = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        err(f"Cannot inspect agora-game-api dependencies: {exc}")
        return
    forbidden = []
    for alias, specification in dependency_tables(manifest):
        package, details = resolved_dependency(alias, specification, workspace)
        # A local crate named serde is not the approved contract dependency.
        if package not in allowed or "path" in details or "git" in details:
            forbidden.append(alias)
    if forbidden:
        err(f"agora-game-api dependencies must be serde, serde_json, semver, thiserror or regex only: {sorted(forbidden)}")
    else:
        print("OK: agora-game-api depends only on serde, serde_json, semver, thiserror and regex")


def is_game_package(name: str, path: str = "") -> bool:
    if name in ("agora-game-api", "agora_game_api"):
        return False
    if name.startswith("agora-game-") or name.startswith("agora_game_"):
        return True
    path_norm = path.replace("\\", "/").rstrip("/")
    if "agora-game-" in path_norm and not path_norm.endswith("agora-game-api"):
        return True
    return False


def check_core_no_game_packages() -> None:
    """Core never references a game package: adapters register packages into
    core's `GameRegistry` through agora-game-api, never the other way round.
    Check renamed dependencies as well as direct/import/include references."""
    hits = []
    if CORE_CARGO.exists():
        try:
            manifest = tomllib.loads(CORE_CARGO.read_text(encoding="utf-8"))
            workspace = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        except (OSError, tomllib.TOMLDecodeError) as exc:
            err(f"Cannot inspect core game package dependencies: {exc}")
            return
        for alias, specification in dependency_tables(manifest):
            package, details = resolved_dependency(alias, specification, workspace)
            path_str = details.get("path", "")
            if is_game_package(package, path_str):
                hits.append(f"  {CORE_CARGO.relative_to(REPO_ROOT)}: dependency {alias}")
    pattern = re.compile(r"\bagora_game_(?!api\b)\w+\b|agora-game-(?!api\b)[\w-]+")
    # Tests and build scripts are part of core's dependency boundary too.
    for path in sorted(CORE_CARGO.parent.rglob("*.rs")):
        # Ignore comments (including multi-line block comments), but retain
        # newlines so reported source locations remain useful.
        source = path.read_text(encoding="utf-8")
        source = re.sub(r"/\*.*?\*/", lambda m: "\n" * m.group().count("\n"), source, flags=re.S)
        for lineno, line in enumerate(source.splitlines(), 1):
            if pattern.search(line.split("//", 1)[0]):
                hits.append(f"  {path.relative_to(REPO_ROOT)}:{lineno}: {line.strip()}")
    if hits:
        err("agora-core references game package modules — packages register through agora-game-api")
        for hit in hits:
            print(hit, file=sys.stderr)
    else:
        print("OK: agora-core does not reference any game package modules")


check_core_no_minecraft_package = check_core_no_game_packages


# ---------------------------------------------------------------------------
# 13. Game packages' remaining dependence on agora-core only shrinks
# ---------------------------------------------------------------------------
# MASTER_SPEC §26.12: a game package depends on agora-game-api, never on
# agora-core. Slice 2 moved Minecraft out of core but left
# agora-game-minecraft using core's types and services; each use moves onto a
# host service when another game needs that service, and the budget is zero by
# Phase 5. Until a package's
# budget reaches zero it may depend on agora-core, and its count of
# `agora_core` references may only fall. A lower count must be written back
# into the budget file, so a gain cannot be quietly given back.
GAME_PACKAGE_CORE_BUDGET = REPO_ROOT / "scripts" / "game_package_core_budget.json"
CRATES_DIR = REPO_ROOT / "crates"


def count_core_references(package_dir: Path) -> int:
    """`agora_core` identifiers in a package's Rust code, comments excluded."""
    total = 0
    for sub in ("src", "tests", "benches", "examples"):
        root = package_dir / sub
        if not root.exists():
            continue
        for path in sorted(root.rglob("*.rs")):
            source = path.read_text(encoding="utf-8", errors="replace")
            source = re.sub(r"/\*.*?\*/", "", source, flags=re.S)
            for line in source.splitlines():
                total += len(re.findall(r"\bagora_core\b", line.split("//", 1)[0]))
    build = package_dir / "build.rs"
    if build.exists():
        total += len(re.findall(r"\bagora_core\b", build.read_text(encoding="utf-8")))
    return total


def depends_on_core(cargo_toml: Path) -> bool:
    try:
        manifest = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
        workspace = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return True  # unreadable: assume the worst
    for alias, specification in dependency_tables(manifest):
        package, details = resolved_dependency(alias, specification, workspace)
        if package == "agora-core" or details.get("path", "").rstrip("/").endswith("agora-core"):
            return True
    return False


def check_game_package_core_budget() -> None:
    try:
        budget = json.loads(GAME_PACKAGE_CORE_BUDGET.read_text(encoding="utf-8"))
    except FileNotFoundError:
        budget = {}
    except (OSError, ValueError) as exc:
        err(f"Cannot read {GAME_PACKAGE_CORE_BUDGET.name}: {exc}")
        return
    problems = []
    report = []
    packages = sorted(
        p for p in CRATES_DIR.glob("agora-game-*")
        if p.name != "agora-game-api" and (p / "Cargo.toml").exists()
    )
    for package in packages:
        name = package.name
        count = count_core_references(package)
        allowed = budget.get(name)
        if allowed is None:
            if depends_on_core(package / "Cargo.toml") or count:
                problems.append(
                    f"  {name}: depends on agora-core ({count} references) but has no "
                    f"entry in {GAME_PACKAGE_CORE_BUDGET.name}; packages reach core "
                    "through agora-game-api only"
                )
            continue
        if count > allowed:
            problems.append(
                f"  {name}: {count} agora_core references, budget {allowed}; "
                "this budget only shrinks"
            )
        elif count < allowed:
            problems.append(
                f"  {name}: {count} agora_core references, below the budget of "
                f"{allowed}; lower it to {count} in {GAME_PACKAGE_CORE_BUDGET.name}"
            )
        elif count == 0 and depends_on_core(package / "Cargo.toml"):
            problems.append(
                f"  {name}: no agora_core references left; remove the agora-core "
                "dependency and the budget entry"
            )
        else:
            report.append(f"{name} {count}")
    for name in sorted(set(budget) - {p.name for p in packages}):
        problems.append(f"  {name}: budget entry for a package that does not exist")
    if problems:
        err("Game package dependence on agora-core (MASTER_SPEC §26.12)")
        for problem in problems:
            print(problem, file=sys.stderr)
    elif report:
        print("OK: game packages within their agora-core budget: " + ", ".join(report))
    else:
        print("OK: no game package depends on agora-core")


# ---------------------------------------------------------------------------
# 14. Process-wide state in core and the Minecraft package is on the allowlist
# ---------------------------------------------------------------------------
# MASTER_SPEC §32.9: a test seam lives on `Ctx` (or is passed in), because a
# process-wide `static` lets one test's hook catch another test's code, and a
# `thread_local!` cannot reach code the operation runs on another thread.
# Configuration is moving onto `Ctx` too. What is left is written down with the
# reason it really has to be process-wide: a guard that is one per process by
# definition, a cache, a counter, or configuration that has not moved yet.
PROCESS_STATE_ALLOWLIST = REPO_ROOT / "scripts" / "process_state_allowlist.json"

# The item kinds §32.9 names: a `static` (which is also how `OnceLock` and
# `LazyLock` state is declared, and what each `thread_local!`/`task_local!` block
# holds), wherever it sits: at module level, inside a function body, or in a
# `thread_local!`/`task_local!` block.
PROCESS_STATE_ITEM = re.compile(r"(?<!')\bstatic\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)")
# The items a function-local static can sit in. A `;` before the `{` means a
# declaration without a body (a trait method), which owns nothing.
ENCLOSING_ITEM = re.compile(r"\b(?:fn|mod)\s+([A-Za-z_][A-Za-z0-9_]*)[^{;]*\{")


def process_state_items(root: Path) -> list[tuple[Path, int, str, str]]:
    """Process-wide items in the non-test code under `root`, as (file, line, name, owner).

    `owner` is the `fn`/`mod` an item sits inside (the innermost one), or `""` at
    module level. A static inside a function is keyed by that function, so a
    fifth `static RE` beside four existing ones in one file does not read as an
    entry that is already allowed.

    Test code is skipped through the shared scanner: only the item a
    `#[cfg(test)]` attribute applies to is blanked, so production code after a
    test helper is still counted.
    """
    items: list[tuple[Path, int, str, str]] = []
    for path in sorted(root.rglob("*.rs")):
        try:
            source = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        cleaned = blank_test_items(blank_rust_noise(source))
        owners = [
            (match.start(), item_end(cleaned, match.start()), match.group(1))
            for match in ENCLOSING_ITEM.finditer(cleaned)
        ]
        for match in PROCESS_STATE_ITEM.finditer(cleaned):
            owner = ""
            owner_start = -1
            for start, end, name in owners:
                if end is not None and start < match.start() < end and start > owner_start:
                    owner, owner_start = name, start
            line = cleaned.count("\n", 0, match.start()) + 1
            items.append((path, line, match.group(1), owner))
    return items


def check_process_state_allowlisted() -> None:
    try:
        allowlist = json.loads(PROCESS_STATE_ALLOWLIST.read_text(encoding="utf-8"))
    except FileNotFoundError:
        allowlist = {}
    except (OSError, ValueError) as exc:
        err(f"Cannot read {PROCESS_STATE_ALLOWLIST.name}: {exc}")
        return
    found: dict[str, dict[str, str]] = {}
    for root in (CORE_SRC, MC_SRC):
        for path, _line, name, owner in process_state_items(root):
            relative = path.relative_to(REPO_ROOT).as_posix()
            # A static inside a function is keyed by that function, so a fifth
            # `static RE` in the same file cannot pass as an existing entry.
            key = f"{relative}::{owner}::{name}" if owner else f"{relative}::{name}"
            # A name declared twice in one owner (rare) gets its own numbered entry.
            count = 1
            while key in found:
                count += 1
                key = f"{relative}::{owner}::{name}#{count}" if owner else f"{relative}::{name}#{count}"
            found[key] = {"category": name}
    unknown = sorted(set(found) - set(allowlist))
    stale = sorted(set(allowlist) - set(found))
    bad_entry = sorted(
        key
        for key, entry in allowlist.items()
        if key in found
        and (
            entry.get("category") not in {"config", "guard", "cache", "counter", "constant"}
            or not entry.get("why")
        )
    )
    if unknown or stale or bad_entry:
        err("Process-wide state in agora-core / agora-game-minecraft (MASTER_SPEC §32.9)")
        for key in unknown:
            print(
                f"  {key}: a static or thread-local that is not on the allowlist. A test "
                "seam belongs on `Ctx` (or is passed in), and configuration is moving there "
                f"too. Add it to {PROCESS_STATE_ALLOWLIST.name} with a category "
                "(config|guard|cache|counter|constant) and the reason it is process-wide",
                file=sys.stderr,
            )
        for key in stale:
            print(f"  {key}: allowlisted but no longer found; remove the entry", file=sys.stderr)
        for key in bad_entry:
            print(
                f"  {key}: allowlist entry needs a category "
                "(config|guard|cache|counter|constant) and a `why`",
                file=sys.stderr,
            )
    else:
        print(
            f"OK: {len(found)} process-wide items in core and the Minecraft package, "
            "each allowlisted"
        )


# ---------------------------------------------------------------------------
# 15. Fail-closed reads: error-read-as-absent sites may only go down
# ---------------------------------------------------------------------------
# MASTER_SPEC §32.13. Most late review findings were one class of bug: an error
# read as "nothing there" (`.exists()`, `.ok()`, `unwrap_or_default()` on a
# read). In recovery, trust, lease, garbage-collection and staging code those
# patterns decide whether user data may be deleted or overwritten, so they are
# counted per file and may only fall: a lower count must be written back, the
# same rule as the game-package budget above.
#
# What the count is, exactly: the three patterns, matched textually in the
# file's production code.
#
# - Comments and the insides of string and char literals are blanked before
#   counting, so `// p.exists()` and a `".ok()"` message string count nothing.
# - Only the item a `#[cfg(test)]` attribute applies to is blanked: an inline
#   `mod tests { ... }` with its braces, a `#[cfg(test)] mod helper;`
#   declaration line, or a `#[cfg(test)] fn ...` test-only helper. Everything
#   after it is still counted — a test-module declaration early in a file must
#   not hide the production code below it. An attribute inside a parameter list
#   (`fn present(#[cfg(test)] _c: usize, p: &Path)`) applies to that parameter
#   alone, so only the attribute is blanked.
# - Whitespace inside a call is allowed (`p.exists ()`, `.ok ( )`).
#
# The count is deliberately textual: telling a filesystem result from a parse
# result needs types, and a count that needs a compiler is not a count any
# contributor can run. It may therefore count a harmless site, which a slice can
# remove or leave, but it does not miss one.
#
# The list is the modules the spec names: recovery (`startup_recovery`, the
# journals and their passes, `backup`), trust (the signed registry and plugin
# signatures), leases and locks (the runtime lease, the recorded process, the OS
# lock, the session ledger), garbage collection (`snapshot`, `game_base`,
# Recently deleted) and staging (`game_user_files`, `game_tool_swap`, plugin
# installs, `content_store`). The authority for the list is MASTER_SPEC §32.13;
# a slice that converts a whole module adds it here, and one that splits a file
# moves its entry.
FAIL_CLOSED_READ_BUDGET = REPO_ROOT / "scripts" / "fail_closed_read_budget.json"
FAIL_CLOSED_READ_PATTERNS = (
    re.compile(r"\.exists\s*\(\s*\)"),
    re.compile(r"\.ok\s*\(\s*\)"),
    re.compile(r"\.unwrap_or_default\s*\(\s*\)"),
)
# A `#[cfg(test)]` attribute, with any spacing.
CFG_TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
# An attribute group, e.g. `#[allow(clippy::too_many_lines)]` on the same item.
ATTRIBUTE = re.compile(r"#\s*!?\s*\[[^\]]*\]")


def blank_rust_noise(source: str) -> str:
    """Comments and the insides of string/char literals replaced by spaces.

    Line structure is kept, so the result lines up with the input. What is left
    cannot contain a pattern that only appears in a comment or a message string.
    """
    out: list[str] = []
    index = 0
    while index < len(source):
        pair = source[index : index + 2]
        if pair == "//":
            end = source.find("\n", index)
            end = len(source) if end == -1 else end
            out.append(" " * (end - index))
            index = end
        elif pair == "/*":
            # Rust block comments nest.
            depth = 0
            end = index
            while end < len(source):
                if source[end : end + 2] == "/*":
                    depth += 1
                    end += 2
                elif source[end : end + 2] == "*/":
                    depth -= 1
                    end += 2
                    if depth == 0:
                        break
                else:
                    end += 1
            out.append("".join("\n" if c == "\n" else " " for c in source[index:end]))
            index = end
        elif source[index] == "r" and re.match(r'r#*"', source[index:]):
            # A raw string: r"..." or r#"..."#, closed by a quote and its hashes.
            hashes = re.match(r'r(#*)"', source[index:]).group(1)
            close = '"' + hashes
            end = source.find(close, index + len(hashes) + 2)
            end = len(source) if end == -1 else end + len(close)
            out.append("".join("\n" if c == "\n" else " " for c in source[index:end]))
            index = end
        elif source[index] == '"':
            end = index + 1
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            out.append("".join("\n" if c == "\n" else " " for c in source[index:end]))
            index = end
        elif source[index] == "'" and source[index + 1 : index + 2] != "":
            # A char literal ('x', '\n', '\\', '\u{..}') or a lifetime ('a). Only a
            # literal closes on its own quote, so a lifetime is left alone.
            if source[index + 1] == "\\":
                close = source.find("'", index + 2)
            else:
                close = index + 2 if source[index + 2 : index + 3] == "'" else -1
            if close == -1:
                out.append(source[index])
                index += 1
            else:
                out.append(" " * (close + 1 - index))
                index = close + 1
        else:
            out.append(source[index])
            index += 1
    return "".join(out)


def item_end(source: str, start: int) -> int | None:
    """One past the end of the item starting at `start`: its `;` or its brace block."""
    brace = source.find("{", start)
    semicolon = source.find(";", start)
    if semicolon != -1 and (brace == -1 or semicolon < brace):
        return semicolon + 1
    if brace == -1:
        return None
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return index + 1
    return None


def inside_parentheses(source: str, position: int) -> bool:
    """Whether `position` sits inside a `(` that is still open — a parameter list.

    An attribute there applies to the parameter it precedes, not to the item after the list.
    """
    stack: list[str] = []
    for char in source[:position]:
        if char in "([{":
            stack.append(char)
        elif char in ")]}" and stack:
            stack.pop()
    return stack[-1:] == ["("]


def blank_test_items(source: str) -> str:
    """Blank what a `#[cfg(test)]` attribute applies to, and only that.

    A test module sits in the middle of some files (`content_store/mod.rs` declares one before its
    production code), so dropping everything from the first test attribute to the end of the file
    would hide real sites. Each attribute is followed to its own item instead.
    """
    while True:
        match = CFG_TEST_ATTRIBUTE.search(source)
        if not match:
            return source
        if inside_parentheses(source, match.start()):
            # `fn present(#[cfg(test)] _c: usize, p: &Path) -> bool { p.exists() }`: the attribute
            # applies to the parameter only, so blank it alone and keep counting the body.
            source = (
                source[: match.start()]
                + " " * (match.end() - match.start())
                + source[match.end() :]
            )
            continue
        cursor = match.end()
        while True:
            whitespace = len(source[cursor:]) - len(source[cursor:].lstrip())
            cursor += whitespace
            further = ATTRIBUTE.match(source, cursor)
            if not further:
                break
            cursor = further.end()
        end = item_end(source, cursor)
        if end is None:
            # Nothing that looks like an item follows: blank the attribute only, so the count
            # still sees whatever comes next.
            end = match.end()
        source = (
            source[: match.start()]
            + "".join("\n" if c == "\n" else " " for c in source[match.start() : end])
            + source[end:]
        )


def count_fail_closed_reads(source: str) -> int:
    """Error-read-as-absent sites in one Rust file, comments and test items excluded."""
    cleaned = blank_test_items(blank_rust_noise(source))
    return sum(len(pattern.findall(cleaned)) for pattern in FAIL_CLOSED_READ_PATTERNS)


def count_fail_closed_reads_in(path: Path) -> int:
    return count_fail_closed_reads(path.read_text(encoding="utf-8", errors="replace"))


def check_fail_closed_reads() -> None:
    try:
        budget = json.loads(FAIL_CLOSED_READ_BUDGET.read_text(encoding="utf-8"))
    except FileNotFoundError:
        err(f"{FAIL_CLOSED_READ_BUDGET.name} is missing: the fail-closed read budget is checked in")
        return
    except (OSError, ValueError) as exc:
        err(f"Cannot read {FAIL_CLOSED_READ_BUDGET.name}: {exc}")
        return
    files = budget.get("files", {})
    directories = budget.get("directories", {})
    problems: list[str] = []
    report: list[str] = []

    def compare(relative: str, count: int, allowed: int) -> None:
        if count > allowed:
            problems.append(
                f"  {relative}: {count} error-read-as-absent sites, budget {allowed}; "
                "this budget only shrinks. Use probe() or match the error instead"
            )
        elif count < allowed:
            problems.append(
                f"  {relative}: {count} error-read-as-absent sites, below the budget of "
                f"{allowed}; lower it to {count} in {FAIL_CLOSED_READ_BUDGET.name}"
            )
        else:
            report.append(f"{relative} {count}")

    for relative, allowed in sorted(files.items()):
        path = REPO_ROOT / relative
        if not path.is_file():
            problems.append(f"  {relative}: budgeted but not found; correct or remove the entry")
            continue
        if not isinstance(allowed, int):
            problems.append(f"  {relative}: budget must be a whole number, not {allowed!r}")
            continue
        compare(relative, count_fail_closed_reads_in(path), allowed)
    for relative, expected in sorted(directories.items()):
        root = REPO_ROOT / relative
        if not root.is_dir():
            problems.append(f"  {relative}/: budgeted directory not found")
            continue
        if not isinstance(expected, dict):
            problems.append(f"  {relative}/: budget must map each file to its count")
            continue
        # Every Rust file below the directory, not just the ones beside it: a
        # recovery module that grows a subdirectory must be counted, not skipped.
        present = {
            path.relative_to(root).as_posix(): path for path in sorted(root.rglob("*.rs"))
        }
        for name in sorted(set(present) - set(expected)):
            problems.append(
                f"  {relative}/{name}: not in the budget "
                f"({count_fail_closed_reads_in(present[name])} sites); add its count to "
                f"{FAIL_CLOSED_READ_BUDGET.name}"
            )
        for name, allowed in sorted(expected.items()):
            if name not in present:
                problems.append(f"  {relative}/{name}: budgeted but not found")
            elif not isinstance(allowed, int):
                problems.append(f"  {relative}/{name}: budget must be a whole number")
            else:
                compare(f"{relative}/{name}", count_fail_closed_reads_in(present[name]), allowed)

    if problems:
        err("Fail-closed reads in recovery, trust, lease, GC and staging code (MASTER_SPEC §32.13)")
        for problem in problems:
            print(problem, file=sys.stderr)
    elif report:
        print(f"OK: fail-closed reads within budget in {len(report)} files")
    else:
        print("OK: no fail-closed read budget to check")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
def main() -> int:

    print("--- 1. Raw reqwest::Client in desktop ---")
    check_reqwest_desktop()
    print()

    print("--- 2. rusqlite::Connection in desktop ---")
    check_rusqlite_desktop()
    print()

    print("--- 3. rusqlite::Connection in CLI ---")
    check_rusqlite_cli()
    print()

    print("--- 4. Duplicate module names (desktop <-> core) ---")
    check_duplicate_modules()
    print()

    print("--- 5. Orphaned source modules in desktop ---")
    check_orphaned_modules()
    print()

    print("--- 6. HTTP request-building in adapter modules ---")
    check_http_free_modules()
    print()

    print("--- 7. Core crate tauri dependency ---")
    check_core_no_tauri()
    print()

    print("--- 8. Tauri binding name manifest ---")
    check_tauri_bindings_manifest()
    print()

    print("--- 9. Single update-check implementation ---")
    check_single_update_check()
    print()

    print("--- 10. Single InstanceManifest loader (lazy backfill) ---")
    check_instance_manifest_raw()
    print()

    print("--- 11. Game API contract dependencies ---")
    check_game_api_dependencies()
    print()

    print("--- 12. Core has no game package references ---")
    check_core_no_game_packages()

    print("\n--- 13. Game packages' agora-core budget ---")
    check_game_package_core_budget()
    print()

    print("--- 14. Process-wide state is on the allowlist ---")
    check_process_state_allowlisted()
    print()

    print("--- 15. Fail-closed reads in recovery, trust, lease, GC and staging code ---")
    check_fail_closed_reads()
    print()

    if EXIT_CODE == 0:
        print("All architecture boundary checks passed.")
    else:
        print(f"FAIL: {EXIT_CODE} architecture boundary violation(s) found.", file=sys.stderr)

    return EXIT_CODE


if __name__ == "__main__":
    sys.exit(main())
