#!/usr/bin/env python3
"""Generate original placeholder art for `projects/fishing/assets/sprites/`.

## 为什么要有这个脚本

精灵目录是 gitignore 的：仓库里只提交**生成方式**，不提交图。但「生成方式」必须
和场景数据对得上 —— 湖面图层的文件名由 `assets/data/lake_layers.json` 的 `slug` 决定：

    运行时（`projects/fishing/src/game/world.rs::layer_path`）:
        sprites/bg/{slug}@{time_of_day}.png

所以这个脚本**从 `data/lake_layers.json` 读 slug 和时段**，不再自己维护一份图层表。
以前脚本里硬编码的是 `far-mountains` 这种短名，而数据里是 `+70_far-mountains`，
跑一次生成出来的是 632 张没人引用的图，而且会覆盖掉本来能用的美术。

## 用法

    python scripts/gen_placeholder_sprites.py              # 生成到工程的 sprites/
    python scripts/gen_placeholder_sprites.py --out <dir>  # 生成到别处（预览用）
    python scripts/gen_placeholder_sprites.py --force      # 允许覆盖已有文件
    python scripts/gen_placeholder_sprites.py --check      # 只检查磁盘上的美术是否齐全

`--check` 只比较文件名：每个 `data/lake_layers.json` 里的图层 × 每个时段都要有对应
的 PNG（缺了就是空白图层，退出码 1）；多出来的图只提示，不算错。

已有目录默认**不覆盖**：这是一套手工调过的美术，占位图不该悄悄把它顶掉。
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import math

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
PROJECT = ROOT / "projects" / "fishing"
LAYERS = PROJECT / "assets" / "data" / "lake_layers.json"
OUT = PROJECT / "assets" / "sprites"

# 图层 slug 里的关键词 → 占位图的底色与尺寸（远 → 近由 slug 的顺序决定）。
# `data/lake_layers.json` 的 slug 形如 `+72_sky` / `+13_+1_front-rocks`，数字是排序权重。
STYLES = [
    ("sky", (150, 176, 196), (1600, 340)),
    ("cloud", (232, 238, 244), (600, 200)),
    ("mountain", (118, 146, 170), (900, 300)),
    ("tree", (58, 92, 74), (400, 200)),
    ("grass", (74, 104, 86), (500, 160)),
    ("seaweed", (58, 92, 74), (300, 140)),
    ("plant", (52, 92, 70), (320, 160)),
    ("dock", (122, 96, 72), (420, 150)),
    ("ground", (38, 72, 96), (1200, 160)),
    ("rock", (70, 66, 62), (260, 120)),
]
DEFAULT_STYLE = ((96, 104, 112), (400, 160))

# 时段染色：和场景里 `time_of_day` 的四个值对齐（未知时段按白天处理）。
TOD_TINT = {
    "dawn": (1.00, 0.86, 0.80),
    "day": (1.00, 1.00, 1.00),
    "dusk": (0.92, 0.78, 0.86),
    "night": (0.46, 0.54, 0.78),
}

PROPS = {
    "water.png": (512, 256, (34, 62, 88)),
    "boat.png": (64, 16, (128, 84, 52)),
    "bobber.png": (16, 32, (214, 72, 64)),
    "hook.png": (5, 6, (196, 196, 200)),
    "splash.png": (960, 16, (220, 236, 246)),
    "ripple.png": (128, 16, (200, 226, 240)),
    "rod.png": (64, 64, (150, 120, 88)),
}
FISH = [
    "Bluegill-Sunfish_L",
    "BrookTrout_L",
    "BrownTrout_L",
    "CommonCarp_L",
    "Crappie_L",
    "LargemouthBass_L",
    "NorthernPike_L",
    "YellowPerch_L",
    "Walleye_L",
    "Lake-Trout_L",
]


# ---------------------------------------------------------------------------
# 场景数据 → 需要哪些文件
# ---------------------------------------------------------------------------


def load_layers() -> dict:
    if not LAYERS.is_file():
        raise SystemExit(f"not found: {LAYERS}")
    return json.loads(LAYERS.read_text(encoding="utf-8"))


def style_for(slug: str) -> tuple[tuple[int, int, int], tuple[int, int]]:
    lowered = slug.lower()
    for keyword, color, size in STYLES:
        if keyword in lowered:
            return color, size
    return DEFAULT_STYLE


def expected_files(data: dict) -> list[str]:
    """`data/lake_layers.json` → 相对 `sprites/` 的完整文件清单。"""
    tods = data.get("time_of_day") or ["dawn", "day", "dusk", "night"]
    files: list[str] = []
    for layer in data.get("layers", []):
        slug = layer["slug"]
        files.extend(f"bg/{slug}@{tod}.png" for tod in tods)
    files.extend(f"fish/{name}.png" for name in FISH)
    files.extend(PROPS)
    return files


# ---------------------------------------------------------------------------
# 画图
# ---------------------------------------------------------------------------


def tinted(color, tod: str):
    r, g, b = TOD_TINT.get(tod, (1.0, 1.0, 1.0))
    return tuple(min(255, int(c * k)) for c, k in zip(color, (r, g, b)))


def layer(size, color, seed):
    im = Image.new("RGBA", size, (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    w, h = size
    # A soft ridge line so parallax motion is visible.
    points = []
    for i in range(0, w + 1, max(1, w // 40)):
        y = int(h * (0.45 + 0.18 * ((i // max(1, w // 8) + seed) % 3) / 2))
        points.append((i, y))
    points += [(w, h), (0, h)]
    d.polygon(points, fill=tuple(color) + (255,))
    return im


def water_tile(size=(512, 256)):
    """Opaque water body: sky-tinted at the horizon, deepening downwards.

    The top row of the texture is the water line, so the quad hides every
    background layer's hard bottom crop instead of letting the sky bleed
    through (a translucent band over a bright sky reads as grey haze).
    """
    w, h = size
    im = Image.new("RGBA", size, (0, 0, 0, 255))
    px = im.load()
    # sRGB stops: horizon (reflects the sky) -> mid lake -> deep water.
    stops = [
        (0.0, (92, 122, 138)),
        (0.18, (58, 92, 116)),
        (0.55, (34, 62, 88)),
        (1.0, (18, 36, 58)),
    ]

    def sample(t):
        for i in range(len(stops) - 1):
            t0, c0 = stops[i]
            t1, c1 = stops[i + 1]
            if t0 <= t <= t1:
                f = (t - t0) / (t1 - t0)
                return tuple(c0[k] + (c1[k] - c0[k]) * f for k in range(3))
        return stops[-1][1]

    for y in range(h):
        t = y / (h - 1)
        r, g, b = sample(t)
        # Ripples get shorter and lower-contrast with distance (nearer water
        # sits at the bottom of the texture).
        amp = 3.0 + 7.0 * t
        for x in range(w):
            wave = math.sin((x * 0.06) + math.sin(y * 0.9) * 2.0)
            ripple = amp * wave
            px[x, y] = (
                max(0, min(255, int(r + ripple * 0.6))),
                max(0, min(255, int(g + ripple * 0.8))),
                max(0, min(255, int(b + ripple))),
                255,
            )
    return im


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------


def generate(out: Path, force: bool) -> int:
    data = load_layers()
    tods = data.get("time_of_day") or ["dawn", "day", "dusk", "night"]

    if out.exists() and any(out.rglob("*.png")) and not force:
        raise SystemExit(
            f"{out} already has art; pass --force to overwrite it "
            "(or --out <dir> to write somewhere else)"
        )

    (out / "bg").mkdir(parents=True, exist_ok=True)
    (out / "fish").mkdir(parents=True, exist_ok=True)
    manifest = {"background": [], "props": {}, "fish": {}}

    for order, layer_def in enumerate(data.get("layers", [])):
        slug = layer_def["slug"]
        color, size = style_for(slug)
        variants = {}
        for tod in tods:
            im = layer(size, tinted(color, tod), order)
            rel = f"sprites/bg/{slug}@{tod}.png"
            im.save(out / "bg" / f"{slug}@{tod}.png")
            variants[tod] = rel
        manifest["background"].append(
            {
                "slug": slug,
                "parallax": layer_def.get("parallax", 1.0),
                "order": order,
                "variants": variants,
            }
        )

    for name, (w, h, color) in PROPS.items():
        if name == "water.png":
            water_tile((w, h)).save(out / name)
            manifest["props"][name] = f"sprites/{name}"
            continue
        im = Image.new("RGBA", (w, h), (0, 0, 0, 0))
        d = ImageDraw.Draw(im)
        d.rounded_rectangle([1, 1, w - 2, h - 2], radius=max(1, h // 4), fill=color + (255,))
        im.save(out / name)
        manifest["props"][name] = f"sprites/{name}"

    for name in FISH:
        im = Image.new("RGBA", (20, 20), (0, 0, 0, 0))
        d = ImageDraw.Draw(im)
        d.ellipse([2, 6, 16, 14], fill=(150, 170, 150, 255))
        d.polygon([(2, 10), (-2 + 4, 5), (2, 15)], fill=(120, 140, 120, 255))
        im.save(out / "fish" / f"{name}.png")
        manifest["fish"][name] = f"sprites/fish/{name}.png"

    (out / "manifest.json").write_text(
        json.dumps(manifest, indent=1, ensure_ascii=False), encoding="utf-8"
    )
    print(f"placeholders -> {out}")
    return 0


def check(out: Path) -> int:
    data = load_layers()
    expected = expected_files(data)
    missing = [name for name in expected if not (out / name).is_file()]
    present = {path.relative_to(out).as_posix() for path in out.rglob("*.png")} if out.is_dir() else set()
    extra = sorted(present - set(expected))
    print(f"{len(expected)} file(s) expected under {out}")
    if missing:
        print(f"missing {len(missing)}:")
        for name in missing[:10]:
            print(f"  - {name}")
        if len(missing) > 10:
            print(f"  ... {len(missing) - 10} more")
    if extra:
        print(f"extra {len(extra)} (not referenced by data/lake_layers.json):")
        for name in extra[:10]:
            print(f"  + {name}")
        if len(extra) > 10:
            print(f"  ... {len(extra) - 10} more")
    # 缺文件 = 失败（图层会渲染成空白）；多出来的图只是没有图层引用它，
    # 美术包里常有别的时段的备用图，不算错。
    if missing:
        return 1
    if extra:
        print("ok: every layer/time-of-day has art (extra files are unused)")
    else:
        print("ok: art matches assets/data/lake_layers.json exactly")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=OUT, help=f"output dir (default {OUT})")
    parser.add_argument("--force", action="store_true", help="overwrite existing art")
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify the art on disk covers every layer/time-of-day in data/lake_layers.json",
    )
    args = parser.parse_args(argv)
    if args.check:
        return check(args.out)
    return generate(args.out, args.force)


if __name__ == "__main__":
    sys.exit(main())
