#!/usr/bin/env python3
"""Decide whether a commit should publish a Seam release.

The release version is ``0.1.<git rev-list --count HEAD>``. Skipping a publish
does not create a tag and does not reuse that count: the next commit's count is
strictly higher, so versions stay monotonic (gaps are expected).

Comparison is against the newest ``v<major>.<minor>.<patch>`` tag that is
reachable from HEAD (``git tag --merged HEAD``) and that has a GitHub Release.
Commit order wins over a higher version number on an older commit. No such tag,
or a release list that cannot be read, publishes.

A change publishes when it can affect the shipped app. Uncertainty publishes:
a parse failure, an unreadable git object, a submodule, a non-UTF-8 path, a
file-mode change of shippable content, or an include path that cannot be
resolved. Skips are limited to known places (``.github/``, ``docs/``, top-level
README and ``*.md``, ``crates/*/tests/``, ``crates/*/src/**/fixtures/``, and the
gate harness). A file pulled into shippable code with ``include_str!`` or
``include_bytes!`` is not skipped.
"""

from __future__ import annotations

import json
import os
import posixpath
import re
import subprocess
import sys
import tomllib
from typing import Iterable


class Uncertain(Exception):
    """The diff cannot be proved free of shippable changes."""


class Unreadable:
    """The path is in the tree, but its bytes cannot be shown to be unchanged."""

    def __init__(self, reason: str) -> None:
        self.reason = reason

    def __repr__(self) -> str:
        return f"Unreadable({self.reason})"


class TreeEntry:
    """One git tree entry: bytes plus the file mode (``100644``, ``100755``, ...)."""

    def __init__(self, data: "str | bytes | None | Unreadable", mode: str | None) -> None:
        self.data = data
        self.mode = mode


class Tok:
    def __init__(self, kind: str, text: str, start: int, end: int) -> None:
        self.kind = kind
        self.text = text
        self.start = start
        self.end = end

    def __repr__(self) -> str:
        return f"Tok({self.kind}:{self.text!r}@{self.start})"


_OPEN = {"(": ")", "[": "]", "{": "}"}
_CLOSE = {")": "(", "]": "[", "}": "{"}
_NON_SHIPPABLE_FILES = {
    "scripts/test.sh",
    "scripts/publish_gate.py",
    "scripts/test_publish_gate.py",
}
_RELEASE_TAG = re.compile(r"v\d+\.\d+\.\d+")


def norm_path(path: str) -> str:
    # lstrip() treats its argument as a character set, so it must not be used
    # on ".github": that would strip the leading dot and miss workflow files.
    p = path.replace("\\", "/").strip()
    while p.startswith("./"):
        p = p[2:]
    return p[1:] if p.startswith("/") else p


def is_non_shippable(path: str) -> bool:
    """Paths that cannot change the shipped app by themselves.

    ``tests/``, ``fixtures/``, ``readme*`` and ``*.md`` are skipped only in
    known places. The same path is shippable when production code includes it;
    that override is applied by ``should_publish``.
    """
    p = norm_path(path)
    parts = [part for part in p.split("/") if part]
    if not parts:
        return False
    if p == ".github" or p.startswith(".github/"):
        return True
    if p == "docs" or p.startswith("docs/"):
        return True
    if p in _NON_SHIPPABLE_FILES:
        return True
    # Top-level README / README.* / *.md only. Nested copies can ship.
    if len(parts) == 1:
        base = parts[0].lower()
        if base == "readme" or base.startswith("readme.") or base.endswith(".md"):
            return True
    # crates/<crate>/tests/** — integration tests, not the library crate root.
    if len(parts) >= 3 and parts[0] == "crates" and parts[2] == "tests":
        return True
    # crates/<crate>/src/**/fixtures/** — unit-test fixtures. An include from
    # shippable Rust overrides this in should_publish.
    if (
        len(parts) >= 4
        and parts[0] == "crates"
        and parts[2] == "src"
        and "fixtures" in parts[3:]
    ):
        return True
    return False


def is_cargo_toml(path: str) -> bool:
    p = norm_path(path)
    return p == "Cargo.toml" or p.endswith("/Cargo.toml")


def is_cargo_lock(path: str) -> bool:
    p = norm_path(path)
    return p == "Cargo.lock" or p.endswith("/Cargo.lock")


def is_cargo_path(path: str) -> bool:
    return is_cargo_toml(path) or is_cargo_lock(path)


def is_rust_path(path: str) -> bool:
    return norm_path(path).endswith(".rs")


def should_publish(
    before: dict[str, str | bytes | None],
    after: dict[str, str | bytes | None],
) -> bool:
    """Return True when the two trees differ in something that can ship.

    ``None`` means the path is absent. Parse failures and unclassified files
    publish (fail-open).
    """
    try:
        return _should_publish(before, after)
    except Uncertain as exc:
        print(f"publish gate uncertain ({exc}); publishing", file=sys.stderr)
        return True


def _split_entries(
    files: dict[str, str | bytes | None | TreeEntry | Unreadable],
) -> tuple[dict[str, str | bytes | None], dict[str, str | None]]:
    """Separate bytes from modes. An unreadable entry is uncertain immediately.

    Two failed reads used to compare equal (both ``None``) and were skipped.
    A path that is present but cannot be read must publish instead.
    """
    data: dict[str, str | bytes | None] = {}
    modes: dict[str, str | None] = {}
    for path, value in files.items():
        if isinstance(value, Unreadable):
            raise Uncertain(f"unreadable {path}: {value.reason}")
        if isinstance(value, TreeEntry):
            if isinstance(value.data, Unreadable):
                raise Uncertain(f"unreadable {path}: {value.data.reason}")
            data[path] = value.data
            modes[path] = value.mode
        else:
            data[path] = value
            modes[path] = None
    return data, modes


