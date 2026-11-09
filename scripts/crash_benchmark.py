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
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path
from typing import Any, Iterable

EXTENSIONS = (".dll", ".esp", ".esm", ".esl", ".jar", ".pex")
GROUP_DIMENSIONS = ("game", "log_type", "difficulty", "culprit_in_log")


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
    key: list[dict[str, Any]], salt: str, test_fraction: float
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Split by manifest sha256, stratified per game, deterministically.

    Rows that share a manifest sha256 are one group, so the same log posted
    twice always lands on the same side. Within a game, groups are ordered by
    sha256(salt + the group's lexicographically smallest id); the first
    round(n * test_fraction) go to test.
    """
    groups: dict[str, list[dict[str, Any]]] = {}
    for row in key:
        groups.setdefault(row["sha256"], []).append(row)

    per_game: dict[str, list[tuple[int, str]]] = {}
    for sha, rows in groups.items():
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


def freeze(
    labels_path: str | Path,
    manifest_path: str | Path,
    out_dir: str | Path,
    salt: str = "",
    test_fraction: float = 0.5,
    force: bool = False,
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
        key.append(
            {
                "id": row_id,
                "game": game,
                "log_type": log_type,
                "difficulty": row.get("difficulty"),
                "culprit_kind": row.get("culprit_kind"),
                "culprit_in_log": row.get("culprit_in_log"),
                "culprit": culprit,
                "sha256": sha256,
            }
        )

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

    dev_rows, test_rows = _split_key(key, salt, test_fraction)

    def public(row: dict[str, Any]) -> dict[str, Any]:
        return {field: row[field] for field in (
            "id", "game", "log_type", "difficulty", "culprit_kind",
            "culprit_in_log", "culprit",
        )}

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
        "abstained": 0,
        "wrong": 0,
        "missing": 0,
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
    return key


def _validate_predictions(rows: Iterable[dict[str, Any]], where: str) -> dict[str, dict[str, Any]]:
    predictions: dict[str, dict[str, Any]] = {}
    for row in rows:
        row_id = _string_field(row, "id", where)
        if row_id in predictions:
            raise BenchmarkError(f"duplicate prediction id {row_id!r}")
        abstained = row.get("abstained")
        if not isinstance(abstained, bool):
            raise BenchmarkError(
                f"prediction {row_id!r}: 'abstained' must be a boolean"
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
        culprit = normalize_name(row["culprit"])
        prediction = predictions.get(row_id)
        metrics = _empty_metrics()
        metrics["rows"] = 1

        if prediction is None:
            metrics["abstained"] = 1
            metrics["missing"] = 1
            outcome = "missing"
            top1_name = None
            top3_hit = False
        elif prediction["abstained"]:
            metrics["abstained"] = 1
            outcome = "abstained"
            top1_name = None
            top3_hit = False
        else:
            suspects = prediction["suspects"]
            names = [normalize_name(suspect["name"]) for suspect in suspects]
            top1_name = suspects[0]["name"]
            top1_hit = names[0] == culprit
            top3_hit = culprit in names[:3]
            if top1_hit:
                metrics["top1_hits"] = 1
            else:
                metrics["wrong"] = 1
            if top3_hit:
                metrics["top3_hits"] = 1
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
        metrics["abstained"],
        metrics["wrong"],
        metrics["missing"],
    ]


METRIC_HEADERS = ["rows", "top-1", "top-3", "abstained", "wrong", "missing"]


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
            [row["id"], row["outcome"], row["top1"] or "-", row["top3_hit"]]
            for row in result["rows"]
        ]
        lines.append("")
        lines.append(
            _table(["id", "outcome", "top-1 suspect", "top-3 hit"], detail_rows)
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
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
