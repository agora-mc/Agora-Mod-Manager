#!/usr/bin/env python3
"""Keep the CLI able to do what the desktop app can.

`scripts/cli_coverage.json` lists every desktop (Tauri) command with one of:

  {"cli": "<command path>"}  the CLI command a user runs for the same thing
  {"gui": "<reason>"}        only meaningful in the desktop (a file dialog, a window)
  {"gap": "<what>"}          a known missing CLI command

This check fails when a desktop command is not listed (a new command must say
how the CLI covers it), when the file lists a command that no longer exists,
when an entry is malformed, or when the number of gaps differs from
`gap_budget`. The budget may only go down: closing a gap means lowering it.

That each `cli` path is a real CLI command is checked by the CLI's smoke test
(`crates/agora/tests/cli_smoke.rs`), which runs `agora <path> --help` for every
one; this script stays hermetic and needs no build.

Usage:
    python scripts/check_cli_coverage.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_tauri_bindings  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parent.parent
COVERAGE = REPO_ROOT / "scripts" / "cli_coverage.json"
KINDS = ("cli", "gui", "gap")


def check(coverage: dict, desktop_commands: set[str]) -> list[str]:
    errors: list[str] = []
    listed = coverage.get("commands", {})
    for name in sorted(desktop_commands - set(listed)):
        errors.append(
            f"desktop command '{name}' is not in scripts/cli_coverage.json: add the CLI command "
            "that does the same thing ({\"cli\": \"...\"}), or say why it is desktop-only "
            "({\"gui\": \"...\"}) or a known gap ({\"gap\": \"...\"})"
        )
    for name in sorted(set(listed) - desktop_commands):
        errors.append(f"scripts/cli_coverage.json lists '{name}', which is no longer a desktop command")
    for name, entry in sorted(listed.items()):
        kinds = [k for k in KINDS if k in entry] if isinstance(entry, dict) else []
        if (
            len(kinds) != 1
            or len(entry) != 1
            or not isinstance(entry[kinds[0]], str)
            or not entry[kinds[0]].strip()
        ):
            errors.append(f"'{name}': an entry needs exactly one of {', '.join(KINDS)}, with text")
    gaps = sum(1 for entry in listed.values() if isinstance(entry, dict) and "gap" in entry)
    budget = coverage.get("gap_budget")
    if not isinstance(budget, int):
        errors.append("gap_budget must be an integer")
    elif gaps > budget:
        errors.append(
            f"{gaps} CLI gaps, over the budget of {budget}: a new desktop command needs a CLI "
            "command (or a reason it is desktop-only); the budget only goes down"
        )
    elif gaps < budget:
        errors.append(f"{gaps} CLI gaps, below the budget of {budget}: lower gap_budget to {gaps}")
    return errors


def main() -> int:
    coverage = json.loads(COVERAGE.read_text(encoding="utf-8"))
    lib = check_tauri_bindings.LIB_RS.read_text(encoding="utf-8")
    desktop = check_tauri_bindings.parse_rust_commands(lib)
    errors = check(coverage, desktop)
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if errors:
        print(f"FAIL: {len(errors)} CLI coverage problem(s).", file=sys.stderr)
        return 1
    commands = coverage["commands"].values()
    print(
        f"OK: {len(desktop)} desktop commands: {sum('cli' in e for e in commands)} have a CLI "
        f"command, {sum('gui' in e for e in commands)} are desktop-only, "
        f"{coverage['gap_budget']} known gaps."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