def _should_publish(
    before: dict[str, str | bytes | None | TreeEntry | Unreadable],
    after: dict[str, str | bytes | None | TreeEntry | Unreadable],
) -> bool:
    before_data, before_mode = _split_entries(before)
    after_data, after_mode = _split_entries(after)
    # Includes are collected from every Rust file we were given, not only the
    # paths that changed, so an unchanged include site still protects the file.
    forced = shippable_includes(before_data) | shippable_includes(after_data)
    saw_cargo = False
    for path in set(before_data) | set(after_data):
        if is_cargo_path(path):
            saw_cargo = True
            continue
        old = before_data.get(path)
        new = after_data.get(path)
        old_mode = before_mode.get(path)
        new_mode = after_mode.get(path)
        ships = path in forced or not is_non_shippable(path)
        # chmod of something that ships is a publish, even when the bytes match.
        if (
            ships
            and old_mode is not None
            and new_mode is not None
            and old_mode != new_mode
        ):
            return True
        if old == new:
            continue
        if not ships:
            continue
        if is_rust_path(path) and not isinstance(old, (bytes, bytearray)) and not isinstance(
            new, (bytes, bytearray)
        ):
            if shippable_rust(old) != shippable_rust(new):
                return True
            continue
        return True
    if saw_cargo and cargo_fingerprint(before_data) != cargo_fingerprint(after_data):
        return True
    return False


_INCLUDE_MACROS = {"include_str", "include_bytes"}


def shippable_includes(files: dict[str, str | bytes | None]) -> set[str]:
    """Paths named by ``include_str!`` / ``include_bytes!`` in shippable Rust.

    An include that is not a single string literal cannot be proved test-only,
    so it publishes. Includes inside ``#[cfg(test)]`` are already stripped.
    """
    found: set[str] = set()
    for path, content in files.items():
        # A Rust file in a known test/fixture location is not part of the
        # shipped crate, so its includes (and a parse failure there) do not
        # publish. Unknown locations still fail open.
        if not is_rust_path(path) or content is None or is_non_shippable(path):
            continue
        if isinstance(content, (bytes, bytearray)):
            raise Uncertain(f"non-text rust {path}")
        if not isinstance(content, str):
            raise Uncertain(f"unexpected rust content for {path}")
        found.update(_includes_in(path, content))
    return found


def _includes_in(path: str, content: str) -> set[str]:
    tokens = lex(shippable_rust(content))
    found: set[str] = set()
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token.kind != "ident" or token.text not in _INCLUDE_MACROS:
            index += 1
            continue
        span = _macro_args(tokens, index)
        if span is None:
            index += 1
            continue
        args, index = span
        if len(args) != 1 or args[0].kind != "string":
            raise Uncertain(f"unresolved {token.text}! in {path}")
        found.add(_resolve_include(path, _decode_include_literal(args[0].text)))
    return found


def _macro_args(tokens: list[Tok], index: int) -> tuple[list[Tok], int] | None:
    if (
        index + 2 >= len(tokens)
        or tokens[index + 1].text != "!"
        or tokens[index + 2].text != "("
    ):
        return None
    depth = 0
    cursor = index + 2
    start = index + 3
    while cursor < len(tokens):
        text = tokens[cursor].text
        if text in _OPEN:
            depth += 1
        elif text in _CLOSE:
            depth -= 1
            if depth == 0:
                return tokens[start:cursor], cursor + 1
        cursor += 1
    raise Uncertain("unclosed include macro")


def _decode_include_literal(text: str) -> str:
    body = text
    if body.startswith(("br", "cr")):
        return _raw_string_body(body[2:])
    if body.startswith("r"):
        return _raw_string_body(body[1:])
    if body.startswith(("b", "c")):
        body = body[1:]
    if len(body) < 2 or not body.startswith('"') or not body.endswith('"'):
        raise Uncertain(f"bad include literal {text!r}")
    return _unescape_string(body[1:-1])


def _raw_string_body(body: str) -> str:
    hashes = 0
    while hashes < len(body) and body[hashes] == "#":
        hashes += 1
    opener = ("#" * hashes) + '"'
    closer = '"' + ("#" * hashes)
    if (
        len(body) < len(opener) + len(closer)
        or not body.startswith(opener)
        or not body.endswith(closer)
    ):
        raise Uncertain(f"bad raw include literal {body!r}")
    return body[len(opener) : len(body) - len(closer)]


