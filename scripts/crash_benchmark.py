#!/usr/bin/env python3
"""Freeze, score and compare crash-doctor benchmark answer keys.

This tool is deliberately game-agnostic: it compares predicted culprit names
against a frozen key and never parses a crash log. The corpus and its answer
key stay outside the repository; only this code and synthetic test fixtures
live here.

Subcommands:

    freeze   build dev-key.jsonl / test-key.jsonl from labels + manifest
    score    score one predictions file against a key
    compare  score several tools (plus an abstain-always baseline) in one table
    predict  turn a tool's output into a predictions file (a benchmark adapter)

Every hit rate is reported twice: **strict**, where a predicted name must
normalise to the key's culprit or one of its `accept` entries, and **loose**,
which splits those answers into words and accepts a prediction that contains a
word. Loose matching is a measuring aid, not a verdict: a short answer inside a
longer unrelated name can still be over-credited.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any, Iterable

EXTENSIONS = (".dll", ".esp", ".esm", ".esl", ".jar", ".pex")
GROUP_DIMENSIONS = ("game", "log_type", "difficulty", "culprit_in_log", "culprit_kind")
SEGMENT_SPLIT = re.compile(r" and | with |[&,()/;]")

# The closed set of cause kinds the crash contract teaches. A key row's `culprit_kind` is mapped
# onto this set so the benchmark can grade what kind of cause a tool found, not only its name.
CAUSE_KINDS = (
    "mod",
    "version_mismatch",
    "missing_requirement",
    "file",
    "setting",
    "save",
    "memory",
    "driver",
    "game_install",
    "other",
)

CAUSE_SYNONYMS = {
    "mod": "mod",
    "mods": "mod",
    "mod_conflict": "mod",
    "mod_loading": "mod",
    "version": "version_mismatch",
    "version_mismatch": "version_mismatch",
    "wrong_version": "version_mismatch",
    "outdated": "version_mismatch",
    "incompatible": "version_mismatch",
    "game_version": "version_mismatch",
    "loader_version": "version_mismatch",
    "missing": "missing_requirement",
    "missing_dependency": "missing_requirement",
    "missing_requirement": "missing_requirement",
    "missing_master": "missing_requirement",
    "missing_mod": "missing_requirement",
    "dependency": "missing_requirement",
    "file": "file",
    "asset": "file",
    "mesh": "file",
    "texture": "file",
    "corrupt_file": "file",
    "setting": "setting",
    "settings": "setting",
    "config": "setting",
    "configuration": "setting",
    "load_order": "setting",
    "save": "save",
    "world": "save",
    "corrupt_save": "save",
    "memory": "memory",
    "oom": "memory",
    "out_of_memory": "memory",
    "ram": "memory",
    "driver": "driver",
    "graphics_driver": "driver",
    "gpu": "driver",
    "game_install": "game_install",
    "game": "game_install",
    "game_files": "game_install",
    "install": "game_install",
    "vanilla": "game_install",
    "other": "other",
    "none": "other",
    "unknown": "other",
}


def cause_kind(value: Any) -> str | None:
    """Map a key's `culprit_kind` (or a prediction's `cause`) onto the closed cause set.

    A missing or blank kind has no opinion and returns `None`. Anything not in the closed set or
    the synonym table is `other`: the set has that open door on purpose.
    """
    if not isinstance(value, str) or not value.strip():
        return None
    text = value.strip().casefold().replace("-", "_").replace(" ", "_")
    if text in CAUSE_KINDS:
        return text
    return CAUSE_SYNONYMS.get(text, "other")



class BenchmarkError(Exception):
    """A user-facing benchmark input or usage error."""


def normalize_name(name: str) -> str:
    """Casefold, drop one known trailing extension, remove non-alphanumerics."""
    text = str(name).strip().casefold()
    for extension in EXTENSIONS:
        if text.endswith(extension):
            text = text[: -len(extension)]
            break
    return "".join(character for character in text if character.isalnum())


def _validate_accept(value: Any, where: str) -> list[str] | None:
    """Return a label row's optional `accept` list, or raise naming the row."""
    if value is None:
        return None
    if not isinstance(value, list) or any(
        not isinstance(entry, str) or not entry for entry in value
    ):
        raise BenchmarkError(
            f"{where}: 'accept' must be a list of non-empty strings"
        )
    return list(value)


def _strict_names(row: dict[str, Any]) -> list[str]:
    """The culprit and every accepted spelling, normalised for exact comparison."""
    names = [normalize_name(row["culprit"])]
    for entry in row.get("accept") or []:
        names.append(normalize_name(entry))
    return names


def _loose_segments(row: dict[str, Any]) -> list[str]:
    """Words from the culprit and its `accept` entries, for loose matching.

    Answers are people's words ("SMIM (Static Mesh Improvement Mod)") while a
    tool predicts file names, so both sides are normalised and the answer is
    split at ` and `, ` with `, `&`, `,`, `/`, `(`, `)`, and `;`. Segments under
    four characters are dropped because they would match almost anything.

    Loose matching still over-credits: a short answer word that appears inside
    a longer, unrelated prediction counts. It is a measuring aid, not a verdict.
    """
    segments: set[str] = set()
    for text in (row["culprit"], *(row.get("accept") or [])):
        for piece in SEGMENT_SPLIT.split(str(text)):
            segment = normalize_name(piece)
            if len(segment) >= 4:
                segments.add(segment)
    return sorted(segments)


