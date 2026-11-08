#!/usr/bin/env python3
"""PrismaRev 提交前自检：一条命令，一个可粘贴到 PR 的 Markdown 块。

    python scripts/pr_ready.py                 # 完整自检（默认）
    python scripts/pr_ready.py --quick         # 快速回路：只做 fmt + clippy
    python scripts/pr_ready.py --all           # 全 workspace + 所有独立 workspace
    python scripts/pr_ready.py --base origin/master
    python scripts/pr_ready.py --verbose       # 失败时打印完整输出

它根据分支相对 --base 的改动（含未提交、未跟踪文件）决定要跑什么：

  * 只对**改动过的 .rs 文件**跑 rustfmt（仓库既有 fmt 漂移不算在每个 PR 头上）；
  * 对改动的 .rs 检查注释规范（迁移期只告警，见 `docs/code-style.md`）；
  * 改动到的根 workspace crate：clippy --all-targets -D warnings + cargo test；
  * 改到 shader/reflection/bindings：在临时目录重跑 shader-bindgen，比对是否漂移；
  * 改到 editor/：cargo test（独立 workspace，含数据层与 egui 壳）；
  * 改到 projects/*、launcher/：各自 cargo check（独立 workspace）；
  * 拦截生成物 / 签名材料（.spv、.pak、keystore 等）和空白错误。

stdout 只输出 Markdown；进度和失败详情走 stderr。任何检查失败 → 退出码 1。
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
IS_WINDOWS = os.name == "nt"
sys.path.insert(0, str(Path(__file__).resolve().parent))

from lib.console import force_utf8_streams  # noqa: E402

# 这些文件变化会影响整个根 workspace。
WORKSPACE_WIDE = {
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
}
WORKSPACE_PREFIXES = (".cargo/",)

# CI 的 desktop job 不覆盖 prism-app（它只作为用户项目依赖被构建）。
# 本地默认也跳过，除非它自己改动或 --all；projects/prisma 的 check 会覆盖它。
SKIP_BY_DEFAULT = {"prism-app"}

BAD_SUFFIXES = (
    ".spv",
    ".pak",
    ".pak.meta.json",
    ".keystore",
    ".jks",
    ".p12",
    ".pfx",
    ".pem",
    ".key",
)
BAD_PARTS = ("node_modules/", "target/", "/target/", "dist/")
BAD_NAMES = {".env", "local.properties", "id_rsa", "id_ed25519"}


# --------------------------------------------------------------------------- #
# 基础设施
# --------------------------------------------------------------------------- #

force_utf8_streams()


def eprint(*args: object) -> None:
    print(*args, file=sys.stderr, flush=True)


def run(cmd: list[str], cwd: Path = ROOT) -> tuple[int, str]:
    if cmd:
        # Windows 上 npm/pnpm/tsc 是 .cmd shim，CreateProcess 不会自动补 PATHEXT。
        resolved = shutil.which(cmd[0])
        if resolved:
            cmd = [resolved, *cmd[1:]]
    try:
        proc = subprocess.run(
            cmd,
            cwd=str(cwd),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            env={**os.environ, "PYTHONIOENCODING": "utf-8"},
        )
    except FileNotFoundError:
        return 127, f"command not found: {cmd[0]}"
    return proc.returncode, proc.stdout or ""


def git(*args: str) -> tuple[int, str]:
    return run(["git", *args])


def ensure_toolchain_in_path() -> None:
    """从 Explorer / 新 shell 启动 python 时，cargo 可能不在 PATH。"""
    cargo = shutil.which("cargo")
    if cargo:
        return
    candidates = [Path.home() / ".cargo" / "bin"]
    if IS_WINDOWS:
        candidates.append(Path(os.environ.get("USERPROFILE", "")) / ".cargo" / "bin")
    for candidate in candidates:
        exe = candidate / ("cargo.exe" if IS_WINDOWS else "cargo")
        if exe.exists():
            os.environ["PATH"] = str(candidate) + os.pathsep + os.environ.get("PATH", "")
            return


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
        ("diff", "--name-only", f"{base}...HEAD"),
        ("diff", "--name-only", "--cached"),
        ("diff", "--name-only"),
        ("ls-files", "--others", "--exclude-standard"),
    )
    for probe in probes:
        code, out = git(*probe)
        if code == 0:
            files.update(line.strip() for line in out.splitlines() if line.strip())
    return sorted(f.replace("\\", "/") for f in files if not f.startswith(".zcode/"))


def workspace_members() -> list[str]:
    try:
        import tomllib

        data = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        return [Path(member).name for member in data["workspace"]["members"]]
    except Exception:
        text = (ROOT / "Cargo.toml").read_text(encoding="utf-8", errors="replace")
        return re.findall(r'"crates/([^"/]+)"', text)


def crate_test_command(crate: str) -> list[str]:
    """Build a test command matching the targets actually declared by a crate.

    Some workspace members, such as `prism-editor-mcp`, are binary-only. Passing
    `--lib` unconditionally makes Cargo reject those packages before running any
    tests. Keep the original lib/bin/integration coverage, but only request the
    target kinds the package provides.
    """
    manifest = ROOT / "crates" / crate / "Cargo.toml"
    has_lib = (manifest.parent / "src" / "lib.rs").is_file()
    has_bin = (manifest.parent / "src" / "main.rs").is_file()
    try:
        import tomllib

        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        has_lib = has_lib or "lib" in data
        has_bin = has_bin or "bin" in data
    except (OSError, ValueError):
        pass

    command = ["cargo", "test", "-p", crate]
    if has_lib:
        command.append("--lib")
    if has_bin:
        command.append("--bins")
    command.append("--tests")
    if crate == "prism-render":
        # 每个 GPU 集成测试都在同一个二进制里自建 `VkInstance` / `VkDevice`。
        # 并行初始化在 lavapipe 上偶发 SIGSEGV（#847），二进制内串行化；
        # 其他 crate 仍保持默认并行，避免整体自检变慢。
        command.extend(["--", "--test-threads=1"])
    return command


def has_prefix(paths: list[str], prefix: str) -> bool:
    return any(path.startswith(prefix) for path in paths)


def changed_rs(paths: list[str]) -> list[str]:
    return [p for p in paths if p.endswith(".rs") and (ROOT / p).is_file()]


def display_cmd(cmd: list[str], cwd: Path) -> str:
    rel = cwd.relative_to(ROOT) if cwd != ROOT else Path(".")
    prefix = "" if rel == Path(".") else f"(cd {rel.as_posix()} && "
    suffix = "" if rel == Path(".") else ")"
    return prefix + " ".join(cmd) + suffix


# --------------------------------------------------------------------------- #
# 检查项
# --------------------------------------------------------------------------- #
class Check:
    def __init__(self, name: str, cmd: list[str], cwd: Path = ROOT, detail: str = "") -> None:
        self.name = name
        self.cmd = cmd
        self.cwd = cwd
        self.detail = detail
        self.status = "skip"
        self.output = ""
        self.done = False

    def run(self, verbose: bool) -> bool:
        self.done = True
        eprint(f"→ {self.name}: {display_cmd(self.cmd, self.cwd)}")
        code, out = run(self.cmd, self.cwd)
        self.output = out
        if code == 0:
            self.status = "ok"
            eprint("  ok")
            return True
        self.status = "fail"
        eprint(f"  FAILED (exit {code})")
        if verbose:
            eprint(out)
        else:
            tail = "\n".join(out.rstrip().splitlines()[-25:])
            eprint(tail)
        return False


def test_summary(output: str) -> str:
    passed = sum(int(n) for n in re.findall(r"test result: ok\. (\d+) passed", output))
    failed = sum(int(n) for n in re.findall(r"test result: FAILED\. (\d+) passed", output))
    if failed:
        return f"{passed} passed, {failed} failed"
    return f"{passed} passed" if passed else "ok"


def file_matches_generated(generated: Path, checked_in: Path) -> bool:
    """生成物与已提交文件是否等价。

    bindgen 始终写 LF；Windows 上 `core.autocrlf=true`（仓库没有 .gitattributes）
    会把检出的 `.rs` 转成 CRLF，于是逐字节比较会在**内容完全一致**时假报漂移。
    这里先做逐字节比较（快路径），不同则把两侧行尾归一化成 LF 再比；
    真正的字段 / 常量 / 顺序变化依然会被抓出来。
    """
    produced = generated.read_bytes()
    existing = checked_in.read_bytes()
    if produced == existing:
        return True
    return produced.replace(b"\r\n", b"\n") == existing.replace(b"\r\n", b"\n")


def shader_bindings_check(changed: list[str]) -> Check:
    """在临时目录重跑 bindgen，与已提交的 bindings 比对；不修改工作区。"""
    check = Check("shader bindings drift", ["shader-bindgen", "(temp out)"])
    check.done = True
    touched = has_prefix(changed, "assets/shaders/") or has_prefix(
        changed, "crates/prism-render/src/shader_bindings/"
    )
    if not touched:
        check.status = "skip"
        check.detail = "no shader changes"
        return check

    tmp = Path(tempfile.mkdtemp(prefix="prism-bindgen-"))
    try:
        eprint("→ shader bindings drift: regenerate into temp dir")
        code, out = run(
            [
                "cargo",
                "run",
                "--quiet",
                "--manifest-path",
                str(ROOT / "crates" / "xtask" / "Cargo.toml"),
                "--bin",
                "shader-bindgen",
                "--",
                str(ROOT / "assets" / "shaders" / "reflection"),
                str(tmp),
            ]
        )
        if code != 0:
            check.status = "fail"
            check.output = out
            check.detail = "bindgen failed"
            eprint("  FAILED (bindgen)")
            eprint("\n".join(out.rstrip().splitlines()[-25:]))
            return check

        committed = ROOT / "crates" / "prism-render" / "src" / "shader_bindings"
        diffs: list[str] = []
        crlf_only = 0
        generated = sorted(p for p in tmp.rglob("*") if p.is_file())
        for path in generated:
            rel = path.relative_to(tmp)
            current = committed / rel
            if not current.is_file():
                diffs.append(rel.as_posix())
                continue
            if file_matches_generated(path, current):
                if current.read_bytes() != path.read_bytes():
                    crlf_only += 1
                continue
            diffs.append(rel.as_posix())
        committed_names = {
            p.relative_to(committed).as_posix()
            for p in committed.rglob("*")
            if p.is_file()
        }
        generated_names = {p.relative_to(tmp).as_posix() for p in generated}
        diffs.extend(sorted(committed_names - generated_names))

        if diffs:
            check.status = "fail"
            check.detail = f"{len(diffs)} file(s) stale: {', '.join(diffs[:4])}"
            eprint(f"  FAILED: {check.detail}")
            eprint("  Run: bash assets/shaders/compile.sh && xtask shader-bindgen, then commit.")
        else:
            check.status = "ok"
            check.detail = "up to date"
            eprint("  ok")
            if crlf_only:
                eprint(
                    f"  note: {crlf_only} file(s) differ only in line endings "
                    "(CRLF checkout, LF generated); content matches."
                )
        return check
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def dependency_budget_check() -> Check:
    """玩家依赖树预算（`scripts/check_deps.py`）。

    `prism-app` 是玩家二进制唯一的顶层依赖：它多带一个 crate，每个用户项目、
    每次 Android 打包都会多带一个。这条检查把它变成 CI 红灯，而不是靠人记。
    """
    return Check("依赖预算 (player)", [sys.executable, str(ROOT / "scripts" / "check_deps.py")])


def feature_matrix_check() -> Check:
    """Lego 特性矩阵：关掉某个子系统后仍要能编译（`scripts/check_features.py`）。"""
    return Check("Lego 特性矩阵", [sys.executable, str(ROOT / "scripts" / "check_features.py")])


def artifact_guard(changed: list[str]) -> Check:
    check = Check("artifact / secret guard", ["git", "diff --name-only"])
    check.done = True
    eprint("→ artifact / secret guard")
    bad: list[str] = []
    for path in changed:
        name = Path(path).name
        lowered = path.lower()
        if lowered.endswith(BAD_SUFFIXES) or name in BAD_NAMES:
            bad.append(path)
        elif any(part in path for part in BAD_PARTS):
            bad.append(path)
    if bad:
        check.status = "fail"
        check.detail = f"must not be committed: {', '.join(bad[:5])}"
        eprint(f"  FAILED: {check.detail}")
        return check
    check.status = "ok"
    check.detail = f"{len(changed)} file(s) checked"
    eprint("  ok")
    return check


def whitespace_check() -> Check:
    check = Check("whitespace / conflict markers", ["git", "diff", "--check"])
    check.done = True
    eprint("→ whitespace / conflict markers")
    for args in (("diff", "--check"), ("diff", "--cached", "--check")):
        code, out = git(*args)
        if code != 0:
            check.status = "fail"
            check.output = out
            check.detail = out.strip().splitlines()[0] if out.strip() else "diff --check failed"
            eprint(f"  FAILED: {check.detail}")
            return check
    check.status = "ok"
    check.detail = "clean"
    eprint("  ok")
    return check


def format_check(files: list[str], verbose: bool) -> Check:
    check = Check("format (changed .rs)", ["rustfmt", "--check"])
    check.done = True
    eprint("→ format (changed .rs)")
    if not files:
        check.status = "skip"
        check.detail = "no .rs changes"
        return check
    rustfmt = shutil.which("rustfmt")
    if not rustfmt:
        check.status = "skip"
        check.detail = "rustfmt not on PATH"
        eprint("  skipped: rustfmt not on PATH")
        return check

    failures: list[str] = []
    for path in files:
        cwd = ROOT
        code, out = run([rustfmt, "--edition", "2021", "--check", str(ROOT / path)], cwd)
        if code != 0:
            failures.append(path)
            if verbose:
                eprint(out)
    if failures:
        check.status = "fail"
        check.detail = f"{len(failures)} file(s): {', '.join(failures[:4])}"
        eprint(f"  FAILED: {check.detail}")
        eprint("  Run: cargo fmt --all  (or rustfmt on the listed files)")
    else:
        check.status = "ok"
        check.detail = f"{len(files)} file(s)"
        eprint("  ok")
    return check


def comment_check(base: str) -> Check:
    """注释规范检查（`scripts/check_comments.py`）。

    迁移期只对改动文件告警，不阻塞合并；存量清零后把 `--warn-only` 去掉即可
    变成硬门禁。规则见 `docs/code-style.md`。
    """
    check = Check(
        "注释规范",
        [
            sys.executable,
            str(ROOT / "scripts" / "check_comments.py"),
            "--changed",
            "--base",
            base,
            "--warn-only",
        ],
    )
    check.done = True
    eprint("→ 注释规范: changed .rs files (report-only)")
    code, out = run(check.cmd)
    check.output = out
    if code != 0:
        check.status = "fail"
        check.detail = "check_comments.py failed"
        eprint("  FAILED")
        eprint("\n".join(out.rstrip().splitlines()[-10:]))
        return check

    match = re.search(r"comments: (\d+) finding\(s\)", out)
    count = int(match.group(1)) if match else 0
    if count:
        check.status = "warn"
        check.detail = f"{count} 处（迁移期，未阻塞）"
        eprint(f"  warn: {count} finding(s)")
    else:
        check.status = "ok"
        check.detail = "clean"
        eprint("  ok")
    return check


def editor_checks(changed: list[str], quick: bool) -> list[Check]:
    """`editor/` 是独立 workspace（egui + ash），只能在有 Vulkan 的桌面上运行。

    Termux 上只做类型检查：它同时链接 `prism-render` / `prism-engine`，所以
    根 workspace 的渲染 / 引擎改动也要复查一遍编辑器。
    """
    manifest = ROOT / "editor" / "Cargo.toml"
    tests = Check(
        "editor tests",
        ["cargo", "test", "--manifest-path", str(manifest)],
    )
    check = Check(
        "editor check",
        [
            "cargo",
            "check",
            "--manifest-path",
            str(manifest),
            "--target",
            "x86_64-pc-windows-gnu",
        ],
    )

    touched = (
        has_prefix(changed, "editor/")
        or has_prefix(changed, "crates/prism-render/")
        or has_prefix(changed, "crates/prism-engine/")
        or has_prefix(changed, "crates/prism-scene/")
    )
    if not touched:
        for item in (tests, check):
            item.status = "skip"
            item.detail = "editor untouched"
            item.done = True
        return [tests, check]

    code, installed = run(["rustup", "target", "list", "--installed"])
    if code != 0 or "x86_64-pc-windows-gnu" not in installed:
        check.status = "skip"
        check.detail = "rustup target x86_64-pc-windows-gnu 未安装"
        check.done = True
    if quick:
        for item in (tests, check):
            item.status = "skip"
            item.detail = "quick mode"
            item.done = True
    return [tests, check]


# --------------------------------------------------------------------------- #
# 主流程
# --------------------------------------------------------------------------- #
def main() -> int:
    parser = argparse.ArgumentParser(description="PrismaRev pre-PR self-check")
    parser.add_argument("--base", help="比较基准（默认 origin/master）")
    parser.add_argument("--quick", action="store_true", help="只做 fmt + clippy，不跑测试")
    parser.add_argument("--all", action="store_true", help="检查所有 workspace 与独立项目")
    parser.add_argument("--verbose", action="store_true", help="失败时打印完整输出")
    args = parser.parse_args()

    ensure_toolchain_in_path()
    base = resolve_base(args.base)
    changed = changed_files(base)
    rs_files = changed_rs(changed)

    members = workspace_members()
    crates: set[str] = set()
    if args.all:
        crates = set(members)
    else:
        for path in changed:
            if path.startswith("crates/"):
                parts = path.split("/")
                if len(parts) > 1 and parts[1] in members:
                    crates.add(parts[1])
        if any(p in WORKSPACE_WIDE or p.startswith(WORKSPACE_PREFIXES) for p in changed):
            crates = set(members)

    # `prism-app` 只在对它的检查成本高 / CI 不覆盖时默认跳过；当它**自己**被
    # 改动时必须跑，否则改 `prism-app` 的自检会显示「touched crates: (none)」。
    directly_changed = {
        path.split("/")[1]
        for path in changed
        if path.startswith("crates/") and len(path.split("/")) > 1
    }
    if args.all:
        default_crates = sorted(crates)
    else:
        default_crates = sorted(crates - (SKIP_BY_DEFAULT - directly_changed))
    eprint(f"PrismaRev self-check — base {base}, {len(changed)} changed file(s)")
    eprint(f"  touched crates: {', '.join(default_crates) or '(none)'}")

    checks: list[Check] = [whitespace_check(), artifact_guard(changed)]
    checks.append(format_check(rs_files, args.verbose))
    checks.append(comment_check(base))

    if default_crates:
        clippy_cmd = ["cargo", "clippy"]
        for crate in default_crates:
            clippy_cmd += ["-p", crate]
        clippy_cmd += ["--all-targets", "--", "-D", "warnings"]
        checks.append(Check(f"clippy ({len(default_crates)} crate{'s' if len(default_crates) > 1 else ''})", clippy_cmd))

        if not args.quick:
            # Run each package separately because Cargo applies `--lib` and
            # `--bins` to the whole selection. A binary-only package must not
            # inherit `--lib` from a neighboring library package.
            for crate in default_crates:
                checks.append(Check(f"test ({crate})", crate_test_command(crate)))

    if args.all or has_prefix(changed, "crates/xtask/"):
        checks.append(
            Check(
                "xtask check",
                ["cargo", "check", "--manifest-path", str(ROOT / "crates" / "xtask" / "Cargo.toml")],
            )
        )

    checks.append(shader_bindings_check(changed))
    checks.append(dependency_budget_check())
    if args.all or any(path.endswith("Cargo.toml") for path in changed):
        checks.append(feature_matrix_check())

    checks.extend(editor_checks(changed, args.quick))

    if not args.quick:
        standalone = [
            ("projects/prisma", "projects/prisma/"),
            ("projects/prisma-sponza", "projects/prisma-sponza/"),
            ("launcher", "launcher/"),
        ]
        for name, prefix in standalone:
            manifest = ROOT / name / "Cargo.toml"
            if manifest.is_file() and (args.all or has_prefix(changed, prefix)):
                checks.append(
                    Check(
                        f"{name} check",
                        ["cargo", "check", "--manifest-path", str(manifest)],
                    )
                )

    failed = False
    for check in checks:
        if check.done:
            continue
        if not check.run(args.verbose):
            failed = True
    if not failed:
        failed = any(check.status == "fail" for check in checks)

    # 特定检查的 detail 需要在运行后才能确定。
    for check in checks:
        if check.name.startswith("test (") and check.status == "ok":
            check.detail = test_summary(check.output)
        elif check.name == "editor check" and check.status == "ok":
            check.detail = "x86_64-pc-windows-gnu"
        elif check.name == "editor tests" and check.status == "ok":
            check.detail = test_summary(check.output)

        elif check.name.startswith("Lego 特性矩阵") and check.status == "ok":
            check.detail = check.output.strip().split("|")[-2].strip()

        elif check.name.startswith("依赖预算") and check.status == "ok":
            match = re.search(r"player (\d+)/(\d+) crates", check.output)
            check.detail = f"{match.group(1)}/{match.group(2)} crates" if match else ""

        elif check.name.startswith("clippy (") and check.status == "ok":
            check.detail = "-D warnings"

    # 推送前同步主干：落后多少 commit 要说出来，别把冲突留到 PR 上。
    behind = 0
    behind_code, behind_out = git("rev-list", "--count", f"HEAD..{base}")
    if behind_code == 0 and behind_out.strip().isdigit():
        behind = int(behind_out.strip())
    if behind:
        eprint(
            f"  ⚠ 落后 {base} {behind} 个 commit —— 先在本地 `git rebase {base}` 解冲突再 push"
        )

    sha_code, sha_out = git("rev-parse", "--short", "HEAD")
    sha = sha_out.strip() if sha_code == 0 else "unknown"
    dirty = bool(git("status", "--porcelain")[1].strip())
    mode = "quick" if args.quick else ("all" if args.all else "full")

    lines = [
        "## Self-check (`scripts/pr_ready.py`)",
        "",
        f"- mode: **{mode}** · base: `{base}`"
        + (f"（**落后 {behind} 个 commit，未 rebase**）" if behind else "")
        + f" · HEAD: `{sha}`"
        + (" · working tree dirty" if dirty else "")
        + f" · changed: {len(changed)} file(s)",
        f"- crates: {', '.join(f'`{c}`' for c in default_crates) or '—'}",
        "",
        "| Check | Result | Detail |",
        "|---|---|---|",
    ]
    icons = {"ok": "✅", "fail": "❌", "skip": "⏭️", "warn": "⚠️"}
    for check in checks:
        detail = check.detail.replace("|", "\\|")
        lines.append(f"| {check.name} | {icons[check.status]} | {detail} |")
    lines.append("")
    if failed:
        # A check can fail because the tool itself crashed (bad console encoding,
        # missing binary). Keep the tail in the report so a red row is never a
        # mystery that has to be re-run with --verbose.
        lines.append("<details><summary>Failed check output (last 25 lines each)</summary>")
        lines.append("")
        for check in checks:
            if check.status != "fail":
                continue
            tail = "\n".join(check.output.rstrip().splitlines()[-25:]).strip()
            lines.append(f"**`{check.name}`**")
            lines.append("")
            lines.append("```text")
            lines.extend(tail.splitlines() or ["(no output)"])
            lines.append("```")
            lines.append("")
        lines.append("</details>")
        lines.append("")
    if failed:
        lines.append("**Result: ❌ checks failed**")
    elif any(check.status == "warn" for check in checks):
        lines.append("**Result: ✅ all checks passed**（注释规范仅有迁移期告警）")
    else:
        lines.append("**Result: ✅ all checks passed**")

    # 未运行的 skip 项没有经过 run()，这里也把它们标出来。
    print("\n".join(lines))
    sys.stdout.flush()

    if failed and not shutil.which("slangc"):
        eprint("")
        eprint("提示：本机没有 slangc。编译依赖 assets/shaders/*.spv；")
        eprint("     请先运行 scripts/run.ps1，或 bash assets/shaders/compile.sh。")

    eprint("")
    eprint("PASS" if not failed else "FAIL")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