def _unescape_string(body: str) -> str:
    out: list[str] = []
    index = 0
    while index < len(body):
        char = body[index]
        if char != "\\":
            out.append(char)
            index += 1
            continue
        if index + 1 >= len(body):
            raise Uncertain("truncated escape in include path")
        nxt = body[index + 1]
        simple = {"n": "\n", "r": "\r", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0"}
        if nxt in simple:
            out.append(simple[nxt])
            index += 2
            continue
        if nxt == "\n":
            index += 2
            continue
        if nxt == "u" and index + 2 < len(body) and body[index + 2] == "{":
            end = body.find("}", index + 3)
            if end < 0:
                raise Uncertain("unclosed unicode escape in include path")
            try:
                out.append(chr(int(body[index + 3 : end], 16)))
            except ValueError as exc:
                raise Uncertain("bad unicode escape in include path") from exc
            index = end + 1
            continue
        if nxt == "x" and index + 3 < len(body):
            try:
                out.append(chr(int(body[index + 2 : index + 4], 16)))
            except ValueError as exc:
                raise Uncertain("bad hex escape in include path") from exc
            index += 4
            continue
        raise Uncertain(f"bad escape in include path: \\{nxt}")
    return "".join(out)


def _resolve_include(rs_path: str, literal: str) -> str:
    if not literal or "\n" in literal or "\r" in literal or "\x00" in literal:
        raise Uncertain(f"bad include path {literal!r} from {rs_path}")
    if literal.startswith("/") or (len(literal) > 2 and literal[1] == ":"):
        raise Uncertain(f"absolute include {literal!r} from {rs_path}")
    base = posixpath.dirname(norm_path(rs_path))
    joined = posixpath.normpath(posixpath.join(base, literal.replace("\\", "/")))
    if joined == ".." or joined.startswith("../") or joined.startswith("/"):
        raise Uncertain(f"include escapes the repo: {literal!r} from {rs_path}")
    return joined


def shippable_rust(src: str | None) -> str:
    """Source with definitely-test-only items removed.

    ``#[cfg(test)]``, ``#[cfg(all(test, ...))]`` (test is required), ``#[test]``,
    and attributes whose last path segment is ``test`` (``#[tokio::test]``) are
    removed, including nested in ``mod`` / ``impl`` / ``trait`` / ``extern``
    blocks. ``#[cfg(any(test, other))]`` and ``#[cfg_attr(test, ...)]`` are kept.
    A file that cannot be parsed raises ``Uncertain``.
    """
    if not src:
        return ""
    if src.startswith("\ufeff"):
        src = src[1:]
    tokens = lex(src)
    deletes: list[tuple[int, int]] = []
    parse_body(tokens, 0, len(tokens), deletes)
    return apply_deletes(src, deletes)


def apply_deletes(src: str, ranges: list[tuple[int, int]]) -> str:
    if not ranges:
        return src
    merged: list[list[int]] = []
    for start, end in sorted(ranges):
        if end <= start:
            continue
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    parts: list[str] = []
    pos = 0
    for start, end in merged:
        parts.append(src[pos:start])
        pos = min(len(src), end)
    parts.append(src[pos:])
    return "".join(parts)


def lex(src: str) -> list[Tok]:
    tokens: list[Tok] = []
    i = 0
    n = len(src)
    while i < n:
        ch = src[i]
        if ch.isspace():
            i += 1
            continue
        if ch == "/" and i + 1 < n and src[i + 1] == "/":
            i += 2
            while i < n and src[i] != "\n":
                i += 1
            continue
        if ch == "/" and i + 1 < n and src[i + 1] == "*":
            i = _scan_block_comment(src, i)
            continue
        prefixed = _scan_prefixed_literal(src, i)
        if prefixed is not None:
            kind, end = prefixed
            tokens.append(Tok(kind, src[i:end], i, end))
            i = end
            continue
        if ch == '"':
            end = _scan_string(src, i)
            tokens.append(Tok("string", src[i:end], i, end))
            i = end
            continue
        if ch == "'":
            end = _try_char(src, i)
            if end is not None:
                tokens.append(Tok("char", src[i:end], i, end))
                i = end
                continue
            end = _scan_lifetime(src, i)
            tokens.append(Tok("lifetime", src[i:end], i, end))
            i = end
            continue
        if ch.isalpha() or ch == "_":
            end = _scan_ident(src, i)
            tokens.append(Tok("ident", src[i:end], i, end))
            i = end
            continue
        if ch.isdigit():
            end = _scan_number(src, i)
            tokens.append(Tok("number", src[i:end], i, end))
            i = end
            continue
        if ch == ":" and i + 1 < n and src[i + 1] == ":":
            tokens.append(Tok("punct", "::", i, i + 2))
            i += 2
            continue
        two = src[i : i + 2]
        if two in {"->", "=>", ">=", "<=", "==", "!=", "&&", "||", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<", ">>", ".."}:
            tokens.append(Tok("punct", two, i, i + 2))
            i += 2
            continue
        if src[i : i + 3] == "...":
            tokens.append(Tok("punct", "...", i, i + 3))
            i += 3
            continue
        tokens.append(Tok("punct", ch, i, i + 1))
        i += 1
    return tokens


def _line_col(src: str, index: int) -> str:
    line = src.count("\n", 0, index) + 1
    col = index - src.rfind("\n", 0, index)
    return f"line {line}:{col}"


def _scan_block_comment(src: str, i: int) -> int:
    depth = 1
    i += 2
    n = len(src)
    while i < n and depth:
        if src.startswith("/*", i):
            depth += 1
            i += 2
            continue
        if src.startswith("*/", i):
            depth -= 1
            i += 2
            continue
        i += 1
    if depth:
        raise Uncertain(f"unclosed block comment at {_line_col(src, i)}")
    return i


def _scan_escape(src: str, i: int) -> int:
    if i + 1 >= len(src):
        raise Uncertain(f"truncated escape at {_line_col(src, i)}")
    if src[i + 1] == "u" and i + 2 < len(src) and src[i + 2] == "{":
        end = src.find("}", i + 3)
        if end < 0:
            raise Uncertain(f"unclosed unicode escape at {_line_col(src, i)}")
        return end + 1
    if src[i + 1] == "x":
        return i + 4
    return i + 2


def _scan_string(src: str, i: int) -> int:
    i += 1
    n = len(src)
    while i < n:
        if src[i] == "\\":
            i = _scan_escape(src, i)
            continue
        if src[i] == '"':
            return i + 1
        # Rust string literals may span lines. A backslash-newline is an
        # escape and is consumed above; a raw newline is part of the literal.
        i += 1
    raise Uncertain("unclosed string")


def _scan_raw_string(src: str, i: int) -> int | None:
    hashes = 0
    while i < len(src) and src[i] == "#":
        hashes += 1
        i += 1
    if i >= len(src) or src[i] != '"':
        return None
    closer = '"' + ("#" * hashes)
    i += 1
    found = src.find(closer, i)
    if found < 0:
        raise Uncertain(f"unclosed raw string at {_line_col(src, i)}")
    return found + len(closer)


def _scan_prefixed_literal(src: str, i: int) -> tuple[str, int] | None:
    rest = src[i:]
    for prefix in ("br", "cr", "b", "c", "r"):
        if not rest.startswith(prefix):
            continue
        j = i + len(prefix)
        if j >= len(src):
            return None
        nxt = src[j]
        if prefix == "b" and nxt == "'":
            end = _try_char(src, j)
            if end is None:
                raise Uncertain(f"bad byte char at {_line_col(src, i)}")
            return "char", end
        if prefix == "r" and nxt == "#" and j + 1 < len(src) and (src[j + 1].isalpha() or src[j + 1] == "_"):
            return "ident", _scan_ident(src, j + 1)
        # r / br / cr strings are raw even with zero hashes. b" and c" are not.
        if prefix in {"r", "br", "cr"} and nxt in {'"', '#'}:
            end = _scan_raw_string(src, j)
            if end is None:
                raise Uncertain(f"bad raw string at {_line_col(src, i)}")
            return "string", end
        if prefix in {"b", "c"} and nxt == '"':
            return "string", _scan_string(src, j)
        return None
    return None


def _try_char(src: str, i: int) -> int | None:
    if i >= len(src) or src[i] != "'":
        return None
    j = i + 1
    if j >= len(src):
        return None
    if src[j] == "\\":
        j = _scan_escape(src, j)
    else:
        j += 1
    if j < len(src) and src[j] == "'":
        return j + 1
    return None


def _scan_lifetime(src: str, i: int) -> int:
    j = i + 1
    if j >= len(src) or not (src[j].isalpha() or src[j] == "_"):
        raise Uncertain(f"bad lifetime or char at {_line_col(src, i)}")
    j += 1
    while j < len(src) and (src[j].isalnum() or src[j] == "_"):
        j += 1
    return j


def _scan_ident(src: str, i: int) -> int:
    j = i + 1
    while j < len(src) and (src[j].isalnum() or src[j] == "_"):
        j += 1
    return j


def _scan_number(src: str, i: int) -> int:
    j = i
    n = len(src)
    if src.startswith(("0x", "0X", "0b", "0B", "0o", "0O"), i):
        j = i + 2
        while j < n and (src[j].isalnum() or src[j] == "_"):
            j += 1
        return j
    while j < n and (src[j].isdigit() or src[j] == "_"):
        j += 1
    if j < n and src[j] == "." and j + 1 < n and src[j + 1].isdigit():
        j += 1
        while j < n and (src[j].isdigit() or src[j] == "_"):
            j += 1
    if j < n and src[j] in "eE":
        k = j + 1
        if k < n and src[k] in "+-":
            k += 1
        if k < n and src[k].isdigit():
            j = k + 1
            while j < n and (src[j].isdigit() or src[j] == "_"):
                j += 1
    if j < n and (src[j].isalpha() or src[j] == "_"):
        j = _scan_ident(src, j)
    return j


def parse_body(tokens: list[Tok], start: int, end: int, deletes: list[tuple[int, int]]) -> None:
    i = start
    if i < end and _is_inner_attr(tokens, i):
        attr_at = i
        test_body = False
        while i < end and _is_inner_attr(tokens, i):
            content, i = _attribute_content(tokens, i, end)
            if _attribute_requires_test(content):
                test_body = True
        if test_body:
            prev = 0 if attr_at == 0 else tokens[attr_at - 1].end
            deletes.append((prev, tokens[end - 1].end))
            return
    while i < end:
        nxt = _parse_item(tokens, i, end, deletes)
        if nxt <= i:
            raise Uncertain(f"parser stuck at {tokens[i]}")
        i = nxt
    if i != end:
        raise Uncertain(f"unparsed tokens at {tokens[i]}")


def _parse_item(tokens: list[Tok], i: int, end: int, deletes: list[tuple[int, int]]) -> int:
    if i >= end:
        raise Uncertain("expected an item")
    attr_at = i
    while i < end and _is_outer_attr(tokens, i):
        _, i = _attribute_content(tokens, i, end)
    if i >= end:
        raise Uncertain("attribute without an item")
    test_only = False
    cursor = attr_at
    while cursor < i:
        content, cursor = _attribute_content(tokens, cursor, end)
        if _attribute_requires_test(content):
            test_only = True
    kw, kind = _classify(tokens, i, end)
    item_end, body, recurse = _finish_item(tokens, kw, kind, end)
    if test_only:
        prev = 0 if attr_at == 0 else tokens[attr_at - 1].end
        deletes.append((prev, tokens[item_end - 1].end))
        return item_end
    if recurse and body is not None:
        parse_body(tokens, body[0], body[1], deletes)
    return item_end


def _is_outer_attr(tokens: list[Tok], i: int) -> bool:
    return (
        i + 1 < len(tokens)
        and tokens[i].text == "#"
        and tokens[i + 1].text == "["
    )


def _is_inner_attr(tokens: list[Tok], i: int) -> bool:
    return (
        i + 2 < len(tokens)
        and tokens[i].text == "#"
        and tokens[i + 1].text == "!"
        and tokens[i + 2].text == "["
    )


def _attribute_content(tokens: list[Tok], i: int, end: int) -> tuple[list[Tok], int]:
    if tokens[i].text != "#":
        raise Uncertain("expected attribute")
    bracket = i + 2 if i + 1 < end and tokens[i + 1].text == "!" else i + 1
    if bracket >= end or tokens[bracket].text != "[":
        raise Uncertain("malformed attribute")
    close = _skip_group(tokens, bracket, end)
    return tokens[bracket + 1 : close - 1], close


def _attribute_requires_test(content: list[Tok]) -> bool:
    try:
        parts, idx = _parse_path(content, 0)
    except Uncertain:
        return False
    if not parts:
        return False
    if parts == ["cfg"] and idx < len(content) and content[idx].text == "(":
        try:
            close = _match_paren(content, idx)
        except Uncertain:
            return False
        return _eval_cfg(content[idx + 1 : close]) is False
    # #[test], #[tokio::test], #[tokio::test(...)]. cfg() is handled above.
    return parts[-1] == "test"


def _parse_path(tokens: list[Tok], i: int) -> tuple[list[str], int]:
    if i >= len(tokens) or tokens[i].kind != "ident":
        return [], i
    parts = [tokens[i].text]
    i += 1
    while i + 1 < len(tokens) and tokens[i].text == "::" and tokens[i + 1].kind == "ident":
        parts.append(tokens[i + 1].text)
        i += 2
    return parts, i


def _match_paren(tokens: list[Tok], i: int) -> int:
    depth = 0
    for j in range(i, len(tokens)):
        if tokens[j].text == "(":
            depth += 1
        elif tokens[j].text == ")":
            depth -= 1
            if depth == 0:
                return j
    raise Uncertain("unbalanced cfg predicate")


def _eval_cfg(tokens: list[Tok]) -> bool | None:
    """Kleene value of a cfg predicate with ``test`` forced off.

    ``False`` means the predicate is off in a non-test build, so the item is
    not shipped. ``None`` means it might still be compiled; keep the item.
    """
    if not tokens:
        return None
    try:
        value, idx = _parse_pred(tokens, 0)
    except Uncertain:
        return None
    if idx != len(tokens):
        return None
    return value


def _parse_pred(tokens: list[Tok], i: int) -> tuple[bool | None, int]:
    if i >= len(tokens) or tokens[i].kind != "ident":
        return None, min(len(tokens), i + 1)
    name = tokens[i].text
    if i + 1 < len(tokens) and tokens[i + 1].text == "(":
        close = _match_paren(tokens, i + 1)
        args = [arg for arg in _split_args(tokens[i + 2 : close]) if arg]
        values = [_eval_cfg(arg) for arg in args]
        nxt = close + 1
        if name == "all":
            return _kleene_and(values), nxt
        if name == "any":
            return _kleene_or(values), nxt
        if name == "not":
            if len(values) != 1:
                return None, nxt
            return _kleene_not(values[0]), nxt
        return None, nxt
    if i + 1 < len(tokens) and tokens[i + 1].text == "=":
        nxt = i + 2
        if nxt < len(tokens):
            nxt += 1
        return None, nxt
    if name == "test":
        return False, i + 1
    if name == "true":
        return True, i + 1
    if name == "false":
        return False, i + 1
    return None, i + 1


def _split_args(tokens: list[Tok]) -> list[list[Tok]]:
    args: list[list[Tok]] = []
    current: list[Tok] = []
    depth = 0
    for tok in tokens:
        if tok.text in _OPEN:
            depth += 1
            current.append(tok)
        elif tok.text in _CLOSE:
            depth -= 1
            current.append(tok)
        elif tok.text == "," and depth == 0:
            args.append(current)
            current = []
        else:
            current.append(tok)
    if current:
        args.append(current)
    return args


def _kleene_and(values: list[bool | None]) -> bool | None:
    if not values:
        return True
    if any(value is False for value in values):
        return False
    if any(value is None for value in values):
        return None
    return True


def _kleene_or(values: list[bool | None]) -> bool | None:
    if not values:
        return False
    if any(value is True for value in values):
        return True
    if any(value is None for value in values):
        return None
    return False


def _kleene_not(value: bool | None) -> bool | None:
    if value is None:
        return None
    return not value


def _skip_group(tokens: list[Tok], i: int, end: int) -> int:
    if i >= end or tokens[i].text not in _OPEN:
        raise Uncertain(f"expected group at {tokens[i] if i < len(tokens) else 'eof'}")
    stack = [_OPEN[tokens[i].text]]
    i += 1
    while i < end and stack:
        text = tokens[i].text
        if text in _OPEN:
            stack.append(_OPEN[text])
        elif text in _CLOSE:
            if text != stack[-1]:
                raise Uncertain(f"mismatched delimiter {text}")
            stack.pop()
        i += 1
    if stack:
        raise Uncertain("unclosed delimiter")
    return i


def _classify(tokens: list[Tok], i: int, end: int) -> tuple[int, str]:
    while i < end:
        text = tokens[i].text
        nxt = tokens[i + 1].text if i + 1 < end else ""
        if text == "pub":
            i += 1
            if i < end and tokens[i].text == "(":
                i = _skip_group(tokens, i, end)
            continue
        if text == "default":
            i += 1
            continue
        if text == "const" and nxt in {"fn", "async", "unsafe"}:
            i += 1
            continue
        if text == "async" and nxt in {"fn", "unsafe", "extern"}:
            i += 1
            continue
        if text == "unsafe" and nxt in {"fn", "trait", "impl", "extern", "async", "mod", "const"}:
            i += 1
            continue
        if text == "extern":
            if nxt == "crate":
                return i, "extern_crate"
            if nxt == "fn":
                i += 1
                continue
            j = i + 1
            if j < end and tokens[j].kind in {"string", "char"}:
                j += 1
            if j < end and tokens[j].text == "fn":
                return j, "fn"
            return i, "extern_block"
        if text == "const":
            return i, "const"
        if tokens[i].kind == "ident" or text in _OPEN:
            return i, text
        raise Uncertain(f"unexpected token {text}")
    raise Uncertain("item has no keyword")


def _finish_item(
    tokens: list[Tok], kw: int, kind: str, end: int
) -> tuple[int, tuple[int, int] | None, bool]:
    if kind == "extern_crate":
        return _consume_semi(tokens, kw, end), None, False
    if kind == "extern_block":
        item_end, body = _consume_brace_or_semi(tokens, kw + 1, end)
        if body is None:
            raise Uncertain("extern block has no body")
        return item_end, body, True
    if kind in {"fn"}:
        item_end, _body = _consume_brace_or_semi(tokens, kw + 1, end)
        return item_end, None, False
    if kind in {"mod", "impl", "trait"}:
        item_end, body = _consume_brace_or_semi(tokens, kw + 1, end)
        if kind != "mod" and body is None:
            raise Uncertain(f"{kind} has no body")
        return item_end, body, body is not None
    if kind in {"struct", "enum", "union"}:
        item_end, _body = _consume_brace_or_semi(tokens, kw + 1, end)
        return item_end, None, False
    if kind in {"use", "type", "const", "static"}:
        return _consume_semi(tokens, kw, end), None, False
    if kind == "macro_rules":
        return _consume_macro_rules(tokens, kw, end), None, False
    return _consume_macro(tokens, kw, end), None, False


def _consume_brace_or_semi(
    tokens: list[Tok], i: int, end: int
) -> tuple[int, tuple[int, int] | None]:
    while i < end:
        text = tokens[i].text
        if text in {"(", "["}:
            i = _skip_group(tokens, i, end)
            continue
        if text == "{":
            item_end = _skip_group(tokens, i, end)
            return item_end, (i + 1, item_end - 1)
        if text == ";":
            return i + 1, None
        i += 1
    raise Uncertain("item did not end")


def _consume_semi(tokens: list[Tok], i: int, end: int) -> int:
    while i < end:
        if tokens[i].text in _OPEN:
            i = _skip_group(tokens, i, end)
            continue
        if tokens[i].text == ";":
            return i + 1
        i += 1
    raise Uncertain("missing semicolon")


def _consume_macro(tokens: list[Tok], i: int, end: int) -> int:
    j = i
    if j >= end or tokens[j].kind != "ident":
        raise Uncertain(f"unknown item {tokens[i].text if i < len(tokens) else ''}")
    while j + 2 < end and tokens[j + 1].text == "::" and tokens[j + 2].kind == "ident":
        j += 2
    if j + 1 >= end or tokens[j + 1].text != "!":
        raise Uncertain(f"unknown item starting at {tokens[i].text}")
    j += 2
    if j >= end or tokens[j].text not in _OPEN:
        raise Uncertain("macro is missing a delimiter")
    j = _skip_group(tokens, j, end)
    if j < end and tokens[j].text == ";":
        j += 1
    return j


def _consume_macro_rules(tokens: list[Tok], i: int, end: int) -> int:
    if i + 1 >= end or tokens[i + 1].text != "!":
        raise Uncertain("malformed macro_rules")
    j = i + 2
    if j >= end or tokens[j].kind != "ident":
        raise Uncertain("macro_rules is missing a name")
    j += 1
    if j >= end or tokens[j].text not in _OPEN:
        raise Uncertain("macro_rules is missing a body")
    j = _skip_group(tokens, j, end)
    if j < end and tokens[j].text == ";":
        j += 1
    return j


def cargo_fingerprint(files: dict[str, str | bytes | None]) -> str:
    manifests = []
    for path in sorted(p for p in files if is_cargo_toml(p)):
        manifests.append([norm_path(path), _manifest_fp(files.get(path))])
    return json.dumps(
        {"manifests": manifests, "lock": lock_runtime_fingerprint(files)},
        sort_keys=True,
        separators=(",", ":"),
    )


def _manifest_fp(text: str | bytes | None) -> object:
    if text is None:
        return None
    if isinstance(text, (bytes, bytearray)):
        raise Uncertain("Cargo.toml is not utf-8")
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        raise Uncertain(f"Cargo.toml parse: {exc}") from exc
    return _drop_dev(data)


def _drop_dev(value: object) -> object:
    if isinstance(value, dict):
        return {
            key: _drop_dev(inner)
            for key, inner in value.items()
            if key != "dev-dependencies"
        }
    if isinstance(value, list):
        return [_drop_dev(inner) for inner in value]
    return value


def lock_runtime_fingerprint(files: dict[str, str | bytes | None]) -> list:
    """Third-party packages reachable through normal or build dependencies.

    Dev-dependencies of workspace members are not walked. A requirement that
    cannot be matched includes every locked version of that crate.
    """
    lock_paths = [p for p in files if is_cargo_lock(p)]
    if not lock_paths:
        return []
    if len(lock_paths) != 1:
        raise Uncertain("multiple Cargo.lock files")
    text = files.get(lock_paths[0])
    if text is None:
        return []
    if isinstance(text, (bytes, bytearray)):
        raise Uncertain("Cargo.lock is not utf-8")
    try:
        lock = tomllib.loads(text)
    except tomllib.TOMLDecodeError as exc:
        raise Uncertain(f"Cargo.lock parse: {exc}") from exc
    packages = lock.get("package")
    if not isinstance(packages, list):
        raise Uncertain("Cargo.lock has no package list")
    manifests = _load_manifests(files)
    if not manifests and text:
        raise Uncertain("Cargo.lock without a readable Cargo.toml")
    by_name: dict[str, list[dict]] = {}
    for pkg in packages:
        if isinstance(pkg, dict) and "name" in pkg:
            by_name.setdefault(str(pkg["name"]), []).append(pkg)
    our = {
        str(data["package"]["name"]): data
        for data in manifests.values()
        if isinstance(data.get("package"), dict) and "name" in data["package"]
    }
    workspace = _workspace_manifest(manifests)
    seen: set[tuple[str, str, str]] = set()
    rows: list[tuple[str, str, str, str]] = []
    queue = list(our)

    def add_third(pkg: dict) -> None:
        pid = (str(pkg["name"]), str(pkg["version"]), str(pkg.get("source", "")))
        if pid in seen:
            return
        seen.add(pid)
        if _is_local(pkg, our):
            queue.append(str(pkg["name"]))
            return
        rows.append((pid[0], pid[1], pid[2], str(pkg.get("checksum", ""))))
        for dep in pkg.get("dependencies") or []:
            if not isinstance(dep, str):
                raise Uncertain("unexpected Cargo.lock dependency")
            for target in _resolve_lock_dep(dep, by_name):
                add_third(target)

    seen_our: set[str] = set()
    while queue:
        name = queue.pop()
        if name in seen_our:
            continue
        seen_our.add(name)
        manifest = our.get(name)
        if manifest is None:
            for pkg in by_name.get(name, []):
                add_third(pkg)
            continue
        for dep_name, kind in _normal_reqs(manifest, workspace):
            if kind[0] == "path":
                queue.append(dep_name)
                continue
            chosen = _select_packages(by_name, dep_name, kind)
            if not chosen and dep_name not in by_name:
                # No locked crate of this name. The manifest fingerprint still
                # records the requirement itself.
                continue
            for pkg in chosen:
                if _is_local(pkg, our):
                    queue.append(str(pkg["name"]))
                else:
                    add_third(pkg)
    rows.sort()
    return rows


def _load_manifests(files: dict[str, str | bytes | None]) -> dict[str, dict]:
    loaded = {}
    for path, text in files.items():
        if not is_cargo_toml(path) or not isinstance(text, str):
            if is_cargo_toml(path) and isinstance(text, (bytes, bytearray)):
                raise Uncertain(f"{path} is not utf-8")
            continue
        try:
            loaded[norm_path(path)] = tomllib.loads(text)
        except tomllib.TOMLDecodeError as exc:
            raise Uncertain(f"{path} parse: {exc}") from exc
    return loaded


def _workspace_manifest(manifests: dict[str, dict]) -> dict | None:
    candidates = [(path, data) for path, data in manifests.items() if "workspace" in data]
    if not candidates:
        return None
    candidates.sort(key=lambda item: (item[0].count("/"), len(item[0])))
    return candidates[0][1]


def _is_local(pkg: dict, our: dict[str, dict]) -> bool:
    return "source" not in pkg and str(pkg.get("name")) in our


def _normal_reqs(manifest: dict, workspace: dict | None) -> Iterable[tuple[str, tuple]]:
    tables = []
    for key in ("dependencies", "build-dependencies"):
        table = manifest.get(key)
        if isinstance(table, dict):
            tables.append(table)
    target = manifest.get("target")
    if isinstance(target, dict):
        for cfg in target.values():
            if not isinstance(cfg, dict):
                continue
            for key in ("dependencies", "build-dependencies"):
                table = cfg.get(key)
                if isinstance(table, dict):
                    tables.append(table)
    for table in tables:
        for key, spec in table.items():
            yield _normalize_req(str(key), spec, workspace)


def _normalize_req(key: str, spec: object, workspace: dict | None) -> tuple[str, tuple]:
    try:
        if isinstance(spec, dict) and spec.get("workspace") is True:
            inherited = _workspace_dep(workspace, key)
            if inherited is None:
                return key, ("all",)
            spec = inherited
        name = key
        if isinstance(spec, dict) and isinstance(spec.get("package"), str):
            name = spec["package"]
        if isinstance(spec, str):
            return name, ("req", spec)
        if isinstance(spec, dict):
            if "git" in spec:
                version = spec.get("version")
                return name, ("git", version if isinstance(version, str) else None)
            if "path" in spec and "version" not in spec:
                return name, ("path", str(spec.get("path", "")))
            if "version" in spec:
                return name, ("req", str(spec["version"]))
            if "path" in spec:
                return name, ("path", str(spec.get("path", "")))
        return name, ("all",)
    except Exception:
        return key, ("all",)


def _workspace_dep(workspace: dict | None, key: str) -> object | None:
    if not isinstance(workspace, dict):
        return None
    block = workspace.get("workspace")
    deps = None
    if isinstance(block, dict):
        deps = block.get("dependencies")
    if not isinstance(deps, dict):
        deps = workspace.get("dependencies") if isinstance(workspace.get("dependencies"), dict) else None
    if isinstance(deps, dict) and key in deps:
        return deps[key]
    return None


def _select_packages(by_name: dict[str, list[dict]], name: str, kind: tuple) -> list[dict]:
    cands = list(by_name.get(name, []))
    if not cands:
        return []
    if kind[0] == "all":
        return cands
    if kind[0] == "path":
        local = [pkg for pkg in cands if "source" not in pkg]
        return local or cands
    if kind[0] == "git":
        git_pkgs = [pkg for pkg in cands if str(pkg.get("source", "")).startswith("git+")]
        version = kind[1]
        if isinstance(version, str):
            matched = [pkg for pkg in git_pkgs if version_matches(str(pkg["version"]), version)]
            git_pkgs = matched or git_pkgs
        return git_pkgs or cands
    if kind[0] == "req" and isinstance(kind[1], str):
        matched = [pkg for pkg in cands if version_matches(str(pkg["version"]), kind[1])]
        return matched or cands
    return cands


def _resolve_lock_dep(spec: str, by_name: dict[str, list[dict]]) -> list[dict]:
    parts = spec.split()
    if not parts:
        return []
    name = parts[0]
    cands = list(by_name.get(name, []))
    if len(parts) >= 2:
        version = parts[1]
        matched = [pkg for pkg in cands if str(pkg.get("version")) == version]
        cands = matched or cands
    if len(parts) >= 3 and parts[2].startswith("(") and parts[2].endswith(")"):
        source = parts[2][1:-1]
        matched = [pkg for pkg in cands if pkg.get("source") == source]
        cands = matched or list(by_name.get(name, []))
    return cands


def version_matches(version: str, req: str) -> bool:
    try:
        return _version_matches(version, req)
    except Exception:
        return False


def _version_matches(version: str, req: str) -> bool:
    req = req.strip()
    if req in {"*", ""}:
        return True
    comparators = [part.strip() for part in req.split(",") if part.strip()]
    if not comparators:
        return False
    # Cargo ignores pre-releases unless a comparator names one.
    if "-" in version.split("+", 1)[0] and not any("-" in comp for comp in comparators):
        return False
    return all(_match_comparator(version, comp) for comp in comparators)


def _match_comparator(version: str, comp: str) -> bool:
    for op in (">=", "<=", "=", "~", "^", ">", "<"):
        if comp.startswith(op):
            raw = comp[len(op) :].strip()
            return _apply_op(op, version, raw)
    return _apply_op("^", version, comp)


def _apply_op(op: str, version: str, raw: str) -> bool:
    left = _parse_version(version)
    right = _parse_version(raw)
    if op == "=":
        return _cmp_version(left, right) == 0
    if op == ">":
        return _cmp_version(left, right) > 0
    if op == ">=":
        return _cmp_version(left, right) >= 0
    if op == "<":
        return _cmp_version(left, right) < 0
    if op == "<=":
        return _cmp_version(left, right) <= 0
    if op == "^":
        upper = _caret_upper(right[0])
        return _cmp_version(left, right) >= 0 and _cmp_numbers(left[0], upper) < 0
    if op == "~":
        upper = _tilde_upper(right[0])
        return _cmp_version(left, right) >= 0 and _cmp_numbers(left[0], upper) < 0
    return False


def _parse_version(text: str) -> tuple[tuple[int, ...], tuple[str, ...]]:
    text = text.split("+", 1)[0].strip()
    if not text or text == "*":
        raise ValueError(text)
    pre: tuple[str, ...] = ()
    if "-" in text:
        text, pre_text = text.split("-", 1)
        pre = tuple(pre_text.split("."))
    nums = tuple(int(part) for part in text.split("."))
    if not nums:
        raise ValueError(text)
    return nums, pre


def _cmp_numbers(left: tuple[int, ...], right: tuple[int, ...]) -> int:
    width = max(len(left), len(right), 1)
    a = left + (0,) * (width - len(left))
    b = right + (0,) * (width - len(right))
    if a < b:
        return -1
    if a > b:
        return 1
    return 0


def _cmp_version(
    left: tuple[tuple[int, ...], tuple[str, ...]],
    right: tuple[tuple[int, ...], tuple[str, ...]],
) -> int:
    ordered = _cmp_numbers(left[0], right[0])
    if ordered:
        return ordered
    if left[1] == right[1]:
        return 0
    if not left[1]:
        return 1
    if not right[1]:
        return -1
    for a, b in zip(left[1], right[1]):
        if a == b:
            continue
        a_num, b_num = a.isdigit(), b.isdigit()
        if a_num and b_num:
            return -1 if int(a) < int(b) else 1
        if a_num:
            return -1
        if b_num:
            return 1
        return -1 if a < b else 1
    return -1 if len(left[1]) < len(right[1]) else 1


def _caret_upper(nums: tuple[int, ...]) -> tuple[int, ...]:
    padded = nums + (0, 0, 0)
    major, minor, patch = padded[0], padded[1], padded[2]
    if major > 0:
        return (major + 1, 0, 0)
    if minor > 0:
        return (0, minor + 1, 0)
    return (0, 0, patch + 1)


def _tilde_upper(nums: tuple[int, ...]) -> tuple[int, ...]:
    if len(nums) <= 1:
        return (nums[0] + 1, 0, 0)
    return (nums[0], nums[1] + 1, 0)


def decide_git(
    head: str = "HEAD",
    cwd: str | None = None,
    releases: Iterable[str] | None = None,
) -> bool:
    """Compare ``head`` with the newest merged release tag. No tag publishes.

    ``releases`` is the set of tags that have a GitHub Release. ``None`` asks
    GitHub. A lookup failure publishes (fail-open).
    """
    try:
        tag = latest_release_tag(cwd, head=head, releases=releases)
        if tag is None:
            return True
        if _git(["rev-parse", "--verify", "--quiet", tag + "^{commit}"], cwd).returncode != 0:
            raise Uncertain(f"tag {tag} does not resolve")
        paths = set(changed_paths(tag, head, cwd))
        paths.update(_tree_paths(tag, cwd, is_cargo_path))
        paths.update(_tree_paths(head, cwd, is_cargo_path))
        # Every Rust file, changed or not, so include_str!/include_bytes! in
        # shippable code is visible when only the included file changed.
        paths.update(_tree_paths(tag, cwd, is_rust_path))
        paths.update(_tree_paths(head, cwd, is_rust_path))
        before = {path: _read_blob(tag, path, cwd) for path in paths}
        after = {path: _read_blob(head, path, cwd) for path in paths}
        return should_publish(before, after)
    except Uncertain as exc:
        print(f"publish gate uncertain ({exc}); publishing", file=sys.stderr)
        return True


def latest_release_tag(
    cwd: str | None = None,
    *,
    head: str = "HEAD",
    releases: Iterable[str] | None = None,
) -> str | None:
    """Newest reachable ``vX.Y.Z`` tag that also has a GitHub Release.

    Reachable means ``git tag --merged <head>``. Among those, the tag whose
    commit is closest to ``head`` wins; a higher version breaks a tie on the
    same commit. Tags that are not merged, or that have no GitHub Release, are
    ignored. If merged tags exist but none (or an unknown set) have a release,
    raise ``Uncertain`` so the caller publishes.
    """
    result = _git(["tag", "--merged", head, "-l", "v*"], cwd)
    if result.returncode != 0:
        raise Uncertain(result.stderr.decode("utf-8", "replace").strip() or "git tag failed")
    merged: list[str] = []
    for line in result.stdout.splitlines():
        try:
            tag = _decode_path(line).strip()
        except Uncertain:
            raise
        if _RELEASE_TAG.fullmatch(tag):
            merged.append(tag)
    if not merged:
        return None
    if releases is None:
        known = _github_release_tags(cwd)
    else:
        known = set(releases)
    candidates = [tag for tag in merged if tag in known]
    if not candidates:
        raise Uncertain("no merged v* tag has a GitHub Release")

    def rank(tag: str) -> tuple[int, tuple[int, ...]]:
        distance = _git(["rev-list", "--count", f"{tag}..{head}"], cwd)
        if distance.returncode != 0:
            raise Uncertain(
                distance.stderr.decode("utf-8", "replace").strip() or f"cannot rank {tag}"
            )
        try:
            commits = int(distance.stdout.decode("utf-8").strip() or "0")
        except ValueError as exc:
            raise Uncertain(f"cannot rank {tag}") from exc
        version = tuple(-int(part) for part in tag[1:].split("."))
        return commits, version

    return min(candidates, key=rank)


def _github_release_tags(cwd: str | None) -> set[str]:
    env = os.environ.copy()
    token = env.get("GH_TOKEN") or env.get("GITHUB_TOKEN")
    if token:
        env["GH_TOKEN"] = token
    proc = subprocess.run(
        ["gh", "release", "list", "--limit", "500", "--json", "tagName"],
        cwd=cwd,
        env=env,
        capture_output=True,
    )
    if proc.returncode != 0:
        detail = proc.stderr.decode("utf-8", "replace").strip()
        raise Uncertain(detail or "gh release list failed")
    try:
        rows = json.loads(proc.stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise Uncertain(f"gh release list: {exc}") from exc
    if not isinstance(rows, list):
        raise Uncertain("gh release list did not return a list")
    if len(rows) >= 500:
        raise Uncertain("gh release list truncated")
    tags: set[str] = set()
    for row in rows:
        if isinstance(row, dict) and isinstance(row.get("tagName"), str):
            tags.add(row["tagName"])
    return tags


def changed_paths(old: str, new: str, cwd: str | None = None) -> list[str]:
    result = _git(["diff", "--name-only", "--no-renames", "-z", old, new], cwd)
    if result.returncode != 0:
        raise Uncertain(result.stderr.decode("utf-8", "replace").strip() or "git diff failed")
    return [_decode_path(name) for name in result.stdout.split(b"\0") if name]


def _tree_paths(rev: str, cwd: str | None, predicate) -> list[str]:
    result = _git(["ls-tree", "-r", "--name-only", "-z", rev], cwd)
    if result.returncode != 0:
        raise Uncertain(result.stderr.decode("utf-8", "replace").strip() or "git ls-tree failed")
    names = [_decode_path(name) for name in result.stdout.split(b"\0") if name]
    return [name for name in names if predicate(name)]


def _decode_path(raw: bytes) -> str:
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise Uncertain("non-utf8 path") from exc


def _read_blob(rev: str, path: str, cwd: str | None) -> TreeEntry:
    """Read one path. Absent is ``None``; present-but-unreadable publishes.

    A submodule (mode ``160000`` / gitlink) cannot be diffed as a blob, so it
    is unreadable. ``cat-file`` failure on a blob that ``ls-tree`` listed is
    the same: the bytes are not available to compare.
    """
    listed = _git(["ls-tree", "-z", rev, "--", path], cwd)
    if listed.returncode != 0:
        raise Uncertain(listed.stderr.decode("utf-8", "replace").strip() or f"ls-tree {path}")
    raw = listed.stdout
    if not raw:
        return TreeEntry(None, None)
    record = raw.split(b"\0", 1)[0]
    meta, sep, name = record.partition(b"\t")
    if sep != b"\t":
        raise Uncertain(f"bad ls-tree record for {path}")
    _decode_path(name)
    parts = meta.split(b" ")
    if len(parts) < 3:
        raise Uncertain(f"bad ls-tree record for {path}")
    try:
        mode = parts[0].decode("ascii")
        kind = parts[1].decode("ascii")
        sha = parts[2].decode("ascii")
    except UnicodeDecodeError as exc:
        raise Uncertain(f"bad ls-tree record for {path}") from exc
    if kind == "commit" or mode == "160000":
        return TreeEntry(Unreadable(f"submodule {path}"), mode)
    if kind != "blob":
        return TreeEntry(Unreadable(f"{kind} {path}"), mode)
    result = _git(["cat-file", "blob", sha], cwd)
    if result.returncode != 0:
        return TreeEntry(Unreadable(f"unreadable {path}"), mode)
    data = result.stdout
    if b"\0" in data:
        return TreeEntry(data, mode)
    try:
        return TreeEntry(data.decode("utf-8"), mode)
    except UnicodeDecodeError:
        return TreeEntry(data, mode)


def _git(args: list[str], cwd: str | None) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True)


def write_output(publish: bool) -> None:
    line = "publish=true\n" if publish else "publish=false\n"
    print(f"publish gate: {line.strip()}")
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(line)


def main() -> int:
    try:
        if os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch":
            publish = True
        else:
            publish = decide_git()
    except Exception as exc:
        print(f"::warning title=Publish gate failed open::{exc}", file=sys.stderr)
        publish = True
    write_output(publish)
    return 0


if __name__ == "__main__":
    sys.exit(main())