def _loose_hit(name: str, segments: Iterable[str]) -> bool:
    """A prediction hits when it is, or contains, an answer segment."""
    return any(segment in name for segment in segments)


def _read_jsonl(path: Path, what: str) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    try:
        handle = path.open("r", encoding="utf-8")
    except OSError as error:
        raise BenchmarkError(f"cannot read {what} file {path}: {error}") from error
    with handle:
        for line_number, line in enumerate(handle, 1):
            stripped = line.strip()
            if not stripped:
                continue
            try:
                row = json.loads(stripped)
            except json.JSONDecodeError as error:
                raise BenchmarkError(
                    f"{what} {path}:{line_number}: invalid JSON: {error}"
                ) from error
            if not isinstance(row, dict):
                raise BenchmarkError(
                    f"{what} {path}:{line_number}: expected a JSON object"
                )
            rows.append(row)
    return rows


def _json_line(row: dict[str, Any]) -> str:
    return json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n"


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _sha256_file(path: Path) -> str:
    try:
        return _sha256_bytes(path.read_bytes())
    except OSError as error:
        raise BenchmarkError(f"cannot read {path}: {error}") from error


def _inside_git_work_tree(path: Path) -> bool:
    resolved = path.resolve()
    start = resolved.parent if resolved.is_file() else resolved
    for candidate in (start, *start.parents):
        if (candidate / ".git").exists():
            return True
    return False


def _string_field(row: dict[str, Any], field: str, where: str) -> str:
    value = row.get(field)
    if not isinstance(value, str) or not value:
        raise BenchmarkError(f"{where}: missing non-empty string field {field!r}")
    return value


# ---------------------------------------------------------------------------
# freeze
# ---------------------------------------------------------------------------


