#!/usr/bin/env python3
"""The game-package surface check: keep ``docs/plugins/game-packages.md`` true.

Two things are checked, both hermetically and with no writes:

* Every key in every game package JSON under ``packages/official/*/games/*.json``
  and ``packages/tracers/*/games/*.json``, at any depth, must appear in the
  guide as an inline-code span such as `` `key` ``. A missing key names the
  file and the key.
* Every public field on the package-format types in
  ``crates/agora-game-api/src/game.rs`` and ``package.rs`` must appear the same
  way. That is what stops the guide going stale when a field is added or
  renamed.

How the Rust fields are read: the file is cut at its first ``#[cfg(test)]``
module, then each checked ``pub struct``/``pub enum`` body is parsed. A struct
field is a line beginning with ``pub name:``; a named enum variant field is the
``name:`` inside its braces. A field's ``#[serde(rename = "...")]`` attribute is
honoured, so the ``match`` key is checked rather than the Rust name
``matching``. Only the types listed in ``CHECKED_TYPES`` are read: they are the
package-format and hook-call objects an author writes or receives, not internal
helpers such as the Thunderstore manifest reader.

Some JSON objects are maps whose keys are data, not schema: a
``game_templates`` entry is named by its author, and an ``environment`` entry is
an environment-variable name. Those two maps are named in ``MAP_VALUE_KEYS`` and
their inner names are skipped explicitly, while their values are still walked,
because a template and an environment value hold ordinary package fields. A
``behaviours`` value is a function name, which is a string, so it is never
visited as a key; ``behaviours``' own keys are schema and are checked.

Usage:
    python scripts/check_package_docs.py
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
GUIDE = REPO_ROOT / "docs" / "plugins" / "game-packages.md"
PACKAGE_GLOBS = (
    "packages/official/*/games/*.json",
    "packages/tracers/*/games/*.json",
)
RUST_FILES = (
    REPO_ROOT / "crates" / "agora-game-api" / "src" / "game.rs",
    REPO_ROOT / "crates" / "agora-game-api" / "src" / "package.rs",
)

# The map-valued keys whose inner names are author data rather than schema.
MAP_VALUE_KEYS = frozenset({"game_templates", "environment"})

# The package-format and hook-call types an author writes or receives.
CHECKED_TYPES = frozenset(
    {
        "PackageDefinition",
        "Behaviours",
        "GameDefinition",
        "StoreIdentifier",
        "VersionSource",
        "DeploymentStrategy",
        "ContentRule",
        "ContentMatch",
        "ContentLayout",
        "LaunchRecipe",
        "LaunchValue",
        "GamePath",
        "UserDataLocation",
        "LoadOrderDefinition",
        "LoadOrderDestination",
        "LoadOrderItems",
        "LoadOrderFormat",
        "LoadOrderRules",
        "LootRule",
        "LoadOrderRule",
        "LoadOrderLine",
        "LoadOrder",
        "LoadOrderEntry",
        "LaunchAlternative",
        "RuntimeFileRule",
        "SessionRules",
        "UserFileMapping",
        "UserFileStrategy",
        "SaveLocationRule",
        "FrameworkDefinition",
        "FrameworkDetection",
        "FrameworkVersionSource",
        "RuntimeConstraint",
        "VersionConstraint",
        "InstalledFramework",
        "RuntimeIdentity",
        "ToolDefinition",
        "ToolOutput",
        "ToolInput",
        "ToolInputKind",
        "ToolScope",
        "BeforeLaunch",
        "BeforeLaunchMode",
        "FailureMarker",
        "RequiredMagic",
        "GameContext",
        "GameInstall",
        "InstallCapabilities",
        "InstallKind",
        "VolumeInfo",
        "Suggestion",
        "DownloadRecognition",
        "DownloadAdd",
        "Diagnostic",
        "DiagnosticSeverity",
    }
)

INLINE_CODE_RE = re.compile(r"`([^`\n]+)`")
STRUCT_RE = re.compile(r"\bpub\s+(struct|enum)\s+(\w+)\b")
STRUCT_FIELD_RE = re.compile(
    r"(?m)^(?P<attrs>(?:[ \t]*#\[[^\n]*\]\s*\n)*)"
    r"[ \t]*pub\s+(?P<field>[a-z_][a-z0-9_]*)\s*:"
)
SERDE_RENAME_RE = re.compile(r'serde\([^)]*rename\s*=\s*"([^"]+)"')
ENUM_FIELD_RE = re.compile(
    r"(?m)(?:^|[{,])\s*([a-z_][a-z0-9_]*)\s*:(?!:)"
)


def documented_names(text: str) -> set[str]:
    """The inline-code spans in the guide, which is how a key is documented."""
    return set(INLINE_CODE_RE.findall(text))


def package_files(repo_root: Path = REPO_ROOT) -> list[Path]:
    files: list[Path] = []
    for pattern in PACKAGE_GLOBS:
        files.extend(repo_root.glob(pattern))
    return sorted(files)


def iter_json_keys(value, path: tuple[str, ...] = ()):
    """Yield ``(key, parent_path)`` for every schema key in ``value``.

    When the current object is a map of author-chosen names (the value of
    ``game_templates``, ``environment`` or ``behaviours``), its keys are data
    and are not yielded. The values are still walked, because a template, an
    environment value and a behaviour's name can hold or name ordinary fields.
    """
    if isinstance(value, dict):
        skip_keys = bool(path) and path[-1] in MAP_VALUE_KEYS
        for key, child in value.items():
            if not skip_keys:
                yield key, path
            yield from iter_json_keys(child, path + (key,))
    elif isinstance(value, list):
        for child in value:
            yield from iter_json_keys(child, path)


def _matching_brace(text: str, opening: int) -> int:
    depth = 0
    for index in range(opening, len(text)):
        if text[index] == "{":
            depth += 1
        elif text[index] == "}":
            depth -= 1
            if depth == 0:
                return index
    raise ValueError("unbalanced braces while reading a Rust type")


def rust_fields(path: Path) -> set[str]:
    """The public field names of the checked types in one Rust source file."""
    text = path.read_text(encoding="utf-8")
    cut = text.find("#[cfg(test)]")
    if cut != -1:
        text = text[:cut]

    fields: set[str] = set()
    for header in STRUCT_RE.finditer(text):
        kind, type_name = header.group(1), header.group(2)
        if type_name not in CHECKED_TYPES:
            continue
        opening = text.find("{", header.end())
        if opening == -1:
            continue
        close = _matching_brace(text, opening)
        body = text[opening + 1 : close]
        if kind == "struct":
            for field in STRUCT_FIELD_RE.finditer(body):
                name = field.group("field")
                renamed = SERDE_RENAME_RE.search(field.group("attrs"))
                fields.add(renamed.group(1) if renamed else name)
        else:
            fields.update(ENUM_FIELD_RE.findall(body))
    return fields


def check(repo_root: Path = REPO_ROOT, guide: Path = GUIDE) -> list[str]:
    """Every problem, each naming the file and the undocumented key or field."""
    documented = documented_names(guide.read_text(encoding="utf-8"))
    guide_name = guide.relative_to(repo_root).as_posix()
    problems: list[str] = []
    seen: set[tuple[str, str]] = set()

    for path in package_files(repo_root):
        relative = path.relative_to(repo_root).as_posix()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            problems.append(f"{relative}: cannot parse JSON: {error}")
            continue
        for key, _ in iter_json_keys(data):
            if key in documented or (relative, key) in seen:
                continue
            seen.add((relative, key))
            problems.append(
                f"{relative}: key `{key}` is not documented in {guide_name}"
            )

    for path in RUST_FILES:
        relative = path.relative_to(repo_root).as_posix()
        for field in sorted(rust_fields(path)):
            if field in documented or (relative, field) in seen:
                continue
            seen.add((relative, field))
            problems.append(
                f"{relative}: Rust field `{field}` is not documented in {guide_name}"
            )

    return problems


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Hermetic game-package surface check: every package JSON key and "
            "every package-format Rust field must be documented in "
            "docs/plugins/game-packages.md. Makes no network requests and "
            "modifies no files."
        )
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="print the files and types that were checked",
    )
    args = parser.parse_args()

    if args.verbose:
        for path in package_files():
            print(f"  package: {path.relative_to(REPO_ROOT).as_posix()}")
        for path in RUST_FILES:
            print(
                "  rust: "
                f"{path.relative_to(REPO_ROOT).as_posix()} "
                f"({len(rust_fields(path))} field(s))"
            )
        print()

    problems = check()
    if problems:
        for problem in problems:
            print(problem)
        print(
            f"\nFAIL: {len(problems)} undocumented key(s) or field(s). "
            "Add each to docs/plugins/game-packages.md as `key`.",
            file=sys.stderr,
        )
        return 1

    print(
        "OK: every game-package JSON key and package-format Rust field is "
        "documented. No network requests were made; no files were modified."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
