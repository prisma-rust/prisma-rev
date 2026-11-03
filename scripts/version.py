#!/usr/bin/env python3
"""Single-source version tooling for the repository.

    python scripts/version.py show
    python scripts/version.py check
    python scripts/version.py check --expect 0.1.0
    python scripts/version.py bump patch
    python scripts/version.py set 0.2.0
    python scripts/version.py tag

The source of truth is `[workspace.package].version` in the root `Cargo.toml`.
Crates in the root workspace inherit it with `version.workspace = true`; the
standalone workspaces (`editor/`, `launcher/`) cannot inherit across Cargo
workspaces, so this script mirrors the value into the files listed in
`TARGETS`.

Rules and the release checklist live in `docs/versioning.md`.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import NoReturn

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - the repo requires Python 3.11+
    raise SystemExit("version.py requires Python 3.11+ (tomllib)")

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "Cargo.toml"
SEMVER_RE = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$"
)


@dataclass(frozen=True)
class Target:
    path: Path
    kind: str  # "toml" or "json"
    section: str = ""  # TOML section without brackets
    label: str = ""


TARGETS = (
    Target(ROOT / "editor" / "Cargo.toml", "toml", "workspace.package", "editor/Cargo.toml"),
    Target(
        ROOT / "launcher" / "src-tauri" / "Cargo.toml",
        "toml",
        "package",
        "launcher/src-tauri/Cargo.toml",
    ),
    Target(ROOT / "crates" / "xtask" / "Cargo.toml", "toml", "package", "crates/xtask/Cargo.toml"),
    Target(ROOT / "launcher" / "package.json", "json", "", "launcher/package.json"),
    Target(
        ROOT / "launcher" / "src-tauri" / "tauri.conf.json",
        "json",
        "",
        "launcher/src-tauri/tauri.conf.json",
    ),
)


def fail(message: str) -> NoReturn:
    raise SystemExit(f"version: {message}")


def read_toml_version(path: Path, section: str) -> str:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    node = data
    for key in section.split(".") if section else ():
        if not isinstance(node, dict) or key not in node:
            fail(f"{path.relative_to(ROOT)} is missing [{section}]")
        node = node[key]
    value = node.get("version") if isinstance(node, dict) else None
    if not isinstance(value, str):
        fail(f"{path.relative_to(ROOT)} [{section}] must contain a literal `version = \"X.Y.Z\"`")
    return value


def read_json_version(path: Path) -> str:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"{path.relative_to(ROOT)} is not valid JSON: {error}")
    value = data.get("version") if isinstance(data, dict) else None
    if not isinstance(value, str):
        fail(f"{path.relative_to(ROOT)} is missing a top-level string `version`")
    return value


def read_version(target: Target) -> str:
    if target.kind == "toml":
        return read_toml_version(target.path, target.section)
    return read_json_version(target.path)


def write_toml_version(path: Path, section: str, version: str) -> None:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    in_section = False
    replaced = False

    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_section = stripped == f"[{section}]"
            continue
        if not in_section or replaced:
            continue
        body = line.rstrip("\r\n")
        newline = line[len(body) :]
        match = re.match(r'^(\s*version\s*=\s*")([^"]*)(".*)$', body)
        if match is None:
            continue
        lines[index] = f"{match.group(1)}{version}{match.group(3)}{newline}"
        replaced = True

    if not replaced:
        fail(f"{path.relative_to(ROOT)} [{section}] has no literal `version = \"X.Y.Z\"` to update")
    path.write_text("".join(lines), encoding="utf-8")


def write_json_version(path: Path, version: str) -> None:
    text = path.read_text(encoding="utf-8")
    old = read_json_version(path)
    pattern = re.compile(r'("version"\s*:\s*")' + re.escape(old) + r'(")')
    updated, count = pattern.subn(lambda match: match.group(1) + version + match.group(2), text, count=1)
    if count != 1:
        fail(f"{path.relative_to(ROOT)} has no top-level `version` to update")
    try:
        json.loads(updated)
    except json.JSONDecodeError as error:
        fail(f"refusing to write invalid JSON to {path.relative_to(ROOT)}: {error}")
    path.write_text(updated, encoding="utf-8")


def write_version(target: Target, version: str) -> None:
    if target.kind == "toml":
        write_toml_version(target.path, target.section, version)
    else:
        write_json_version(target.path, version)


def source_version() -> str:
    return read_toml_version(SOURCE, "workspace.package")


def validate(version: str) -> tuple[int, int, int, str, str]:
    match = SEMVER_RE.match(version)
    if match is None:
        fail(f"invalid SemVer: {version!r} (expected MAJOR.MINOR.PATCH[-pre][+build])")
    major, minor, patch = (int(match.group(index)) for index in (1, 2, 3))
    return major, minor, patch, match.group(4) or "", match.group(5) or ""


def bump(current: str, part: str) -> str:
    major, minor, patch, _pre, _build = validate(current)
    if part == "major":
        major, minor, patch = major + 1, 0, 0
    elif part == "minor":
        minor, patch = minor + 1, 0
    else:
        patch += 1
    return f"{major}.{minor}.{patch}"


def check(expect: str | None, markdown: bool, quiet: bool) -> int:
    version = source_version()
    validate(version)
    problems: list[str] = []
    if expect is not None and version != expect:
        problems.append(f"root Cargo.toml has {version}, expected {expect}")

    for target in TARGETS:
        try:
            value = read_version(target)
        except SystemExit as error:
            problems.append(str(error))
            continue
        if value != version:
            problems.append(f"{target.label}: {value} != {version}")

    if markdown:
        if problems:
            print(f"| 版本一致性 | ❌ | {'；'.join(problems)} |")
        else:
            print(f"| 版本一致性 | ✅ | {version} · {len(TARGETS)} 个同步点 |")
        if problems and not quiet:
            for problem in problems:
                print(f"version: {problem}", file=sys.stderr)
    elif not quiet:
        if problems:
            for problem in problems:
                print(f"version: {problem}", file=sys.stderr)
        else:
            print(f"version: {version} (root Cargo.toml + {len(TARGETS)} synced file(s))")

    return 1 if problems else 0


def update(version: str, dry_run: bool, quiet: bool) -> int:
    validate(version)
    changes: list[tuple[str, str, str]] = []

    old = source_version()
    if old != version:
        changes.append(("Cargo.toml", old, version))
        if not dry_run:
            write_toml_version(SOURCE, "workspace.package", version)

    for target in TARGETS:
        old = read_version(target)
        if old == version:
            continue
        changes.append((target.label, old, version))
        if not dry_run:
            write_version(target, version)

    if not quiet:
        if not changes:
            print(f"version: already {version}; nothing to update")
        else:
            prefix = "would update" if dry_run else "updated"
            for label, old, new in changes:
                print(f"version: {prefix} {label}: {old} -> {new}")
            if not dry_run:
                print("version: run `cargo check` (or `cargo metadata`) to refresh Cargo.lock")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="PrismaRev version tooling")
    parser.add_argument(
        "command",
        nargs="?",
        default="check",
        choices=("show", "check", "bump", "set", "tag"),
        help="default: check",
    )
    parser.add_argument("value", nargs="?", help="bump part (major/minor/patch) or an exact version for set")
    parser.add_argument("--expect", help="expected version (used by tags and the docs workflow)")
    parser.add_argument("--dry-run", action="store_true", help="print changes without writing")
    parser.add_argument("--markdown", action="store_true", help="print one Markdown row on stdout")
    parser.add_argument("--quiet", action="store_true", help="summary only")
    args = parser.parse_args()

    if args.command == "show":
        print(source_version())
        return 0

    if args.command == "check":
        return check(args.expect, args.markdown, args.quiet)

    if args.command == "tag":
        version = source_version()
        if args.expect is not None and version != args.expect:
            fail(f"root Cargo.toml has {version}, expected {args.expect}")
        print(f"v{version}")
        return 0

    if args.command == "bump":
        if args.value not in ("major", "minor", "patch"):
            fail("bump requires one of: major, minor, patch")
        return update(bump(source_version(), args.value), args.dry_run, args.quiet)

    if args.command == "set":
        if not args.value:
            fail("set requires an exact version, for example `set 0.2.0`")
        validate(args.value)
        return update(args.value, args.dry_run, args.quiet)

    fail(f"unknown command: {args.command}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
