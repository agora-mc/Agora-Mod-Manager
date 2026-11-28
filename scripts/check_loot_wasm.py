#!/usr/bin/env python3
"""Prove the LOOT WASI module against a native build of the same crate.

The script builds the native sorter and the tiny wasmtime host, then runs both
on synthetic fixtures and (optionally) a real Data folder. The two stdout
streams must be byte-identical.

Usage::

    python scripts/check_loot_wasm.py --offline
    python scripts/check_loot_wasm.py --offline \
        --data D:/tmp/lootspike/data \
        --masterlist D:/tmp/lootspike/masterlist.yaml \
        --prelude D:/tmp/lootspike/prelude.yaml \
        --load-order-file D:/tmp/lootspike/profile/loadorder.txt \
        --active-file D:/tmp/lootspike/profile/plugins.txt

The real fixture is read only; the script writes only under
``target/loot-wasm-proof``.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CRATE_DIR = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-sort"
HOST_DIR = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-host"
WASM = REPO_ROOT / "packages" / "official" / "creation-engine" / "wasm" / "loot-sort.wasm"
PROOF_ROOT = REPO_ROOT / "target" / "loot-wasm-proof"
NATIVE_TARGET = REPO_ROOT / "target" / "loot-wasm-proof-native"
HOST_TARGET = REPO_ROOT / "target" / "loot-wasm-proof-host"


def run(command: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    subprocess.run(command, cwd=cwd, env=env, check=True)


def build_native(offline: bool) -> Path:
    env = os.environ.copy()
    if offline:
        env["CARGO_NET_OFFLINE"] = "true"
    run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "--manifest-path",
            str(CRATE_DIR / "Cargo.toml"),
        ],
        REPO_ROOT,
        env | {"CARGO_TARGET_DIR": str(NATIVE_TARGET)},
    )
    return NATIVE_TARGET / "release" / ("loot-sort.exe" if os.name == "nt" else "loot-sort")


def build_host(offline: bool) -> Path:
    env = os.environ.copy()
    if offline:
        env["CARGO_NET_OFFLINE"] = "true"
    run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "--manifest-path",
            str(HOST_DIR / "Cargo.toml"),
        ],
        REPO_ROOT,
        env | {"CARGO_TARGET_DIR": str(HOST_TARGET)},
    )
    return HOST_TARGET / "release" / ("loot-host.exe" if os.name == "nt" else "loot-host")


def subrecord(kind: bytes, payload: bytes) -> bytes:
    return kind + struct.pack("<H", len(payload)) + payload


def plugin_bytes(masters: list[str], flags: int = 0) -> bytes:
    body = subrecord(b"HEDR", b"\x00" * 12)
    for master in masters:
        body += subrecord(b"MAST", master.encode("ascii") + b"\x00")
        body += subrecord(b"DATA", b"\x00" * 8)
    return (
        b"TES4"
        + struct.pack("<I", len(body))
        + struct.pack("<I", flags)
        + b"\x00" * 12
        + body
    )


def write_fixture(
    root: Path,
    plugins: dict[str, list[str]],
    masterlist: str,
    order: list[str],
    active: list[str],
    prelude: str | None = None,
) -> tuple[Path, Path]:
    if root.exists():
        shutil.rmtree(root)
    data = root / "data"
    data.mkdir(parents=True)
    for name, masters in plugins.items():
        (data / name).write_bytes(plugin_bytes(masters))
    (root / "masterlist.yaml").write_text(masterlist, encoding="utf-8", newline="\n")
    if prelude is not None:
        (root / "prelude.yaml").write_text(prelude, encoding="utf-8", newline="\n")
    (root / "order.json").write_text(json.dumps(order), encoding="utf-8")
    (root / "active.json").write_text(json.dumps(active), encoding="utf-8")
    return data, root


def native_request(
    data: Path,
    order: list[str],
    active: list[str],
    masterlist: Path,
    prelude: Path | None,
) -> str:
    return json.dumps(
        {
            "game_type": "SkyrimSE",
            "data": data.as_posix(),
            "load_order": order,
            "active": active,
            "masterlist": masterlist.as_posix(),
            "prelude": prelude.as_posix() if prelude else None,
        }
    )


def wasm_request(
    order: list[str],
    active: list[str],
    masterlist_name: str,
    prelude: bool,
) -> str:
    return json.dumps(
        {
            "game_type": "SkyrimSE",
            "data": "/data",
            "load_order": order,
            "active": active,
            "masterlist": f"/input/{masterlist_name}",
            "prelude": "/input/prelude.yaml" if prelude else None,
        }
    )


def run_native(binary: Path, request: str) -> tuple[bytes, float]:
    start = time.perf_counter()
    result = subprocess.run(
        [str(binary)],
        input=request.encode("utf-8"),
        capture_output=True,
        check=False,
    )
    elapsed = time.perf_counter() - start
    if result.returncode != 0:
        raise SystemExit(
            f"native exited with {result.returncode}\n"
            f"stdout: {result.stdout.decode('utf-8', 'replace')}\n"
            f"stderr: {result.stderr.decode('utf-8', 'replace')}"
        )
    return result.stdout, elapsed


def run_wasm(
    host: Path,
    request: str,
    data: Path,
    input_dir: Path,
    out_path: Path,
) -> tuple[bytes, float]:
    request_path = PROOF_ROOT / "request-wasm.json"
    request_path.write_text(request, encoding="utf-8")
    start = time.perf_counter()
    result = subprocess.run(
        [
            str(host),
            str(WASM),
            str(request_path),
            str(data),
            str(input_dir),
            str(out_path),
        ],
        capture_output=True,
        check=False,
    )
    elapsed = time.perf_counter() - start
    if result.returncode != 0:
        raise SystemExit(
            f"wasm host exited with {result.returncode}\n"
            f"stdout: {result.stdout.decode('utf-8', 'replace')}\n"
            f"stderr: {result.stderr.decode('utf-8', 'replace')}"
        )
    return out_path.read_bytes(), elapsed


def compare(
    label: str,
    native_binary: Path,
    host: Path,
    data: Path,
    order: list[str],
    active: list[str],
    masterlist: Path,
    prelude: Path | None,
) -> None:
    native_out, native_ms = run_native(
        native_binary, native_request(data, order, active, masterlist, prelude)
    )
    wasm_out, wasm_ms = run_wasm(
        host,
        wasm_request(order, active, masterlist.name, prelude is not None),
        data,
        masterlist.parent,
        PROOF_ROOT / f"wasm-{label}.json",
    )
    if native_out != wasm_out:
        print(f"{label}: DIFFER", file=sys.stderr)
        print(f"  native: {native_out.decode('utf-8', 'replace')}", file=sys.stderr)
        print(f"  wasm:   {wasm_out.decode('utf-8', 'replace')}", file=sys.stderr)
        raise SystemExit(1)
    print(f"{label}: identical  native {native_ms * 1000:.0f} ms  wasm {wasm_ms * 1000:.0f} ms")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="do not let cargo reach the network")
    parser.add_argument("--data", type=Path, help="a real Data folder to check (read only)")
    parser.add_argument("--masterlist", type=Path, help="the real fixture's masterlist.yaml")
    parser.add_argument("--prelude", type=Path, help="the real fixture's prelude.yaml")
    parser.add_argument("--load-order-file", type=Path, help="loadorder.txt for the real fixture")
    parser.add_argument("--active-file", type=Path, help="plugins.txt for the real fixture")
    args = parser.parse_args(argv)

    if not WASM.is_file():
        raise SystemExit(f"{WASM} is missing; run scripts/build_loot_wasm.py first")
    PROOF_ROOT.mkdir(parents=True, exist_ok=True)
    native_binary = build_native(args.offline)
    host = build_host(args.offline)

    basic = PROOF_ROOT / "basic"
    data, root = write_fixture(
        basic,
        {"A.esp": [], "B.esp": [], "C.esp": []},
        "plugins:\n  - name: 'B.esp'\n    after:\n      - 'A.esp'\n",
        ["B.esp", "A.esp", "C.esp"],
        ["A.esp", "B.esp", "C.esp"],
    )
    compare(
        "basic",
        native_binary,
        host,
        data,
        ["B.esp", "A.esp", "C.esp"],
        ["A.esp", "B.esp", "C.esp"],
        root / "masterlist.yaml",
        None,
    )

    missing = PROOF_ROOT / "missing-master"
    data, root = write_fixture(
        missing,
        {"A.esp": [], "Child.esp": ["Missing.esm"]},
        "plugins: []\n",
        ["Child.esp", "A.esp"],
        ["Child.esp", "A.esp"],
    )
    compare(
        "missing-master",
        native_binary,
        host,
        data,
        ["Child.esp", "A.esp"],
        ["Child.esp", "A.esp"],
        root / "masterlist.yaml",
        None,
    )

    checksum = PROOF_ROOT / "checksum"
    data, root = write_fixture(
        checksum,
        {"A.esp": [], "B.esp": []},
        "plugins: []\n",
        ["B.esp", "A.esp"],
        ["B.esp", "A.esp"],
    )
    crc = zlib.crc32((data / "A.esp").read_bytes()) & 0xFFFFFFFF
    for tag, value in (("match", crc), ("no-match", crc ^ 1)):
        (root / f"masterlist-{tag}.yaml").write_text(
            "plugins:\n"
            "  - name: 'B.esp'\n"
            "    after:\n"
            "      - name: 'A.esp'\n"
            f"        condition: 'checksum(\"A.esp\", {value:08X})'\n",
            encoding="utf-8",
            newline="\n",
        )
        compare(
            f"checksum-{tag}",
            native_binary,
            host,
            data,
            ["B.esp", "A.esp"],
            ["B.esp", "A.esp"],
            root / f"masterlist-{tag}.yaml",
            None,
        )

    active = PROOF_ROOT / "active"
    data, root = write_fixture(
        active,
        {"A.esp": [], "B.esp": []},
        "plugins:\n"
        "  - name: 'B.esp'\n"
        "    after:\n"
        "      - name: 'A.esp'\n"
        "        condition: 'active(\"A.esp\")'\n",
        ["B.esp", "A.esp"],
        ["A.esp", "B.esp"],
    )
    compare(
        "active-on",
        native_binary,
        host,
        data,
        ["B.esp", "A.esp"],
        ["A.esp", "B.esp"],
        root / "masterlist.yaml",
        None,
    )
    compare(
        "active-off",
        native_binary,
        host,
        data,
        ["B.esp", "A.esp"],
        ["B.esp"],
        root / "masterlist.yaml",
        None,
    )

    if args.data is not None:
        if args.masterlist is None:
            raise SystemExit("--data needs --masterlist")
        prelude = args.prelude
        order = sorted(
            (
                path.name
                for path in args.data.iterdir()
                if path.is_file() and path.suffix.lower() in {".esp", ".esm", ".esl"}
            ),
            key=str.lower,
        )
        if args.load_order_file is not None:
            present = {name.lower(): name for name in order}
            listed: list[str] = []
            for line in args.load_order_file.read_text(encoding="utf-8-sig").splitlines():
                line = line.strip()
                if line and not line.startswith("#") and line.lower() in present:
                    name = present[line.lower()]
                    if name not in listed:
                        listed.append(name)
            order = listed + [name for name in order if name not in listed]
        active = order
        if args.active_file is not None:
            present = {name.lower(): name for name in order}
            active = []
            for line in args.active_file.read_text(encoding="utf-8-sig").splitlines():
                line = line.strip()
                if line.startswith("*") and line[1:].lower() in present:
                    active.append(present[line[1:].lower()])
        input_dir = PROOF_ROOT / "real-input"
        if input_dir.exists():
            shutil.rmtree(input_dir)
        input_dir.mkdir(parents=True)
        shutil.copy2(args.masterlist, input_dir / "masterlist.yaml")
        if prelude is not None:
            shutil.copy2(prelude, input_dir / "prelude.yaml")
        compare(
            "real-fixture",
            native_binary,
            host,
            args.data,
            order,
            active,
            input_dir / "masterlist.yaml",
            (input_dir / "prelude.yaml") if prelude is not None else None,
        )

    print("all comparisons are byte-identical")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
