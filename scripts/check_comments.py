#!/usr/bin/env python3
"""Check code comments against `docs/code-style.md`.

    python scripts/check_comments.py --all
    python scripts/check_comments.py --changed --base origin/master
    python scripts/check_comments.py --staged --markdown
    python scripts/check_comments.py crates/prism-ecs/src/lib.rs

What it flags:
  * CJK characters in comments (comments must be English);
  * decorative separators such as `// ----------`;
  * `TODO` / `FIXME` / `XXX` / `HACK` without an issue reference;
  * `lib.rs` / `main.rs` / `mod.rs` without an inner `//!` module doc.

Strings and user-facing text are not comments, so Chinese UI strings and
error messages are not flagged. Generated files such as shader bindings and
Tauri codegen output are skipped.

The default mode checks files changed against the base branch. Use `--all`
for the whole repository; during the migration this will report the existing
backlog, so CI wiring should start with `--changed --warn-only`.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.console import force_utf8_streams  # noqa: E402
from lib.rust_comments import comment_body, has_module_doc, lex  # noqa: E402

# CJK ideographs, CJK punctuation and fullwidth forms. Typographic symbols
# such as arrows or em dashes are allowed in English prose.
CJK_RE = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\u3000-\u303f\uff01-\uff60]")
SEPARATOR_RE = re.compile(r"^[\s\-=_*#~`^+─━┄┅┈┉…·.]+$")
TODO_RE = re.compile(r"\b(TODO|FIXME|XXX|HACK)\b")
ISSUE_RE = re.compile(r"(#\d+|https?://\S+|issues?/\d+)")

EXCLUDED_PREFIXES = (
    "crates/prism-render/src/shader_bindings/",
    "launcher/src-tauri/gen/",
    "tools/slang/",
)

MODULE_FILES = {"lib.rs", "main.rs", "mod.rs"}


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


def normalize(path: str) -> str:
    return path.replace("\\", "/").lstrip("./")


def is_excluded(path: str) -> bool:
    normalized = normalize(path)
    if normalized.startswith(EXCLUDED_PREFIXES):
        return True
    if normalized.startswith("target/") or "/target/" in normalized:
        return True
    if normalized.startswith("node_modules/") or "/node_modules/" in normalized:
        return True
    if normalized.startswith(".zcode/"):
        return True
    return False


def display_path(path: str) -> str:
    resolved = Path(path)
    if not resolved.is_absolute():
        resolved = ROOT / resolved
    try:
        return resolved.resolve().relative_to(ROOT.resolve()).as_posix()
    except ValueError:
        return resolved.as_posix()


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
    return sorted(path for path in files if path.endswith(".rs") and not is_excluded(path))


def staged_files() -> list[str]:
    code, output = git("diff", "--name-only", "--diff-filter=ACMRTUXB", "--cached")
    if code != 0:
        return []
    return sorted(
        line.strip()
        for line in output.splitlines()
        if line.strip().endswith(".rs") and not is_excluded(line.strip())
    )


def tracked_files() -> list[str]:
    code, output = git("ls-files", "-z", "--", "*.rs")
    if code == 0:
        return sorted(path for path in output.split("\0") if path and not is_excluded(path))

    # Not a git checkout (for example a source tarball): walk the tree.
    return sorted(
        path.relative_to(ROOT).as_posix()
        for path in ROOT.rglob("*.rs")
        if not is_excluded(path.relative_to(ROOT).as_posix())
    )


def explicit_files(paths: list[str]) -> list[str]:
    result: set[str] = set()
    for raw in paths:
        path = Path(raw)
        if not path.is_absolute():
            path = ROOT / path
        if path.is_dir():
            for child in path.rglob("*.rs"):
                rel = child.relative_to(ROOT).as_posix() if child.is_relative_to(ROOT) else child.as_posix()
                if not is_excluded(rel):
                    result.add(rel)
        elif path.suffix == ".rs" and not is_excluded(display_path(raw)):
            result.add(display_path(raw))
    return sorted(result)


def is_separator(body: str) -> bool:
    compact = body.strip()
    return len(compact) >= 4 and bool(SEPARATOR_RE.fullmatch(compact))


def first_line(body: str, limit: int = 100) -> str:
    line = body.strip().splitlines()[0] if body.strip() else ""
    return line if len(line) <= limit else line[: limit - 1] + "…"


def check_file(rel_path: str) -> list[tuple[str, int, str]]:
    path = ROOT / rel_path
    try:
        source = path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        source = path.read_text(encoding="utf-8", errors="replace")

    issues: list[tuple[str, int, str]] = []
    for token in lex(source):
        if not token.is_comment:
            continue
        body = comment_body(token)
        if CJK_RE.search(body):
            issues.append(("cjk", token.line, f"comment must be English: {first_line(body)}"))
        if is_separator(body):
            issues.append(("separator", token.line, "decorative separator"))
        if TODO_RE.search(body) and not ISSUE_RE.search(body):
            issues.append(
                ("todo", token.line, "TODO/FIXME without an issue reference (`TODO(#123)`)")
            )

    if path.name in MODULE_FILES and not has_module_doc(source):
        issues.append(("module-doc", 1, "missing `//!` module documentation"))

    return issues


def markdown_row(total: int, counts: dict[str, int], warn_only: bool) -> str:
    if not total:
        return "| 注释规范 | ✅ | 无 |"
    status = "⚠️" if warn_only else "❌"
    detail = (
        f"{total} 处：CJK {counts.get('cjk', 0)}，"
        f"分隔线 {counts.get('separator', 0)}，"
        f"TODO {counts.get('todo', 0)}，"
        f"缺模块文档 {counts.get('module-doc', 0)}"
    )
    return f"| 注释规范 | {status} | {detail} |"


def main() -> int:
    force_utf8_streams()
    parser = argparse.ArgumentParser(description="Check Rust comments against the repository style")
    parser.add_argument("paths", nargs="*", help="explicit files or directories; overrides selection flags")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--all", action="store_true", help="check every tracked .rs file")
    selection.add_argument("--changed", action="store_true", help="check files changed against --base")
    selection.add_argument("--staged", action="store_true", help="check staged .rs files")
    parser.add_argument("--base", help="git ref for --changed (default: origin/master when present)")
    parser.add_argument("--warn-only", action="store_true", help="print findings but exit 0")
    parser.add_argument("--markdown", action="store_true", help="print one Markdown row on stdout")
    parser.add_argument("--quiet", action="store_true", help="summary only")
    parser.add_argument("--max-examples", type=int, default=30, help="limit printed findings")
    args = parser.parse_args()

    if args.paths:
        paths = explicit_files(args.paths)
    elif args.staged:
        paths = staged_files()
    elif args.changed:
        paths = changed_files(resolve_base(args.base))
    elif args.all:
        paths = tracked_files()
    else:
        # Default to the useful PR behavior when git is available.
        if git("rev-parse", "--verify", "--quiet", "HEAD")[0] == 0:
            paths = changed_files(resolve_base(args.base))
        else:
            paths = tracked_files()

    if not paths:
        if args.markdown:
            print(markdown_row(0, {}, args.warn_only))
        elif not args.quiet:
            print("comments: no .rs files to check")
        return 0

    issues_by_file: list[tuple[str, list[tuple[str, int, str]]]] = []
    counts: dict[str, int] = {}
    total = 0
    for rel_path in paths:
        issues = check_file(rel_path)
        if not issues:
            continue
        issues_by_file.append((rel_path, issues))
        total += len(issues)
        for kind, _line, _message in issues:
            counts[kind] = counts.get(kind, 0) + 1

    if args.markdown:
        print(markdown_row(total, counts, args.warn_only))
        if total and not args.quiet:
            stream = sys.stderr
            shown = 0
            for rel_path, issues in issues_by_file:
                for kind, line, message in issues:
                    if shown >= args.max_examples:
                        break
                    print(f"{rel_path}:{line}: [{kind}] {message}", file=stream)
                    shown += 1
            if total > shown:
                print(f"comments: ... {total - shown} more finding(s)", file=stream)
    elif not args.quiet:
        shown = 0
        for rel_path, issues in issues_by_file:
            for kind, line, message in issues:
                if shown >= args.max_examples:
                    break
                print(f"{rel_path}:{line}: [{kind}] {message}")
                shown += 1
        if total > shown:
            print(f"comments: ... {total - shown} more finding(s)")

    if total:
        summary = (
            f"comments: {total} finding(s) in {len(issues_by_file)} file(s) "
            f"(cjk={counts.get('cjk', 0)}, separator={counts.get('separator', 0)}, "
            f"todo={counts.get('todo', 0)}, module-doc={counts.get('module-doc', 0)})"
        )
        if not args.quiet:
            print(summary, file=sys.stderr if args.markdown else sys.stdout)
    elif not args.quiet and not args.markdown:
        print("comments: ok")

    return 0 if args.warn_only or not total else 1


if __name__ == "__main__":
    raise SystemExit(main())
