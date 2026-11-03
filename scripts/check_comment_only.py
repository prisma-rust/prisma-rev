#!/usr/bin/env python3
"""Verify that a branch changes comments only, not code.

    python scripts/check_comment_only.py --base origin/master
    python scripts/check_comment_only.py --base HEAD crates/prism-ecs/src/lib.rs

It lexes the base version and the working-tree version with the same Rust
lexer, drops comments and whitespace, and compares the remaining tokens. A
comment-only diff (including deleting comments) passes; any code token change
fails. Use it on every comment cleanup PR before asking for review.

New files and deleted files are reported as skipped: there is no base blob to
compare against.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.rust_comments import code_signature, lex  # noqa: E402


def git(*args: str) -> tuple[int, str]:
    try:
        proc = subprocess.run(
            ["git", *args],
            cwd=str(ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
    except FileNotFoundError:
        return 127, ""
    return proc.returncode, proc.stdout or ""


def resolve_base(explicit: str | None) -> str:
    if explicit:
        return explicit
    for ref in ("origin/master", "origin/main", "master", "main"):
        if git("rev-parse", "--verify", "--quiet", ref)[0] == 0:
            return ref
    return "HEAD"


def changed_files(base: str) -> list[str]:
    files: set[str] = set()
    probes = (
        ("diff", "--name-only", "--diff-filter=ACMRTUXB", f"{base}...HEAD"),
        ("diff", "--name-only", "--diff-filter=ACMRTUXB", "--cached"),
        ("diff", "--name-only", "--diff-filter=ACMRTUXB"),
        ("ls-files", "--others", "--exclude-standard"),
    )
    for probe in probes:
        code, output = git(*probe)
        if code != 0:
            continue
        files.update(line.strip() for line in output.splitlines() if line.strip())
    return sorted(path for path in files if path.endswith(".rs"))


def explicit_files(paths: list[str]) -> list[str]:
    result: set[str] = set()
    for raw in paths:
        path = Path(raw)
        if not path.is_absolute():
            path = ROOT / path
        if path.is_dir():
            for child in path.rglob("*.rs"):
                try:
                    rel = child.resolve().relative_to(ROOT.resolve()).as_posix()
                except ValueError:
                    rel = child.as_posix()
                result.add(rel)
        elif path.suffix == ".rs":
            try:
                rel = path.resolve().relative_to(ROOT.resolve()).as_posix()
            except ValueError:
                rel = path.as_posix()
            result.add(rel)
    return sorted(result)


def code_tokens(source: str) -> list:
    return [token for token in lex(source) if token.kind not in ("comment", "whitespace")]


def first_difference(base_source: str, current_source: str) -> tuple[str, str] | None:
    base_tokens = code_tokens(base_source)
    current_tokens = code_tokens(current_source)
    for base_token, current_token in zip(base_tokens, current_tokens):
        if base_token.text != current_token.text:
            return (
                f"base line {base_token.line}: {base_token.text!r}",
                f"now  line {current_token.line}: {current_token.text!r}",
            )
    if len(base_tokens) != len(current_tokens):
        if len(base_tokens) > len(current_tokens):
            token = base_tokens[len(current_tokens)]
            return (f"base line {token.line}: {token.text!r} (removed)", "now: end of file")
        token = current_tokens[len(base_tokens)]
        return ("base: end of file", f"now  line {token.line}: {token.text!r} (added)")
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description="Check that a diff changes comments only")
    parser.add_argument("paths", nargs="*", help="explicit files or directories")
    parser.add_argument("--base", help="git ref to compare against (default: origin/master)")
    parser.add_argument("--quiet", action="store_true", help="only print failures")
    args = parser.parse_args()

    base = resolve_base(args.base)
    paths = explicit_files(args.paths) if args.paths else changed_files(base)
    if not paths:
        if not args.quiet:
            print(f"comment-only: no changed .rs files against {base}")
        return 0

    failures: list[str] = []
    skipped: list[str] = []
    checked = 0

    for rel_path in paths:
        current = ROOT / rel_path
        if not current.is_file():
            skipped.append(f"{rel_path} (deleted)")
            continue

        code, base_source = git("show", f"{base}:{rel_path}")
        if code != 0:
            skipped.append(f"{rel_path} (new file)")
            continue

        try:
            current_source = current.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            current_source = current.read_text(encoding="utf-8", errors="replace")

        if code_signature(lex(base_source)) == code_signature(lex(current_source)):
            checked += 1
            continue

        difference = first_difference(base_source, current_source)
        failures.append(rel_path)
        print(f"{rel_path}: code changed (comment-only check failed)", file=sys.stderr)
        if difference is not None:
            before, after = difference
            print(f"  {before}", file=sys.stderr)
            print(f"  {after}", file=sys.stderr)

    for item in skipped:
        if not args.quiet:
            print(f"comment-only: skipped {item}", file=sys.stderr)

    if failures:
        print(
            f"comment-only: {len(failures)} file(s) changed code against {base}: "
            + ", ".join(failures),
            file=sys.stderr,
        )
        return 1

    if not args.quiet:
        print(f"comment-only: ok ({checked} file(s) against {base})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