def _split_key(
    key: list[dict[str, Any]],
    salt: str,
    test_fraction: float,
    dev_ids: frozenset[str] = frozenset(),
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Split by manifest sha256, stratified per game, deterministically.

    Rows that share a manifest sha256 are one group, so the same log posted
    twice always lands on the same side. Within a game, groups are ordered by
    sha256(salt + the group's lexicographically smallest id); the first
    round(n * test_fraction) go to test.

    A group holding any id in `dev_ids` goes to dev and is left out of that
    count: a case somebody has already studied cannot be held out.
    """
    groups: dict[str, list[dict[str, Any]]] = {}
    for row in key:
        groups.setdefault(row["sha256"], []).append(row)

    per_game: dict[str, list[tuple[int, str]]] = {}
    for sha, rows in groups.items():
        if any(row["id"] in dev_ids for row in rows):
            continue
        canonical = min(rows, key=lambda item: item["id"])
        digest = hashlib.sha256((salt + canonical["id"]).encode("utf-8")).hexdigest()
        per_game.setdefault(canonical["game"], []).append((int(digest, 16), sha))

    test_shas: set[str] = set()
    for entries in per_game.values():
        entries.sort()
        count = int(len(entries) * test_fraction + 0.5)
        count = max(0, min(count, len(entries)))
        for _, sha in entries[:count]:
            test_shas.add(sha)

    dev: list[dict[str, Any]] = []
    test: list[dict[str, Any]] = []
    for row in key:
        (test if row["sha256"] in test_shas else dev).append(row)
    return dev, test


def _read_dev_ids(path: str | Path | None) -> frozenset[str]:
    """Ids that must land in dev: one per line, or JSON lines with an `id`."""
    if path is None:
        return frozenset()
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as error:
        raise BenchmarkError(f"cannot read dev ids file {path}: {error}") from error
    ids: set[str] = set()
    for line_number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("{"):
            try:
                value = json.loads(stripped).get("id")
            except (json.JSONDecodeError, AttributeError) as error:
                raise BenchmarkError(
                    f"dev ids {path}:{line_number}: invalid JSON: {error}"
                ) from error
            if not isinstance(value, str) or not value:
                raise BenchmarkError(f"dev ids {path}:{line_number}: no `id`")
            ids.add(value)
        else:
            ids.add(stripped)
    if not ids:
        raise BenchmarkError(f"dev ids file {path} names no ids")
    return frozenset(ids)


def freeze(
    labels_path: str | Path,
    manifest_path: str | Path,
    out_dir: str | Path,
    salt: str = "",
    test_fraction: float = 0.5,
    force: bool = False,
    dev_ids_path: str | Path | None = None,
) -> dict[str, Any]:
    """Build dev-key.jsonl and test-key.jsonl, returning the freeze summary."""
    if not 0.0 <= test_fraction <= 1.0:
        raise BenchmarkError("--test-fraction must be between 0 and 1")
    if not isinstance(salt, str):
        raise BenchmarkError("--salt must be a string")

    labels_path = Path(labels_path)
    manifest_path = Path(manifest_path)
    out = Path(out_dir)

    labels = _read_jsonl(labels_path, "labels")
    manifest = _read_jsonl(manifest_path, "manifest")
    if not labels:
        raise BenchmarkError(f"labels file {labels_path} has no rows")
    if not manifest:
        raise BenchmarkError(f"manifest file {manifest_path} has no rows")

    by_id: dict[str, dict[str, Any]] = {}
    for row in manifest:
        manifest_id = _string_field(row, "id", f"manifest {manifest_path}")
        by_id[manifest_id] = row

    dropped = {
        "label_silver": 0,
        "label_none": 0,
        "label_other": 0,
        "culprit_null": 0,
        "template_risk": 0,
    }
    key: list[dict[str, Any]] = []
    seen: set[str] = set()

    for row in labels:
        where = f"labels {labels_path}"
        row_id = _string_field(row, "id", where)
        label = row.get("label")
        if label != "gold":
            if label == "silver":
                dropped["label_silver"] += 1
            elif label == "none":
                dropped["label_none"] += 1
            else:
                dropped["label_other"] += 1
            continue
        culprit = row.get("culprit")
        if culprit is None:
            dropped["culprit_null"] += 1
            continue
        if row.get("template_risk") is True:
            dropped["template_risk"] += 1
            continue
        accept = _validate_accept(
            row.get("accept"), f"labels {labels_path}: row {row_id!r}"
        )

        manifest_row = by_id.get(row_id)
        if manifest_row is None:
            raise BenchmarkError(
                f"key row id {row_id!r} is not in the manifest"
            )
        game = _string_field(manifest_row, "game", f"manifest id {row_id!r}")
        log_type = _string_field(manifest_row, "log_type", f"manifest id {row_id!r}")
        sha256 = _string_field(manifest_row, "sha256", f"manifest id {row_id!r}")
        if row_id in seen:
            raise BenchmarkError(f"duplicate key id {row_id!r}")
        seen.add(row_id)
        key_row: dict[str, Any] = {
            "id": row_id,
            "game": game,
            "log_type": log_type,
            "difficulty": row.get("difficulty"),
            "culprit_kind": row.get("culprit_kind"),
            "culprit_in_log": row.get("culprit_in_log"),
            "culprit": culprit,
            "sha256": sha256,
        }
        if accept is not None:
            key_row["accept"] = accept
        key.append(key_row)

    if not key:
        raise BenchmarkError(
            "no rows qualify for the key; refusing to write an empty benchmark"
        )

    if _inside_git_work_tree(out):
        raise BenchmarkError(
            f"refusing --out {out}: it is inside a git work tree, and benchmark "
            "keys must never enter a repository"
        )

    dev_path = out / "dev-key.jsonl"
    test_path = out / "test-key.jsonl"
    if not force and (dev_path.exists() or test_path.exists()):
        raise BenchmarkError(
            f"refusing to overwrite an existing key in {out}; use --force to replace it"
        )

    dev_ids = _read_dev_ids(dev_ids_path)
    dev_rows, test_rows = _split_key(key, salt, test_fraction, dev_ids)
    forced_dev = sum(1 for row in dev_rows if row["id"] in dev_ids)

    def public(row: dict[str, Any]) -> dict[str, Any]:
        fields = {field: row[field] for field in (
            "id", "game", "log_type", "difficulty", "culprit_kind",
            "culprit_in_log", "culprit",
        )}
        if "accept" in row:
            fields["accept"] = row["accept"]
        return fields

    dev_rows.sort(key=lambda item: (item["game"], item["id"]))
    test_rows.sort(key=lambda item: (item["game"], item["id"]))

    out.mkdir(parents=True, exist_ok=True)
    dev_bytes = "".join(_json_line(public(row)) for row in dev_rows).encode("utf-8")
    test_bytes = "".join(_json_line(public(row)) for row in test_rows).encode("utf-8")
    dev_path.write_bytes(dev_bytes)
    test_path.write_bytes(test_bytes)

    test_sha256 = _sha256_bytes(test_bytes)
    summary = {
        "labels": len(labels),
        "manifest": len(manifest),
        "key_rows": len(key),
        "dev_rows": len(dev_rows),
        "test_rows": len(test_rows),
        "test_sha256": test_sha256,
        "salt": salt,
        "test_fraction": test_fraction,
        "forced_dev": forced_dev,
        "dropped": dropped,
        "dropped_total": sum(dropped.values()),
    }
    (out / "freeze.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return summary


def format_freeze(summary: dict[str, Any], out_dir: str | Path) -> str:
    dropped = summary["dropped"]
    reasons = ", ".join(f"{name}={count}" for name, count in dropped.items())
    lines = [
        f"Key: {summary['key_rows']} rows "
        f"(dev {summary['dev_rows']}, test {summary['test_rows']})",
        f"Dropped: {summary['dropped_total']} ({reasons})",
        f"Forced into dev: {summary.get('forced_dev', 0)}",
        f"test-key.jsonl sha256: {summary['test_sha256']}",
        f"Wrote {Path(out_dir) / 'dev-key.jsonl'}, "
        f"{Path(out_dir) / 'test-key.jsonl'} and {Path(out_dir) / 'freeze.json'}",
    ]
    if summary["test_rows"] == 0:
        lines.append("Warning: the test key is empty; no benchmark rows were held out.")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# scoring
# ---------------------------------------------------------------------------


def _empty_metrics() -> dict[str, int]:
    return {
        "rows": 0,
        "top1_hits": 0,
        "top3_hits": 0,
        "loose_top1_hits": 0,
        "loose_top3_hits": 0,
        "abstained": 0,
        "wrong": 0,
        "loose_wrong": 0,
        "missing": 0,
        "errors": 0,
        "cause_rows": 0,
        "cause_hits": 0,
    }


def _add(metrics: dict[str, int], other: dict[str, int]) -> None:
    for field in metrics:
        metrics[field] += other[field]


def _group_value(row: dict[str, Any], dimension: str) -> str:
    if dimension == "culprit_in_log":
        value = row.get(dimension)
        if value is True:
            return "true"
        if value is False:
            return "false"
        return "unknown"
    value = row.get(dimension)
    if isinstance(value, str) and value:
        return value
    return "unknown"


def _load_key(key_path: Path) -> list[dict[str, Any]]:
    key = _read_jsonl(key_path, "key")
    if not key:
        raise BenchmarkError(f"key file {key_path} has no rows")
    seen: set[str] = set()
    for row in key:
        row_id = _string_field(row, "id", f"key {key_path}")
        if row_id in seen:
            raise BenchmarkError(f"duplicate id in key: {row_id!r}")
        seen.add(row_id)
        if row.get("culprit") is None:
            raise BenchmarkError(f"key row {row_id!r} has a null culprit")
        accept = _validate_accept(
            row.get("accept"), f"key {key_path}: row {row_id!r}"
        )
        if accept is not None:
            row["accept"] = accept
    return key


def _validate_predictions(rows: Iterable[dict[str, Any]], where: str) -> dict[str, dict[str, Any]]:
    predictions: dict[str, dict[str, Any]] = {}
    for row in rows:
        row_id = _string_field(row, "id", where)
        if row_id in predictions:
            raise BenchmarkError(f"duplicate prediction id {row_id!r}")
        error = row.get("error")
        if error is not None:
            if not isinstance(error, str) or not error:
                raise BenchmarkError(
                    f"prediction {row_id!r}: 'error' must be a non-empty string"
                )
            predictions[row_id] = row
            continue
        abstained = row.get("abstained")
        if not isinstance(abstained, bool):
            raise BenchmarkError(
                f"prediction {row_id!r}: 'abstained' must be a boolean"
            )
        cause = row.get("cause")
        if cause is not None and cause not in CAUSE_KINDS:
            raise BenchmarkError(
                f"prediction {row_id!r}: 'cause' must be one of {', '.join(CAUSE_KINDS)}"
            )
        suspects = row.get("suspects")
        if not isinstance(suspects, list):
            raise BenchmarkError(
                f"prediction {row_id!r}: 'suspects' must be a list"
            )
        if abstained and suspects:
            raise BenchmarkError(
                f"prediction {row_id!r}: abstained rows must not list suspects"
            )
        if not abstained and not suspects:
            raise BenchmarkError(
                f"prediction {row_id!r}: an answer needs at least one suspect"
            )
        for suspect in suspects:
            if not isinstance(suspect, dict):
                raise BenchmarkError(
                    f"prediction {row_id!r}: each suspect must be an object"
                )
            name = suspect.get("name")
            if not isinstance(name, str) or not name:
                raise BenchmarkError(
                    f"prediction {row_id!r}: each suspect needs a non-empty name"
                )
            confidence = suspect.get("confidence")
            if confidence is not None and (
                isinstance(confidence, bool)
                or not isinstance(confidence, (int, float))
            ):
                raise BenchmarkError(
                    f"prediction {row_id!r}: confidence must be a number or null"
                )
        predictions[row_id] = row
    return predictions


def _load_predictions(predictions_path: Path) -> dict[str, dict[str, Any]]:
    rows = _read_jsonl(predictions_path, "predictions")
    if not rows:
        raise BenchmarkError(
            f"predictions file {predictions_path} has no rows; "
            "an empty file is not a perfect score"
        )
    return _validate_predictions(rows, f"predictions {predictions_path}")


def _score(
    key: list[dict[str, Any]],
    predictions: dict[str, dict[str, Any]],
    tool: str,
    detail: bool,
) -> dict[str, Any]:
    key_ids = {row["id"] for row in key}
    unknown = sorted(set(predictions) - key_ids)
    if unknown:
        raise BenchmarkError(
            f"prediction id(s) not in the key: {', '.join(unknown)}"
        )

    overall = _empty_metrics()
    by: dict[str, dict[str, dict[str, int]]] = {
        dimension: {} for dimension in GROUP_DIMENSIONS
    }
    detail_rows: list[dict[str, Any]] = []

    for row in sorted(key, key=lambda item: item["id"]):
        row_id = row["id"]
        strict_names = _strict_names(row)
        segments = _loose_segments(row)
        prediction = predictions.get(row_id)
        metrics = _empty_metrics()
        metrics["rows"] = 1

        key_cause = cause_kind(row.get("culprit_kind"))
        predicted_cause = (
            cause_kind(prediction.get("cause")) if prediction is not None else None
        )
        if key_cause is not None:
            metrics["cause_rows"] = 1
            if predicted_cause == key_cause:
                metrics["cause_hits"] = 1

        top1_hit = False
        top3_hit = False
        top1_loose = False
        top3_loose = False
        matched_by: str | None = None

        if prediction is None:
            metrics["abstained"] = 1
            metrics["missing"] = 1
            outcome = "missing"
            top1_name = None
        elif "error" in prediction:
            metrics["errors"] = 1
            outcome = "error"
            top1_name = None
        elif prediction["abstained"]:
            metrics["abstained"] = 1
            outcome = "abstained"
            top1_name = None
        else:
            suspects = prediction["suspects"]
            names = [normalize_name(suspect["name"]) for suspect in suspects]
            top1_name = suspects[0]["name"]
            top1_hit = names[0] in strict_names
            top3_hit = any(name in strict_names for name in names[:3])
            top1_loose = _loose_hit(names[0], segments)
            top3_loose = any(_loose_hit(name, segments) for name in names[:3])
            if top1_hit:
                metrics["top1_hits"] = 1
                matched_by = "strict"
            else:
                metrics["wrong"] = 1
                if top1_loose:
                    matched_by = "loose"
            if top3_hit:
                metrics["top3_hits"] = 1
            if top1_loose:
                metrics["loose_top1_hits"] = 1
            if top3_loose:
                metrics["loose_top3_hits"] = 1
            if not top1_loose:
                # Named someone, and not even loosely the culprit: wrong by any reading.
                metrics["loose_wrong"] = 1
            outcome = "hit" if top1_hit else "wrong"

        _add(overall, metrics)
        for dimension in GROUP_DIMENSIONS:
            bucket = by[dimension].setdefault(
                _group_value(row, dimension), _empty_metrics()
            )
            _add(bucket, metrics)

        if detail:
            detail_rows.append(
                {
                    "id": row_id,
                    "outcome": outcome,
                    "top1": top1_name,
                    "top3_hit": top3_hit,
                    "strict_top1_hit": top1_hit,
                    "loose_top1_hit": top1_loose,
                    "loose_top3_hit": top3_loose,
                    "matched_by": matched_by,
                    "cause": predicted_cause,
                    "key_cause": key_cause,
                    "groups": {
                        dimension: _group_value(row, dimension)
                        for dimension in GROUP_DIMENSIONS
                    },
                }
            )

    result: dict[str, Any] = {
        "tool": tool,
        "overall": overall,
        "by": {
            dimension: {
                value: by[dimension][value]
                for value in sorted(by[dimension])
            }
            for dimension in GROUP_DIMENSIONS
        },
    }
    if detail:
        result["rows"] = detail_rows
    return result


def score(
    key_path: str | Path,
    predictions_path: str | Path,
    tool: str | None = None,
    expect_sha256: str | None = None,
    detail: bool = False,
    i_am_done_writing_rules: bool = False,
) -> dict[str, Any]:
    """Score one predictions file against a key."""
    key_path = Path(key_path)
    predictions_path = Path(predictions_path)
    if (
        detail
        and key_path.name.casefold() == "test-key.jsonl"
        and not i_am_done_writing_rules
    ):
        raise BenchmarkError(
            "refusing --detail on test-key.jsonl: rule writers must not see test "
            "answers; pass --i-am-done-writing-rules once the rules are frozen"
        )
    if expect_sha256 is not None:
        actual = _sha256_file(key_path)
        if actual != expect_sha256.strip().lower():
            raise BenchmarkError(
                f"key {key_path} has sha256 {actual}, not the expected "
                f"{expect_sha256}; refusing to score a changed key"
            )
    key = _load_key(key_path)
    predictions = _load_predictions(predictions_path)
    return _score(key, predictions, tool or predictions_path.name, detail)


def _table(headers: list[str], rows: list[list[Any]]) -> str:
    widths = [len(header) for header in headers]
    for row in rows:
        for index, cell in enumerate(row):
            widths[index] = max(widths[index], len(str(cell)))
    lines = [
        "  ".join(header.ljust(widths[index]) for index, header in enumerate(headers)),
        "  ".join("-" * width for width in widths),
    ]
    for row in rows:
        lines.append(
            "  ".join(str(cell).ljust(widths[index]) for index, cell in enumerate(row))
        )
    return "\n".join(lines)


def _metrics_cells(metrics: dict[str, int]) -> list[Any]:
    return [
        metrics["rows"],
        metrics["top1_hits"],
        metrics["top3_hits"],
        metrics["loose_top1_hits"],
        metrics["loose_top3_hits"],
        metrics["abstained"],
        metrics["wrong"],
        metrics["loose_wrong"],
        metrics["missing"],
        metrics["errors"],
        f"{metrics['cause_hits']}/{metrics['cause_rows']}",
    ]


METRIC_HEADERS = [
    "rows",
    "top-1",
    "top-3",
    "loose top-1",
    "loose top-3",
    "abstained",
    "wrong",
    "loose wrong",
    "missing",
    "errors",
    "cause",
]


def format_score(result: dict[str, Any], detail: bool) -> str:
    rows: list[list[Any]] = [["overall", *_metrics_cells(result["overall"])]]
    for dimension in GROUP_DIMENSIONS:
        for value, metrics in result["by"][dimension].items():
            rows.append(
                [f"{dimension}={value}", *_metrics_cells(metrics)]
            )
    lines = [f"Tool: {result['tool']}", _table(["group", *METRIC_HEADERS], rows)]
    if detail:
        detail_rows = [
            [
                row["id"],
                row["outcome"],
                row["top1"] or "-",
                row["top3_hit"],
                row["loose_top1_hit"],
                row["loose_top3_hit"],
                row["matched_by"] or "-",
                row["cause"] or "-",
                row["key_cause"] or "-",
            ]
            for row in result["rows"]
        ]
        lines.append("")
        lines.append(
            _table(
                [
                    "id",
                    "outcome",
                    "top-1 suspect",
                    "top-3 hit",
                    "loose top-1",
                    "loose top-3",
                    "matched by",
                    "cause",
                    "key cause",
                ],
                detail_rows,
            )
        )
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# compare
# ---------------------------------------------------------------------------


def abstain_always_predictions(
    key: Iterable[dict[str, Any]],
) -> dict[str, dict[str, Any]]:
    return {
        row["id"]: {"id": row["id"], "suspects": [], "abstained": True}
        for row in key
    }


def compare(
    key_path: str | Path, tool_specs: list[tuple[str, str | Path]]
) -> list[dict[str, Any]]:
    """Score every tool plus the built-in abstain-always baseline."""
    key_path = Path(key_path)
    key = _load_key(key_path)
    results = [_score(key, abstain_always_predictions(key), "abstain-always", False)]
    for name, predictions_path in tool_specs:
        predictions = _load_predictions(Path(predictions_path))
        results.append(_score(key, predictions, name, False))
    return results


def format_compare(results: list[dict[str, Any]]) -> str:
    rows = [
        [result["tool"], *_metrics_cells(result["overall"])] for result in results
    ]
    return _table(["tool", *METRIC_HEADERS], rows)


# ---------------------------------------------------------------------------
# predict: run a tool and turn its answer into predictions
# ---------------------------------------------------------------------------


def _split_command(command: str) -> list[str]:
    """Split `--agora` into argv without ever handing a shell a string."""
    if not isinstance(command, str) or not command.strip():
        raise BenchmarkError("--agora needs a command to run")
    parts = shlex.split(command, posix=os.name != "nt")
    if os.name == "nt":
        # posix=False keeps the quotes around a path with spaces; drop them.
        parts = [
            part[1:-1]
            if len(part) >= 2 and part[0] == part[-1] and part[0] in "\"'"
            else part
            for part in parts
        ]
    if not parts:
        raise BenchmarkError("--agora needs a command to run")
    return parts


def _read_ids(path: str | Path, what: str) -> list[str]:
    """Ids from a key file's JSON rows, or one per line in a plain list."""
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as error:
        raise BenchmarkError(f"cannot read {what} file {path}: {error}") from error
    ids: list[str] = []
    seen: set[str] = set()
    for line_number, line in enumerate(text.splitlines(), 1):
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("{"):
            try:
                value = json.loads(stripped).get("id")
            except (json.JSONDecodeError, AttributeError) as error:
                raise BenchmarkError(
                    f"{what} {path}:{line_number}: invalid JSON: {error}"
                ) from error
            if not isinstance(value, str) or not value:
                raise BenchmarkError(f"{what} {path}:{line_number}: no `id`")
        else:
            value = stripped
        if value in seen:
            raise BenchmarkError(
                f"{what} {path}:{line_number}: duplicate id {value!r}"
            )
        seen.add(value)
        ids.append(value)
    if not ids:
        raise BenchmarkError(f"{what} file {path} names no ids")
    return ids


def _short_message(text: str, limit: int = 300) -> str:
    collapsed = " ".join(text.split())
    if len(collapsed) <= limit:
        return collapsed
    return collapsed[:limit] + "..."


def _prediction_from_run(
    row_id: str, completed: subprocess.CompletedProcess[str]
) -> dict[str, Any]:
    """Turn one analyzer run into a row. A failed run is an error, never silence."""
    stdout = completed.stdout or ""
    stderr = completed.stderr or ""
    payload: Any = None
    if stdout.strip():
        try:
            payload = json.loads(stdout)
        except json.JSONDecodeError:
            payload = None
    if payload is None:
        reason = f"the analyzer exited {completed.returncode} without parsable JSON"
        if stderr.strip():
            reason += f": {_short_message(stderr)}"
        return {"id": row_id, "error": reason}
    if not isinstance(payload, dict) or not isinstance(payload.get("answer"), dict):
        return {"id": row_id, "error": "the analyzer's JSON has no answer object"}
    answer = payload["answer"]
    candidates = answer.get("candidates")
    if not isinstance(candidates, list):
        return {
            "id": row_id,
            "error": "the analyzer's answer has no candidates list",
        }

    files = payload.get("files")
    read_by: Any = None
    file_reason: Any = None
    if isinstance(files, list) and files and isinstance(files[0], dict):
        read_by = files[0].get("read_by")
        file_reason = files[0].get("reason")

    suspects: list[dict[str, Any]] = []
    skipped_unnamed = 0
    for candidate in candidates:
        owner = candidate.get("owner") if isinstance(candidate, dict) else None
        name = owner.get("name") if isinstance(owner, dict) else None
        if isinstance(name, str) and name.strip():
            suspects.append({"name": name, "confidence": None})
        else:
            skipped_unnamed += 1

    row: dict[str, Any] = {
        "id": row_id,
        "suspects": suspects,
        "abstained": not suspects,
        "read_by": read_by,
    }
    cause = cause_kind(answer.get("cause"))
    if cause is not None:
        row["cause"] = cause
    if skipped_unnamed:
        row["skipped_unnamed"] = skipped_unnamed
    if file_reason and not suspects:
        row["reason"] = file_reason
    return row


def _write_jsonl_atomic(path: Path, rows: list[dict[str, Any]]) -> None:
    """Write the whole file beside its target, then swap it in."""
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(handle, "w", encoding="utf-8", newline="\n") as stream:
            for row in rows:
                stream.write(_json_line(row))
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


def predict(
    manifest_path: str | Path,
    corpus_root: str | Path,
    ids_path: str | Path,
    out_path: str | Path,
    agora: str,
    plugin_dirs: Iterable[str | Path] = (),
    allow_code: bool = False,
    jobs: int = 1,
    timeout: float = 120.0,
) -> dict[str, Any]:
    """Run `agora crash analyze` for each id and write a predictions file.

    Arguments are passed as a list, never through a shell, so a path with
    spaces or quotes is safe. The analyzer exits non-zero when it read no file
    but still prints JSON; that is an abstention and the file report's reason
    is kept. A run that times out, exits non-zero with no JSON, or prints JSON
    that does not parse becomes an `error` row, never an abstention.
    """
    if not isinstance(jobs, int) or jobs < 1:
        raise BenchmarkError("--jobs must be at least 1")
    if timeout is None or timeout <= 0:
        raise BenchmarkError("--timeout must be greater than 0")

    manifest_path = Path(manifest_path)
    corpus_root = Path(corpus_root)
    out = Path(out_path)
    if _inside_git_work_tree(out):
        raise BenchmarkError(
            f"refusing --out {out}: it is inside a git work tree, and benchmark "
            "predictions must never enter a repository"
        )

    manifest = {}
    for row in _read_jsonl(manifest_path, "manifest"):
        manifest_id = _string_field(row, "id", f"manifest {manifest_path}")
        file_field = _string_field(row, "file", f"manifest id {manifest_id!r}")
        manifest[manifest_id] = file_field
    if not manifest:
        raise BenchmarkError(f"manifest file {manifest_path} has no rows")

    ids = _read_ids(ids_path, "ids")
    command = _split_command(agora)
    plugin_options: list[str] = []
    for folder in plugin_dirs:
        plugin_options.extend(["--plugin-dir", str(folder)])
    if allow_code:
        plugin_options.append("--allow-code")

    jobs_by_id: list[tuple[str, Path]] = []
    for row_id in ids:
        if row_id not in manifest:
            raise BenchmarkError(f"ids file row {row_id!r} is not in the manifest")
        jobs_by_id.append((row_id, corpus_root / manifest[row_id]))

    def run_one(row_id: str, log_path: Path) -> dict[str, Any]:
        if not log_path.is_file():
            return {"id": row_id, "error": f"log file not found: {log_path}"}
        argv = [
            *command,
            "crash",
            "analyze",
            str(log_path),
            "--json",
            *plugin_options,
        ]
        try:
            completed = subprocess.run(
                argv,
                capture_output=True,
                text=True,
                timeout=timeout,
                stdin=subprocess.DEVNULL,
            )
        except subprocess.TimeoutExpired:
            return {"id": row_id, "error": f"timed out after {timeout:g} s"}
        except OSError as error:
            return {"id": row_id, "error": f"could not run the analyzer: {error}"}
        return _prediction_from_run(row_id, completed)

    if jobs == 1:
        rows = [run_one(row_id, log_path) for row_id, log_path in jobs_by_id]
    else:
        with ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = [
                pool.submit(run_one, row_id, log_path)
                for row_id, log_path in jobs_by_id
            ]
            rows = [future.result() for future in futures]

    _write_jsonl_atomic(out, rows)

    summary = {
        "rows": len(rows),
        "answered": sum(1 for row in rows if "error" not in row and not row["abstained"]),
        "abstained": sum(1 for row in rows if "error" not in row and row["abstained"]),
        "errors": sum(1 for row in rows if "error" in row),
        "no_parser": sum(
            1 for row in rows if "error" not in row and row.get("read_by") is None
        ),
        "skipped_unnamed": sum(row.get("skipped_unnamed", 0) for row in rows),
        "out": str(out),
    }
    return summary


def format_predict(summary: dict[str, Any]) -> str:
    lines = [
        f"Predicted {summary['rows']} rows: "
        f"answered {summary['answered']}, abstained {summary['abstained']}, "
        f"errors {summary['errors']}",
        f"No parser recognised {summary['no_parser']} log(s)",
        f"Skipped {summary['skipped_unnamed']} candidate(s) with no owner name",
        f"Wrote {summary['out']}",
    ]
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _parse_tool_specs(specs: list[str]) -> list[tuple[str, str]]:
    parsed: list[tuple[str, str]] = []
    names: set[str] = set()
    for spec in specs:
        name, separator, path = spec.partition("=")
        if not separator or not name or not path:
            raise BenchmarkError(
                f"expected TOOL=PREDICTIONS, got {spec!r}"
            )
        if name in names:
            raise BenchmarkError(f"duplicate tool name {name!r}")
        names.add(name)
        parsed.append((name, path))
    return parsed


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Freeze and score game-agnostic crash benchmark keys."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    freeze_parser = subparsers.add_parser(
        "freeze", help="build dev/test answer keys from labels and a manifest"
    )
    freeze_parser.add_argument("--labels", required=True)
    freeze_parser.add_argument("--manifest", required=True)
    freeze_parser.add_argument("--out", required=True)
    freeze_parser.add_argument("--salt", default="")
    freeze_parser.add_argument("--test-fraction", type=float, default=0.5)
    freeze_parser.add_argument("--force", action="store_true")
    freeze_parser.add_argument(
        "--dev-ids",
        help="file of ids (one per line, or JSON lines with `id`) that must go "
        "to dev because they were already studied",
    )

    score_parser = subparsers.add_parser(
        "score", help="score one predictions file against a key"
    )
    score_parser.add_argument("--key", required=True)
    score_parser.add_argument("--predictions", required=True)
    score_parser.add_argument("--tool")
    score_parser.add_argument("--expect-sha256")
    score_parser.add_argument("--detail", action="store_true")
    score_parser.add_argument(
        "--i-am-done-writing-rules", action="store_true", dest="done_writing_rules"
    )
    score_parser.add_argument("--json", action="store_true")

    compare_parser = subparsers.add_parser(
        "compare", help="score several tools in one table"
    )
    compare_parser.add_argument("--key", required=True)
    compare_parser.add_argument("tools", nargs="+", metavar="TOOL=PREDICTIONS")

    predict_parser = subparsers.add_parser(
        "predict", help="run a tool over a corpus and write a predictions file"
    )
    predict_parser.add_argument("--manifest", required=True)
    predict_parser.add_argument("--corpus-root", required=True)
    predict_parser.add_argument(
        "--ids", required=True, help="a key file's rows or a plain list of ids"
    )
    predict_parser.add_argument("--out", required=True)
    predict_parser.add_argument(
        "--agora", required=True, help='the analyzer command, for example "agora.exe"'
    )
    predict_parser.add_argument(
        "--plugin-dir", action="append", default=[], dest="plugin_dirs"
    )
    predict_parser.add_argument("--allow-code", action="store_true")
    predict_parser.add_argument("--jobs", type=int, default=1)
    predict_parser.add_argument("--timeout", type=float, default=120.0)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.command == "freeze":
            summary = freeze(
                args.labels,
                args.manifest,
                args.out,
                salt=args.salt,
                test_fraction=args.test_fraction,
                force=args.force,
                dev_ids_path=args.dev_ids,
            )
            print(format_freeze(summary, args.out))
            return 0
        if args.command == "score":
            result = score(
                args.key,
                args.predictions,
                tool=args.tool,
                expect_sha256=args.expect_sha256,
                detail=args.detail,
                i_am_done_writing_rules=args.done_writing_rules,
            )
            if args.json:
                print(json.dumps(result, indent=2, sort_keys=True))
            else:
                print(format_score(result, args.detail))
            return 0
        if args.command == "compare":
            results = compare(args.key, _parse_tool_specs(args.tools))
            print(format_compare(results))
            return 0
        if args.command == "predict":
            summary = predict(
                args.manifest,
                args.corpus_root,
                args.ids,
                args.out,
                args.agora,
                plugin_dirs=args.plugin_dirs,
                allow_code=args.allow_code,
                jobs=args.jobs,
                timeout=args.timeout,
            )
            print(format_predict(summary))
            return 0
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
