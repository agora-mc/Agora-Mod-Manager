#!/usr/bin/env python3
"""Generate an Address Library id -> function name table from CommonLibSSE-NG.

Usage::

    python scripts/gen_addrlib_names.py .spike/CommonLibSSE-NG
    python scripts/gen_addrlib_names.py <checkout> --out <json> --js <js>

CommonLibSSE-NG pairs a game function with its Address Library id inside the
function that wraps it, as ``RELOCATION_ID(se, ae)``, ``REL::RelocationID(se,
ae)``, ``REL::VariantID(se, ae, vr)`` or ``REL::ID(id)``. This script walks the
checkout, works out which function each occurrence belongs to, and writes the
pairs for Skyrim SE and Skyrim AE separately.

Only the standard library is used. The output is deterministic for one commit:
files are walked in sorted order, an id named twice keeps the first name
(sorted path, then position) and the rest are counted as conflicts, and both
files are written with LF endings. The generator never downloads anything.

The source metadata (repository, commit, licence) is read from a sibling
``<checkout>.commit`` file unless the matching option is given. Its first line
is ``<repository-url> <commit>`` and its remaining lines are the licence.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Iterable

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_JSON = (
    REPO_ROOT / "packages" / "official" / "creation-engine" / "crash" / "addrlib-names.json"
)
DEFAULT_JS = DEFAULT_JSON.with_suffix(".js")

SOURCE_SUFFIXES = {".h", ".hpp", ".hxx", ".inl", ".cpp", ".cc", ".cxx"}
SKIP_DIRS = {"test", "tests", "testing", "third_party", "third-party", "external", "vendor"}

# Macros that select a runtime in a preprocessor condition. A condition this
# table cannot read is left as "every runtime", which only makes a bare
# `REL::ID` ambiguous; it never invents a runtime.
RUNTIME_MACROS = {
    "ENABLE_SKYRIM_SE": frozenset({"se"}),
    "ENABLE_SKYRIM_AE": frozenset({"ae"}),
    "ENABLE_SKYRIM_VR": frozenset({"vr"}),
    "EXCLUSIVE_SKYRIM_SE": frozenset({"se"}),
    "EXCLUSIVE_SKYRIM_AE": frozenset({"ae"}),
    "EXCLUSIVE_SKYRIM_FLAT": frozenset({"se", "ae"}),
    "EXCLUSIVE_SKYRIM_VR": frozenset({"vr"}),
    "SKYRIM_CROSS_VR": frozenset({"se", "ae", "vr"}),
}
ALL_RUNTIMES = frozenset({"se", "ae", "vr"})

ID_CALL = re.compile(r"\b(RELOCATION_ID|REL::RelocationID|REL::VariantID|REL::ID)\s*\(")
ID_DECLARATION = re.compile(r"\bREL::(VariantID|RelocationID|ID)\s+[A-Za-z_]\w*\s*\(")
OPERATOR_NAME = r"operator\s*(?:\(\s*\)|\[\s*\]|[^\s(]+)"
NAME_AT_END = re.compile(
    r"([A-Za-z_]\w*(?:::[~A-Za-z_]\w*)*::" + OPERATOR_NAME + r""
    r"|" + OPERATOR_NAME + r""
    r"|~?[A-Za-z_]\w*(?:::[~A-Za-z_]\w*)*)\s*$"
)
CONTROL_WORDS = {
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "do",
    "else",
    "return",
    "sizeof",
    "alignof",
    "decltype",
    "static_assert",
}
SPECIFIERS = {
    "inline",
    "static",
    "virtual",
    "constexpr",
    "consteval",
    "explicit",
    "friend",
    "extern",
    "FORCEINLINE",
    "SKYRIM_REL",
    "SKYRIM_REL_CONSTEXPR",
    "SKYRIM_REL_VR",
    "SKYRIM_REL_VR_CONSTEXPR",
    "SKYRIM_REL_VR_VIRTUAL",
    "SKYRIM_REL_VR_OVERRIDE",
}


class GenerationError(Exception):
    """A checkout or option the generator cannot use."""


# ---------------------------------------------------------------------------
# Text preparation
# ---------------------------------------------------------------------------


def strip_comments_and_literals(text: str) -> str:
    """Blank comments and the contents of string/char literals, keeping offsets.

    Newlines are kept so line numbers stay correct. Braces, parentheses and
    commas inside a comment or a literal must never reach the scanner.
    """
    out = list(text)
    index = 0
    length = len(text)
    state = "code"
    while index < length:
        char = text[index]
        nxt = text[index + 1] if index + 1 < length else ""
        if state == "code":
            if char == "/" and nxt == "/":
                out[index] = out[index + 1] = " "
                index += 2
                state = "line"
                continue
            if char == "/" and nxt == "*":
                out[index] = out[index + 1] = " "
                index += 2
                state = "block"
                continue
            if char == '"':
                out[index] = " "
                index += 1
                state = "string"
                continue
            if char == "'":
                out[index] = " "
                index += 1
                state = "char"
                continue
            index += 1
            continue
        if state == "line":
            if char == "\n":
                state = "code"
            else:
                out[index] = " "
            index += 1
            continue
        if state == "block":
            if char == "*" and nxt == "/":
                out[index] = out[index + 1] = " "
                index += 2
                state = "code"
                continue
            if char != "\n":
                out[index] = " "
            index += 1
            continue
        # string or char
        if char == "\\":
            out[index] = " "
            if index + 1 < length and text[index + 1] != "\n":
                out[index + 1] = " "
            index += 2
            continue
        if char == "\n":
            # An unterminated literal must not eat the rest of the file.
            state = "code"
            index += 1
            continue
        out[index] = " "
        if (state == "string" and char == '"') or (state == "char" and char == "'"):
            state = "code"
        index += 1
    return "".join(out)


def _balanced(text: str, start: int) -> int:
    """Index just past the group opened at `start`, or -1 when it never closes."""
    depth = 0
    for index in range(start, len(text)):
        char = text[index]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index + 1
    return -1


def _split_top_level(text: str) -> list[str]:
    parts = []
    depth = 0
    current = []
    for char in text:
        if char in "(<[{":
            depth += 1
        elif char in ")>]}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current).strip())
            current = []
        else:
            current.append(char)
    if current:
        parts.append("".join(current).strip())
    return parts


# ---------------------------------------------------------------------------
# Preprocessor runtime contexts
# ---------------------------------------------------------------------------


class ConditionParser:
    def __init__(self, tokens: list[str]):
        self.tokens = tokens
        self.at = 0

    def peek(self) -> str | None:
        return self.tokens[self.at] if self.at < len(self.tokens) else None

    def take(self) -> str | None:
        token = self.peek()
        if token is not None:
            self.at += 1
        return token

    def parse(self) -> frozenset[str]:
        value = self.parse_or()
        # Ignore anything trailing rather than mis-reading it.
        return value

    def parse_or(self) -> frozenset[str]:
        value = self.parse_and()
        while self.peek() == "||":
            self.take()
            value = value | self.parse_and()
        return value

    def parse_and(self) -> frozenset[str]:
        value = self.parse_unary()
        while self.peek() == "&&":
            self.take()
            value = value & self.parse_unary()
        return value

    def parse_unary(self) -> frozenset[str]:
        token = self.take()
        if token is None:
            return ALL_RUNTIMES
        if token == "!":
            return ALL_RUNTIMES - self.parse_unary()
        if token == "(":
            value = self.parse_or()
            if self.peek() == ")":
                self.take()
            return value
        if token == "defined":
            name = self.take()
            if name == "(":
                name = self.take()
                if self.peek() == ")":
                    self.take()
            return self.macro(name)
        if token.isdigit():
            return ALL_RUNTIMES if int(token) != 0 else frozenset()
        return self.macro(token)

    @staticmethod
    def macro(name: str | None) -> frozenset[str]:
        if name is None:
            return ALL_RUNTIMES
        return RUNTIME_MACROS.get(name, ALL_RUNTIMES)


def condition_runtimes(expression: str) -> frozenset[str]:
    tokens = re.findall(r"defined|[A-Za-z_]\w*|\d+|&&|\|\||!|\(|\)", expression)
    return ConditionParser(tokens).parse()


def line_runtimes(text: str) -> list[frozenset[str]]:
    """The runtimes each line can be compiled for, from its preprocessor guards."""
    contexts: list[frozenset[str]] = []
    stack: list[dict] = []
    current = ALL_RUNTIMES
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            directive = stripped[1:].strip()
            word = directive.split(None, 1)[0] if directive else ""
            rest = directive[len(word) :].strip()
            if word in ("if", "ifdef", "ifndef"):
                if word == "ifdef":
                    condition = condition_runtimes("defined(" + rest + ")")
                elif word == "ifndef":
                    condition = ALL_RUNTIMES - condition_runtimes("defined(" + rest + ")")
                else:
                    condition = condition_runtimes(rest)
                entry = {
                    "parent": current,
                    "taken": condition,
                    "current": current & condition,
                }
                stack.append(entry)
                current = entry["current"]
            elif word == "elif" and stack:
                entry = stack[-1]
                condition = condition_runtimes(rest)
                entry["current"] = entry["parent"] & condition & (ALL_RUNTIMES - entry["taken"])
                entry["taken"] = entry["taken"] | condition
                current = entry["current"]
            elif word == "else" and stack:
                entry = stack[-1]
                entry["current"] = entry["parent"] & (ALL_RUNTIMES - entry["taken"])
                entry["taken"] = ALL_RUNTIMES
                current = entry["current"]
            elif word == "endif" and stack:
                entry = stack.pop()
                current = entry["parent"]
            contexts.append(current)
            continue
        contexts.append(current)
    return contexts


# ---------------------------------------------------------------------------
# Scope and declaration scanning
# ---------------------------------------------------------------------------


def _strip_leading(text: str) -> str:
    text = text.strip()
    while True:
        previous = text
        text = re.sub(r"^template\s*<", "", text)
        if text is not previous and text != previous:
            # Remove the balanced template argument list.
            depth = 1
            for index, char in enumerate(text):
                if char == "<":
                    depth += 1
                elif char == ">":
                    depth -= 1
                    if depth == 0:
                        text = text[index + 1 :].lstrip()
                        break
            else:
                return text
        if text.startswith("[["):
            end = text.find("]]")
            if end < 0:
                return text
            text = text[end + 2 :].lstrip()
            continue
        match = re.match(r"([A-Za-z_]\w*)\b", text)
        if match and match.group(1) in SPECIFIERS:
            text = text[match.end() :].lstrip()
            continue
        return text


def _decltype_name(body: str) -> str | None:
    """The exact function name a `decltype(&Name)` hint names, when there is one."""
    at = body.find("decltype")
    while at >= 0:
        open_paren = body.find("(", at)
        close = _balanced(body, open_paren) if open_paren >= 0 else -1
        if open_paren < 0 or close < 0:
            return None
        inner = body[open_paren + 1 : close - 1].strip()
        if inner.startswith("&"):
            inner = inner[1:].strip()
            if re.fullmatch(
                r"[A-Za-z_]\w*(?:::[~A-Za-z_]\w*)*(?:::operator\s*(?:\(\s*\)|\[\s*\]|[^\s(]+))?",
                inner,
            ):
                return " ".join(inner.split())
        at = body.find("decltype", close)
    return None


def function_name(header: str, class_path: str) -> str | None:
    """The name a definition header declares, or None when it is not a function."""
    cleaned = _strip_leading(_directive_lines_removed(header))
    if not cleaned:
        return None
    first = re.match(r"[A-Za-z_]\w*", cleaned)
    if first and first.group(0) in CONTROL_WORDS:
        return None
    depth = 0
    paren = -1
    index = 0
    while index < len(cleaned):
        char = cleaned[index]
        if char in "<[":
            depth += 1
        elif char in ">]":
            depth -= 1
        elif char == "(" and depth == 0:
            # `operator()(args)` has an empty parameter group of its own; the
            # declaration's parameters are the group after it.
            before = cleaned[:index].rstrip()
            if before.endswith("operator"):
                close = _balanced(cleaned, index)
                after = cleaned[close:].lstrip() if close >= 0 else ""
                if after.startswith("("):
                    index = close
                    continue
            paren = index
            break
        index += 1
    if paren < 0:
        return None
    before = cleaned[:paren]
    if "=" in before:
        return None
    match = NAME_AT_END.search(before)
    if not match:
        return None
    name = " ".join(match.group(1).split())
    word = name.split("::")[0]
    if word in CONTROL_WORDS or name.endswith("operator"):
        return None
    if "::" in name:
        return name.lstrip(":")
    if not class_path:
        return name
    return class_path + "::" + name


def _directive_lines_removed(text: str) -> str:
    return " ".join(
        line for line in text.splitlines() if not line.strip().startswith("#")
    )


def _class_name(header: str) -> str | None:
    cleaned = _directive_lines_removed(header).strip()
    match = re.search(r"\b(?:class|struct|union)\s+([A-Za-z_]\w*)\b", cleaned)
    if not match:
        return None
    # `enum class Foo` is a scope too, but not one that holds functions.
    if re.search(r"\benum\s+class\s+[A-Za-z_]\w*\s*$", cleaned):
        return None
    return match.group(1)


def _namespace(header: str) -> str | None:
    cleaned = _directive_lines_removed(header).strip()
    if re.fullmatch(r"namespace(?:\s+[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)?", cleaned):
        return ""
    return None


class Occurrence:
    __slots__ = ("offset", "kind", "args", "line", "function", "class_path")

    def __init__(
        self,
        offset: int,
        kind: str,
        args: list[str],
        line: int,
        function: dict | None,
        class_path: str,
    ):
        self.offset = offset
        self.kind = kind
        self.args = args
        self.line = line
        self.function = function
        self.class_path = class_path


def scan_scopes(text: str) -> tuple[list[Occurrence], dict[str, int]]:
    """Find every id call and the function (or class) that encloses it.

    `text` must already be [stripped](strip_comments_and_literals).
    """
    occurrences: list[Occurrence] = []
    unparsed: dict[str, int] = {}

    scopes: list[dict] = []
    class_stack: list[str] = []
    last_boundary = 0
    index = 0
    length = len(text)

    def bump(shape: str) -> None:
        unparsed[shape] = unparsed.get(shape, 0) + 1

    def current_class() -> str:
        return "::".join(class_stack)

    while index < length:
        char = text[index]
        if char == "{":
            header = text[last_boundary:index]
            namespace = _namespace(header)
            class_name = _class_name(header)
            name = None
            if namespace is None and class_name is None:
                name = function_name(header, current_class())
            if namespace is not None:
                scopes.append({"kind": "namespace", "open": index, "name": ""})
            elif class_name is not None:
                scopes.append({"kind": "class", "open": index, "name": class_name})
                class_stack.append(class_name)
            elif name is not None:
                scopes.append({"kind": "function", "open": index, "name": name})
            else:
                scopes.append({"kind": "block", "open": index, "name": ""})
            last_boundary = index + 1
            index += 1
            continue
        if char == "}":
            if scopes:
                scope = scopes.pop()
                if scope["kind"] == "class" and class_stack:
                    class_stack.pop()
                if scope["kind"] == "function":
                    body = text[scope["open"] : index]
                    hint = _decltype_name(body[:2000])
                    if hint:
                        scope["name"] = hint
            last_boundary = index + 1
            index += 1
            continue
        if char == ";":
            last_boundary = index + 1
            index += 1
            continue
        match = ID_CALL.match(text, index)
        if match:
            kind = match.group(1)
            open_paren = match.end() - 1
            close = _balanced(text, open_paren)
            if close < 0:
                bump(kind + " unclosed")
                index = match.end()
                continue
            args = _split_top_level(text[open_paren + 1 : close - 1])
            line = text.count("\n", 0, index) + 1
            if all(re.fullmatch(r"(?:0[xX][0-9A-Fa-f]+|\d+)[uUlL]*", arg or "") for arg in args):
                function = next(
                    (scope for scope in reversed(scopes) if scope["kind"] == "function"),
                    None,
                )
                occurrences.append(
                    Occurrence(index, kind, args, line, function, current_class())
                )
            else:
                # A macro definition or a symbolic constant: no id can be read.
                prefix = text[last_boundary:index]
                if not _directive_lines_removed(prefix).strip().startswith("#"):
                    bump(kind + " with non-literal arguments")
            index = close
            continue
        declaration = ID_DECLARATION.match(text, index)
        if declaration:
            open_paren = declaration.end() - 1
            close = _balanced(text, open_paren)
            if close >= 0:
                args = _split_top_level(text[open_paren + 1 : close - 1])
                line = text.count("\n", 0, index) + 1
                if all(
                    re.fullmatch(r"(?:0[xX][0-9A-Fa-f]+|\d+)[uUlL]*", arg or "")
                    for arg in args
                ):
                    function = next(
                        (scope for scope in reversed(scopes) if scope["kind"] == "function"),
                        None,
                    )
                    occurrences.append(
                        Occurrence(
                            index,
                            "REL::" + declaration.group(1) + " declaration",
                            args,
                            line,
                            function,
                            current_class(),
                        )
                    )
                    index = close
                    continue
        index += 1

    # A file that ends inside a function (malformed or generated) still has a
    # range, so its ids are not silently dropped.
    while scopes:
        scope = scopes.pop()
        if scope["kind"] == "class" and class_stack:
            class_stack.pop()
        if scope["kind"] == "function":
            body = text[scope["open"] : length]
            hint = _decltype_name(body[:2000])
            if hint:
                scope["name"] = hint
    return occurrences, unparsed


# ---------------------------------------------------------------------------
# Id extraction
# ---------------------------------------------------------------------------


def _int_value(text: str) -> int | None:
    text = text.strip().rstrip("uUlL")
    try:
        if text.lower().startswith("0x"):
            return int(text, 16)
        return int(text)
    except ValueError:
        return None


def _declaration_name(text: str, offset: int) -> tuple[str, str] | None:
    """(shape, name) for an id at `offset` that sits outside every function."""
    start = max(text.rfind(";", 0, offset), text.rfind("}", 0, offset)) + 1
    statement = text[start : offset + 1]
    relocation = re.search(
        r"REL::Relocation\s*<[^{}]*>\s+([A-Za-z_]\w*)\s*\{[^{}]*$", statement, re.S
    )
    if relocation:
        return ("REL::Relocation declaration", relocation.group(1))
    table = re.search(
        r"std::array\s*<\s*REL::(?:VariantID|RelocationID|ID)[^{}]*>\s+([A-Za-z_]\w*)\s*\{",
        statement,
        re.S,
    )
    if table:
        return ("offset table (std::array of ids; not a function)", table.group(1))
    constant = re.search(
        r"(?:constexpr|inline|static|const)?\s*REL::(?:VariantID|RelocationID|ID)\s+([A-Za-z_]\w*)\s*\(",
        statement,
        re.S,
    )
    if constant:
        return ("namespace-scope id constant (not a function)", constant.group(1))
    return None


def _outer_class(scopes: list[dict]) -> str:
    names = [scope["name"] for scope in scopes if scope["kind"] == "class" and scope["name"]]
    return "::".join(names)



def extract(
    checkout: Path,
) -> tuple[dict[str, str], dict[str, str], dict[str, int], int]:
    """Return (se, ae, unparsed shape counts, conflict count)."""
    se: dict[str, str] = {}
    ae: dict[str, str] = {}
    unparsed: dict[str, int] = {}
    conflicts = 0

    def add(table: dict[str, str], identifier: int, name: str) -> None:
        nonlocal conflicts
        key = str(identifier)
        if key in table:
            if table[key] != name:
                conflicts += 1
            return
        table[key] = name

    def bump(shape: str) -> None:
        unparsed[shape] = unparsed.get(shape, 0) + 1

    for path in sorted(checkout.rglob("*"), key=lambda item: item.as_posix()):
        if not path.is_file() or path.suffix.lower() not in SOURCE_SUFFIXES:
            continue
        relative = path.relative_to(checkout)
        if any(part.lower() in SKIP_DIRS for part in relative.parts[:-1]):
            continue
        raw = path.read_text(encoding="utf-8", errors="replace")
        text = strip_comments_and_literals(raw)
        contexts = line_runtimes(text)
        occurrences, shapes = scan_scopes(text)
        for shape, count in shapes.items():
            unparsed[shape] = unparsed.get(shape, 0) + count
        for occurrence in occurrences:
            function = occurrence.function["name"] if occurrence.function else None
            class_path = occurrence.class_path
            single = occurrence.kind in ("REL::ID", "REL::ID declaration")
            if single:
                context = contexts[occurrence.line - 1] if occurrence.line - 1 < len(contexts) else ALL_RUNTIMES
                runtime = next(iter(context)) if len(context) == 1 else None
                if runtime is None:
                    if context == frozenset({"vr"}):
                        bump("REL::ID in a VR-only branch")
                    else:
                        bump("REL::ID with no single runtime")
                    continue
                if runtime == "vr":
                    bump("REL::ID in a VR-only branch")
                    continue
                values = [(runtime, _int_value(occurrence.args[0]))]
            else:
                if len(occurrence.args) < 2:
                    bump(occurrence.kind + " with fewer than two ids")
                    continue
                se_id = _int_value(occurrence.args[0])
                ae_id = _int_value(occurrence.args[1])
                values = [("se", se_id), ("ae", ae_id)]
            name = function
            if name is None and occurrence.kind.endswith(" declaration"):
                bump("namespace-scope id constant (not a function)")
                continue
            if name is None:
                declared = _declaration_name(text, occurrence.offset)
                if declared is None:
                    bump((occurrence.kind + " outside a function or declaration"))
                    continue
                shape, variable = declared
                if "table" in shape or "constant" in shape:
                    bump(shape)
                    continue
                name = class_path + "::" + variable if class_path else variable
            for runtime, identifier in values:
                if not identifier:
                    continue
                if runtime == "se":
                    add(se, identifier, name)
                elif runtime == "ae":
                    add(ae, identifier, name)
    return se, ae, unparsed, conflicts



# ---------------------------------------------------------------------------
# Source metadata and output
# ---------------------------------------------------------------------------


def source_metadata(
    checkout: Path,
    repo: str | None,
    commit: str | None,
    licence: str | None,
) -> dict[str, str]:
    if repo and commit and licence:
        return {"repo": repo, "commit": commit, "licence": licence}
    marker = checkout.with_name(checkout.name + ".commit")
    if not marker.is_file():
        missing = [name for name, value in (("repo", repo), ("commit", commit), ("licence", licence)) if not value]
        raise GenerationError(
            "source metadata is incomplete ("
            + ", ".join(missing)
            + f") and {marker} does not exist"
        )
    lines = [line.strip() for line in marker.read_text(encoding="utf-8").splitlines()]
    if not lines:
        raise GenerationError(f"{marker} is empty")
    first = lines[0].split()
    if len(first) < 2:
        raise GenerationError(
            f"{marker}: the first line must be '<repository-url> <commit>'"
        )
    found_repo, found_commit = first[0], first[1]
    found_licence = " ".join(line for line in lines[1:] if line)
    return {
        "repo": repo or found_repo,
        "commit": commit or found_commit,
        "licence": licence or found_licence or "unknown",
    }


def render_json(source: dict[str, str], se: dict[str, str], ae: dict[str, str]) -> str:
    payload = {
        "source": {"repo": source["repo"], "commit": source["commit"], "licence": source["licence"]},
        "se": se,
        "ae": ae,
    }
    return json.dumps(payload, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def render_js(source: dict[str, str], se: dict[str, str], ae: dict[str, str]) -> str:
    lines = [
        "// Generated by scripts/gen_addrlib_names.py. Do not edit by hand.",
        f"// Source: {source['repo']} at {source['commit']}",
        f"// Licence: {source['licence']}",
        "",
        "export const SE_NAMES = Object.freeze({",
    ]
    for key in sorted(se, key=int):
        lines.append("  " + json.dumps(key) + ": " + json.dumps(se[key], ensure_ascii=False) + ",")
    lines.append("});")
    lines.append("")
    lines.append("export const AE_NAMES = Object.freeze({")
    for key in sorted(ae, key=int):
        lines.append("  " + json.dumps(key) + ": " + json.dumps(ae[key], ensure_ascii=False) + ",")
    lines.append("});")
    lines.append("")
    return "\n".join(lines)


def write_if_changed(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_file() and path.read_text(encoding="utf-8") == content:
        return
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


def generate(
    checkout: Path,
    json_path: Path = DEFAULT_JSON,
    js_path: Path | None = None,
    repo: str | None = None,
    commit: str | None = None,
    licence: str | None = None,
) -> dict:
    if not checkout.is_dir():
        raise GenerationError(f"{checkout} is not a directory")
    source = source_metadata(checkout, repo, commit, licence)
    se, ae, unparsed, conflicts = extract(checkout)
    payload = render_json(source, se, ae)
    write_if_changed(json_path, payload)
    if js_path is not None:
        write_if_changed(js_path, render_js(source, se, ae))
    return {
        "source": source,
        "json": str(json_path),
        "js": str(js_path) if js_path else None,
        "se": len(se),
        "ae": len(ae),
        "conflicts": conflicts,
        "unparsed": dict(sorted(unparsed.items())),
    }


def format_report(result: dict) -> str:
    lines = [
        f"SE ids: {result['se']}",
        f"AE ids: {result['ae']}",
        f"conflicts: {result['conflicts']}",
        f"wrote {result['json']}",
    ]
    if result["js"]:
        lines.append(f"wrote {result['js']}")
    if result["unparsed"]:
        lines.append("source shapes not read:")
        for shape, count in result["unparsed"].items():
            lines.append(f"  {count:6d}  {shape}")
    else:
        lines.append("source shapes not read: none")
    return "\n".join(lines)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("checkout", type=Path, help="a CommonLibSSE-NG checkout")
    parser.add_argument("--out", type=Path, default=DEFAULT_JSON, help="JSON table to write")
    parser.add_argument(
        "--js",
        type=Path,
        default=None,
        help="also write an ES module exporting SE_NAMES and AE_NAMES",
    )
    parser.add_argument("--repo")
    parser.add_argument("--commit")
    parser.add_argument("--licence")
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = build_parser().parse_args(list(argv) if argv is not None else None)
    js_path = args.js
    if js_path is None and args.out == DEFAULT_JSON:
        js_path = DEFAULT_JS
    try:
        result = generate(
            args.checkout,
            json_path=args.out,
            js_path=js_path,
            repo=args.repo,
            commit=args.commit,
            licence=args.licence,
        )
    except GenerationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(format_report(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
