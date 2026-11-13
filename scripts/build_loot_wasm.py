#!/usr/bin/env python3
"""Build the LOOT WebAssembly module reproducibly.

The module lives in ``packages/official/creation-engine/wasm/loot-sort`` and is
built for ``wasm32-wasip1`` with the release profile in its ``Cargo.toml``
(LTO, ``opt-level=3``, ``panic=abort``, strip). Paths are remapped so the bytes
do not depend on where the repository or the cargo registry lives.

The committed module is the **Linux** build. Windows and macOS toolchains embed
different standard-library source paths, so their bytes differ even with the
same Rust version; ``--check`` therefore compares only on Linux and says so on
other operating systems instead of reporting a mismatch.

Usage::

    python scripts/build_loot_wasm.py
    python scripts/build_loot_wasm.py --offline
    python scripts/build_loot_wasm.py --check

The default mode builds twice, from two different target folders, compares the
bytes, writes the committed ``loot-sort.wasm`` and prints its SHA-256.
``--check`` rebuilds once and refuses when the committed file differs; CI runs
it on Linux, the reference platform. On another OS the default mode refuses to
overwrite the reference and ``--check`` reports that it cannot compare there.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CRATE_DIR = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-sort"
OUTPUT = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-sort.wasm"
SOURCES = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "SOURCES.md"
LOCKFILE = CRATE_DIR / "Cargo.lock"
MANIFEST = CRATE_DIR / "Cargo.toml"
TARGET = "wasm32-wasip1"
BUILT = Path("wasm32-wasip1") / "release" / "loot-sort.wasm"
DEFAULT_TARGET_ROOT = REPO_ROOT / "target"
ENCODED_SEPARATOR = "\x1f"


def is_reference_platform() -> bool:
    """Whether this OS can build and verify the committed module.

    The committed ``loot-sort.wasm`` is the Linux build. A Windows or macOS
    toolchain embeds different standard-library source paths (Windows records
    rust-src paths under the sysroot, Linux records ``/rustc/<hash>`` paths),
    so the bytes differ even with the same Rust version and remap flags. Only
    Linux can reproduce and compare the reference bytes.
    """
    return sys.platform.startswith("linux")


def cargo_home() -> Path:
    return Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))


def rust_sysroot() -> Path | None:
    """The installed toolchain's sysroot, if rustc can report it."""
    try:
        output = subprocess.run(
            ["rustc", "--print", "sysroot"],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    text = output.stdout.strip()
    return Path(text) if text else None


def remap_flags(repo_root: Path, home: Path, sysroot: Path | None = None) -> list[str]:
    """The ``rustc`` flags that make source paths location-independent."""
    registry = home / "registry" / "src"
    flags = [
        f"--remap-path-prefix={repo_root}=/agora",
        f"--remap-path-prefix={registry}=/cargo/registry/src",
    ]
    if sysroot is not None:
        flags.append(f"--remap-path-prefix={sysroot}=/rust")
    return flags


def build_environment(target_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["CARGO_INCREMENTAL"] = "0"
    flags = remap_flags(REPO_ROOT, cargo_home(), rust_sysroot())
    existing = env.get("CARGO_ENCODED_RUSTFLAGS")
    if existing:
        flags = existing.split(ENCODED_SEPARATOR) + flags
    env["CARGO_ENCODED_RUSTFLAGS"] = ENCODED_SEPARATOR.join(flags)
    env.pop("RUSTFLAGS", None)
    return env


def fetch_locked(target_dir: Path) -> None:
    """Download every crate in the lockfile so licence lookup is deterministic.

    ``cargo build`` fetches only what the ``wasm32-wasip1`` target needs, so on
    a cold cache the licence of a target-specific crate (Windows, Redox, the
    dev-dependencies) is unknown. Fetching the whole lockfile makes
    ``SOURCES.md`` the same on every machine.
    """
    subprocess.run(
        ["cargo", "fetch", "--locked"],
        cwd=CRATE_DIR,
        env=build_environment(target_dir),
        check=True,
    )


def build(target_dir: Path) -> Path:
    """Build from scratch into ``target_dir`` and return the module path."""
    if target_dir.exists():
        shutil.rmtree(target_dir)
    subprocess.run(
        ["cargo", "build", "--release", "--locked", "--target", TARGET],
        cwd=CRATE_DIR,
        env=build_environment(target_dir),
        check=True,
    )
    built = target_dir / BUILT
    if not built.is_file():
        raise SystemExit(f"cargo finished but {built} does not exist")
    return built


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def vendored_patches(manifest: Path = MANIFEST) -> dict[str, str]:
    """The crates replaced by a local patch, and where each lives."""
    with manifest.open("rb") as handle:
        parsed = tomllib.load(handle)
    patches = parsed.get("patch", {}).get("crates-io", {})
    out: dict[str, str] = {}
    for name, value in patches.items():
        if isinstance(value, dict) and value.get("path"):
            out[name] = str(value["path"])
    return out


def registry_roots(cargo_home: Path) -> list[Path]:
    """The cargo registry source folders that may hold a crate's Cargo.toml."""
    root = cargo_home / "registry" / "src"
    if not root.is_dir():
        return []
    return [entry for entry in root.iterdir() if entry.is_dir()]


def licence_of(
    name: str,
    version: str,
    vendored: dict[str, str],
    registry: list[Path],
    crate_dir: Path = CRATE_DIR,
) -> str:
    """A crate's licence from its own Cargo.toml, read offline."""
    manifests: list[Path] = []
    if name in vendored:
        manifests.append(crate_dir / vendored[name] / "Cargo.toml")
    for root in registry:
        manifests.append(root / f"{name}-{version}" / "Cargo.toml")
    for manifest in manifests:
        if not manifest.is_file():
            continue
        try:
            with manifest.open("rb") as handle:
                package = tomllib.load(handle).get("package", {})
        except (OSError, tomllib.TOMLDecodeError):
            continue
        licence = package.get("license")
        if isinstance(licence, str) and licence:
            return licence
        licence_file = package.get("license-file")
        if isinstance(licence_file, str) and licence_file:
            return f"see {licence_file}"
        return "not declared in Cargo.toml"
    return "not in the local cargo registry"


def sources_document(
    lockfile: Path = LOCKFILE,
    manifest: Path = MANIFEST,
    cargo_home: Path | None = None,
    crate_dir: Path = CRATE_DIR,
) -> str:
    """The GPL source list for the module, generated from its Cargo.lock."""
    cargo_home = cargo_home or Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    vendored = vendored_patches(manifest)
    registry = registry_roots(cargo_home)
    with lockfile.open("rb") as handle:
        packages = tomllib.load(handle).get("package", [])

    rows: list[str] = []
    vendored_rows: list[str] = []
    for package in sorted(packages, key=lambda entry: str(entry.get("name", ""))):
        name = str(package.get("name", ""))
        version = str(package.get("version", ""))
        if name in ("", "loot-sort"):
            continue
        checksum = str(package.get("checksum", "vendored (not on crates.io)"))
        licence = licence_of(name, version, vendored, registry, crate_dir)
        url = f"https://crates.io/crates/{name}/{version}"
        if name in vendored:
            path = vendored[name]
            rows.append(f"| `{name}` | {version} | vendored | {licence} | {url} |")
            vendored_rows.append(
                f"- `{name}` {version}: patched sources in `{path}`; upstream at {url}"
            )
        else:
            rows.append(f"| `{name}` | {version} | `{checksum}` | {licence} | {url} |")

    lines = [
        "# Third-party sources in loot-sort.wasm",
        "",
        "The module statically links the crates below. The exact sources are available",
        "from crates.io at these versions; each row links to its crate page. The crates",
        "under \"Vendored with patches\" are shipped with local changes in this plugin.",
        "",
        "The committed module is the Linux build. `--check` compares it only on Linux;",
        "Windows and macOS toolchains embed different standard-library source paths, so",
        "they cannot reproduce these bytes and the script says so instead of failing.",
        "",
        "Generated by `python scripts/build_loot_wasm.py --sources`; do not edit by hand.",
        "",
        "| Crate | Version | Checksum | Licence | Source |",
        "|---|---|---|---|---|",
        *rows,
        "",
    ]
    if vendored_rows:
        lines.extend(["## Vendored with patches", "", *vendored_rows, ""])
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="rebuild and compare with the committed loot-sort.wasm",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        help="pass CARGO_NET_OFFLINE=true to cargo (the crates must be cached)",
    )
    parser.add_argument(
        "--target-root",
        type=Path,
        default=DEFAULT_TARGET_ROOT,
        help="where the two build target folders go",
    )
    parser.add_argument(
        "--sources",
        action="store_true",
        help=f"write {SOURCES.relative_to(REPO_ROOT)} from the module's Cargo.lock",
    )
    args = parser.parse_args(argv)

    if args.offline:
        os.environ["CARGO_NET_OFFLINE"] = "true"

    if args.sources:
        fetch_locked(args.target_root / "loot-wasm-sources")
        text = sources_document()
        SOURCES.write_text(text, encoding="utf-8", newline="\n")
        print(f"Wrote {SOURCES}")
        return 0

    if not is_reference_platform():
        if args.check:
            print(
                "cannot compare the committed module on this OS: it is the Linux "
                "reference build, and other toolchains embed different source paths.\n"
                "Run `python3 scripts/build_loot_wasm.py --check` on Linux or in WSL.",
                file=sys.stderr,
            )
            return 0
        print(
            "refusing to overwrite the committed module: it is the Linux reference "
            "build, and a build on this OS would not match `--check`.\n"
            "Build it on Linux or in WSL.",
            file=sys.stderr,
        )
        return 1

    first = build(args.target_root / "loot-wasm-a")
    first_bytes = first.read_bytes()
    digest = hashlib.sha256(first_bytes).hexdigest()

    if args.check:
        if not OUTPUT.is_file():
            print(f"{OUTPUT} is missing; run without --check first", file=sys.stderr)
            return 1
        committed = OUTPUT.read_bytes()
        if committed != first_bytes:
            print(
                "the committed module does not match a fresh build:\n"
                f"  committed {OUTPUT}: {hashlib.sha256(committed).hexdigest()}\n"
                f"  rebuilt:               {digest}",
                file=sys.stderr,
            )
            return 1
        fetch_locked(args.target_root / "loot-wasm-a")
        expected = sources_document()
        if not SOURCES.is_file() or SOURCES.read_text(encoding="utf-8") != expected:
            print(
                f"{SOURCES.relative_to(REPO_ROOT)} is missing or out of date; "
                "run `python scripts/build_loot_wasm.py --sources`",
                file=sys.stderr,
            )
            return 1
        print(f"{OUTPUT} matches the build ({digest})")
        print(f"{SOURCES.relative_to(REPO_ROOT)} is current")
        return 0

    second = build(args.target_root / "loot-wasm-b")
    second_bytes = second.read_bytes()
    if first_bytes != second_bytes:
        print(
            "the two builds are not byte-identical; CI cannot check the committed file:\n"
            f"  first:  {digest}\n"
            f"  second: {hashlib.sha256(second_bytes).hexdigest()}",
            file=sys.stderr,
        )
        return 1

    OUTPUT.write_bytes(first_bytes)
    print(f"Wrote {OUTPUT}")
    print(f"sha256 {digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
