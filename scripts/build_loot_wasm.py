#!/usr/bin/env python3
"""Build the LOOT WebAssembly module reproducibly.

The module lives in ``packages/official/creation-engine/wasm/loot-sort`` and is
built for ``wasm32-wasip1`` with the release profile in its ``Cargo.toml``
(LTO, ``opt-level=3``, ``panic=abort``, strip). Paths are remapped so the bytes
do not depend on where the repository or the cargo registry lives.

Usage::

    python scripts/build_loot_wasm.py
    python scripts/build_loot_wasm.py --offline
    python scripts/build_loot_wasm.py --check

The default mode builds twice, from two different target folders, compares the
bytes, writes the committed ``loot-sort.wasm`` and prints its SHA-256.
``--check`` rebuilds once and refuses when the committed file differs, which is
what CI runs.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CRATE_DIR = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-sort"
OUTPUT = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-sort.wasm"
TARGET = "wasm32-wasip1"
BUILT = Path("wasm32-wasip1") / "release" / "loot-sort.wasm"
DEFAULT_TARGET_ROOT = REPO_ROOT / "target"
ENCODED_SEPARATOR = "\x1f"


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
    args = parser.parse_args(argv)

    if args.offline:
        os.environ["CARGO_NET_OFFLINE"] = "true"

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
        print(f"{OUTPUT} matches the build ({digest})")
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
