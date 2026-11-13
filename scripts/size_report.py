#!/usr/bin/env python3
"""子系统体积账本：按 feature 组合构建玩家二进制，量出每块「砖」值多少字节。

    python scripts/size_report.py            # 跑默认组合表（增量构建，已构建过则秒出）
    python scripts/size_report.py --release=false

做法：临时改写 `projects/template/Cargo.toml` 的 feature 列表 → `cargo build -p template`
→ 记录二进制大小 → 还原文件。用**同一个**工程和 profile，保证差值只来自 feature。

⚠️ 这是**未 strip** 的 release 二进制。strip 后比例不同（符号表被去掉），
所以只在同一张表内比较，不要跨表比数字。
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "projects" / "template"

APP_DEP = 'prism-app = { path = "../../crates/prism-app", optional = true }'
ENGINE_DEP = 'prism-engine = { path = "../../crates/prism-engine", optional = true }'

# (标签, 启用的 prism-app features)。基线为「全关」。
COMBOS: list[tuple[str, list[str]]] = [
    ("base（全关）", []),
    ("pathtrace", ["pathtrace"]),
    ("audio", ["audio"]),
    ("physics", ["physics"]),
    ("audio + pathtrace", ["audio", "pathtrace"]),
    ("all（audio+pathtrace+physics）", ["audio", "pathtrace", "physics"]),
]


def build(features: list[str]) -> int:
    original = (PROJECT / "Cargo.toml").read_text()
    try:
        feat = ", ".join(f'"{f}"' for f in features)
        patched = original.replace(
            APP_DEP,
            f'prism-app = {{ path = "../../crates/prism-app", optional = true, '
            f'default-features = false, features = [{feat}] }}',
        ).replace(
            ENGINE_DEP,
            'prism-engine = { path = "../../crates/prism-engine", optional = true, '
            'default-features = false }',
        )
        if patched == original:
            raise SystemExit("size_report: 找不到 template 的 prism-app/prism-engine 依赖行")
        (PROJECT / "Cargo.toml").write_text(patched)

        proc = subprocess.run(
            ["cargo", "build", "-p", "template", "--release"], cwd=PROJECT, capture_output=True, text=True
        )
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr[-3000:])
            raise SystemExit("size_report: 构建失败")
        return (PROJECT / "target" / "release" / "template").stat().st_size
    finally:
        (PROJECT / "Cargo.toml").write_text(original)


def main() -> int:
    parser = argparse.ArgumentParser(description="PrismaRev subsystem binary-size ledger")
    parser.add_argument("--json", action="store_true", help="只输出 JSON")
    args = parser.parse_args()

    sizes = {label: build(feats) for label, feats in COMBOS}
    base = sizes[COMBOS[0][0]]

    if args.json:
        print(json.dumps(sizes, ensure_ascii=False, indent=2))
        return 0

    print("| 组合 | 二进制 | 相对 base |")
    print("|---|---|---|")
    for label, size in sizes.items():
        delta = size - base
        sign = "" if delta == 0 else f"+{delta:,}" if delta > 0 else f"{delta:,}"
        print(f"| {label} | {size / 1e6:.2f} MB | {sign} |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
