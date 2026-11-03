"""Rust source lexer for the repository comment checks.

The checks need to tell comments apart from string literals, raw strings,
char literals and lifetimes. A regex over lines is not enough: `//` inside a
string is not a comment, and a `'` can start either a char literal or a
lifetime. This module keeps the scanning rules in one place so
`check_comments.py` and `check_comment_only.py` agree on what a comment is.

The lexer is intentionally small and dependency-free. It does not build a
full Rust AST; it only needs stable token boundaries for:
  * listing comments with their line numbers;
  * comparing two versions of a file with comments and whitespace ignored.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

__all__ = [
    "Token",
    "lex",
    "code_signature",
    "comment_body",
    "has_module_doc",
]


@dataclass(frozen=True)
class Token:
    """One lexed token.

    `kind` is one of `comment`, `whitespace`, `string`, `char` or `code`.
    `doc` is `outer` / `inner` for doc comments and empty otherwise.
    """

    kind: str
    text: str
    line: int
    column: int = 1
    doc: str = ""

    @property
    def is_comment(self) -> bool:
        return self.kind == "comment"


# Longest first so `<<=` is not split into `<<` + `=`.
_MULTI_CHAR_OPERATORS = (
    "<<=",
    ">>=",
    "...",
    "..=",
    "::",
    "->",
    "=>",
    "==",
    "!=",
    "<=",
    ">=",
    "&&",
    "||",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "&=",
    "|=",
    "^=",
    "<<",
    ">>",
    "..",
)


class _Lexer:
    def __init__(self, source: str) -> None:
        self.source = source
        self.length = len(source)
        self.index = 0
        self.line = 1
        self.column = 1

    def _advance(self, count: int) -> None:
        if count <= 0:
            return
        text = self.source[self.index : self.index + count]
        self.index += count
        newline = text.rfind("\n")
        if newline == -1:
            self.column += count
        else:
            self.line += text.count("\n")
            self.column = count - newline

    def tokens(self) -> Iterable[Token]:
        while self.index < self.length:
            start_line = self.line
            start_column = self.column
            char = self.source[self.index]

            if char in " \t\r\n":
                start = self.index
                while self.index < self.length and self.source[self.index] in " \t\r\n":
                    self._advance(1)
                yield Token("whitespace", self.source[start : self.index], start_line, start_column)
                continue

            if self.source.startswith("//", self.index):
                yield self._line_comment(start_line, start_column)
                continue

            if self.source.startswith("/*", self.index):
                yield self._block_comment(start_line, start_column)
                continue

            raw = self._raw_string_prefix()
            if raw is not None:
                prefix_length, hashes = raw
                yield self._raw_string(prefix_length, hashes, start_line, start_column)
                continue

            if self.source.startswith('b"', self.index) or self.source.startswith('c"', self.index):
                quote = self.index + 1
                end = self._normal_string_end(quote)
                text = self.source[self.index : end]
                self._advance(end - self.index)
                yield Token("string", text, start_line, start_column)
                continue

            if self.source.startswith("b'", self.index):
                end = self._char_end(self.index + 1)
                if end is not None:
                    text = self.source[self.index : end]
                    self._advance(end - self.index)
                    yield Token("char", text, start_line, start_column)
                    continue

            if char == '"':
                end = self._normal_string_end(self.index)
                text = self.source[self.index : end]
                self._advance(end - self.index)
                yield Token("string", text, start_line, start_column)
                continue

            if char == "'":
                end = self._char_end(self.index)
                if end is not None:
                    text = self.source[self.index : end]
                    self._advance(end - self.index)
                    yield Token("char", text, start_line, start_column)
                    continue
                # Lifetime or loop label: `'a`, `'static`, `'outer:`.
                self._advance(1)
                yield Token("code", char, start_line, start_column)
                continue

            if char.isalpha() or char == "_":
                start = self.index
                self._advance(1)
                while self.index < self.length:
                    current = self.source[self.index]
                    if current.isalnum() or current == "_":
                        self._advance(1)
                    else:
                        break
                yield Token("code", self.source[start : self.index], start_line, start_column)
                continue

            if char.isdigit():
                start = self.index
                self._advance(1)
                while self.index < self.length:
                    current = self.source[self.index]
                    if current.isalnum() or current in "._":
                        self._advance(1)
                    else:
                        break
                yield Token("code", self.source[start : self.index], start_line, start_column)
                continue

            operator = next(
                (op for op in _MULTI_CHAR_OPERATORS if self.source.startswith(op, self.index)),
                None,
            )
            if operator is not None:
                self._advance(len(operator))
                yield Token("code", operator, start_line, start_column)
                continue

            self._advance(1)
            yield Token("code", char, start_line, start_column)

    def _line_comment(self, line: int, column: int) -> Token:
        start = self.index
        end = self.source.find("\n", self.index)
        if end == -1:
            end = self.length
        text = self.source[start:end]
        self._advance(end - start)

        doc = ""
        if text.startswith("///") and not text.startswith("////"):
            doc = "outer"
        elif text.startswith("//!"):
            doc = "inner"
        return Token("comment", text, line, column, doc)

    def _block_comment(self, line: int, column: int) -> Token:
        start = self.index
        end = self.index + 2
        depth = 1
        while end < self.length and depth:
            if self.source.startswith("/*", end):
                depth += 1
                end += 2
            elif self.source.startswith("*/", end):
                depth -= 1
                end += 2
            else:
                end += 1
        text = self.source[start:end]
        self._advance(end - start)

        doc = ""
        if text.startswith("/**") and not text.startswith("/**/"):
            doc = "outer"
        elif text.startswith("/*!"):
            doc = "inner"
        return Token("comment", text, line, column, doc)

    def _raw_string_prefix(self) -> tuple[int, int] | None:
        """Return `(prefix_length, hash_count)` for `r"..."` / `br#"..."#`."""
        for prefix in ("br", "cr", "r"):
            if not self.source.startswith(prefix, self.index):
                continue
            cursor = self.index + len(prefix)
            hashes = 0
            while cursor < self.length and self.source[cursor] == "#":
                hashes += 1
                cursor += 1
            if cursor < self.length and self.source[cursor] == '"':
                return len(prefix), hashes
        return None

    def _raw_string(self, prefix_length: int, hashes: int, line: int, column: int) -> Token:
        start = self.index
        content_start = start + prefix_length + hashes + 1
        terminator = '"' + ("#" * hashes)
        end = self.source.find(terminator, content_start)
        if end == -1:
            end = self.length
        else:
            end += len(terminator)
        text = self.source[start:end]
        self._advance(end - start)
        return Token("string", text, line, column)

    def _normal_string_end(self, quote: int) -> int:
        cursor = quote + 1
        while cursor < self.length:
            char = self.source[cursor]
            if char == "\\":
                cursor += 2
            elif char == '"':
                return cursor + 1
            else:
                cursor += 1
        return self.length

    def _char_end(self, quote: int) -> int | None:
        """End offset for a char literal starting at `quote`, or None for a lifetime."""
        cursor = quote + 1
        if cursor >= self.length:
            return None

        if self.source[cursor] == "\\":
            cursor += 1
            if cursor >= self.length:
                return None
            if self.source[cursor] == "u" and cursor + 1 < self.length and self.source[cursor + 1] == "{":
                close = self.source.find("}", cursor + 2)
                if close == -1:
                    return None
                cursor = close + 1
            elif self.source[cursor] == "x":
                cursor += 3
            else:
                cursor += 1
            if cursor < self.length and self.source[cursor] == "'":
                return cursor + 1
            return None

        if cursor + 1 < self.length and self.source[cursor + 1] == "'":
            return cursor + 2
        return None


def lex(source: str) -> list[Token]:
    """Lex `source` into code, string, char and comment tokens."""
    return list(_Lexer(source).tokens())


def code_signature(tokens: Iterable[Token]) -> str:
    """A comparison key that keeps code tokens and drops comments/whitespace.

    Token boundaries are preserved with a NUL separator. This lets a
    comment-only diff compare equal while a real code edit (including one that
    glues two identifiers together) still differs.
    """
    return "\x00".join(token.text for token in tokens if token.kind not in ("comment", "whitespace"))


def comment_body(token: Token) -> str:
    """Comment text without delimiters, leading `*`, or trailing `*/`."""
    text = token.text
    if text.startswith("//"):
        body = text[2:]
        if body.startswith("/") or body.startswith("!"):
            body = body[1:]
        return body.strip()

    body = text[2:]
    if body.endswith("*/"):
        body = body[:-2]
    if body.startswith("*") or body.startswith("!"):
        body = body[1:]

    lines: list[str] = []
    for line in body.splitlines():
        stripped = line.strip()
        if stripped.startswith("*"):
            stripped = stripped[1:].lstrip()
        lines.append(stripped)
    return "\n".join(lines).strip()


def has_module_doc(source: str) -> bool:
    """True when an inner doc comment appears near the top of the file."""
    for line in source.splitlines()[:40]:
        stripped = line.lstrip()
        if stripped.startswith("//!") or stripped.startswith("/*!"):
            return True
    return False
