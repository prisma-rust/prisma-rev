#!/usr/bin/env python3
"""
PrismaRev 引擎展示视频制作脚本（简化版）

不依赖引擎截图，纯动效展示特性清单
"""

import os
import sys
import subprocess
from pathlib import Path
from typing import List

# 检查并安装依赖
try:
    import moviepy
    from moviepy import (
        VideoClip, TextClip, CompositeVideoClip,
        AudioFileClip, concatenate_videoclips, vfx
    )
    import numpy as np
except ImportError:
    print("Installing dependencies...")
    subprocess.check_call([sys.executable, "-m", "pip", "install", "-q", "moviepy", "numpy"])
    import moviepy
    from moviepy import (
        VideoClip, TextClip, CompositeVideoClip,
        AudioFileClip, concatenate_videoclips, vfx
    )
    import numpy as np

# Windows 字体路径
if sys.platform == 'win32':
    FONT_PATH = r'C:\Windows\Fonts\arial.ttf'
else:
    FONT_PATH = 'Arial'

# 视频配置
W, H = 1920, 1080
FPS = 30

# 配色（深空蓝 + 青绿强调）
BG = np.array([5, 7, 12], dtype=np.uint8)
SURFACE_1 = np.array([15, 19, 28], dtype=np.uint8)
SURFACE_2 = np.array([22, 29, 42], dtype=np.uint8)
ACCENT_CYAN = np.array([56, 189, 248], dtype=np.uint8)
ACCENT_GREEN = np.array([110, 231, 183], dtype=np.uint8)
TEXT_WHITE = np.array([240, 242, 245], dtype=np.uint8)
TEXT_GRAY = np.array([160, 170, 180], dtype=np.uint8)


def generate_ambient_audio(duration: float, output_path: str):
    """生成环境音（简单的 drone 和弦）"""
    print(f"Generating audio ({duration:.0f}s)...")
    
    sr = 44100
    t = np.linspace(0, duration, int(sr * duration))
    
    # 泛音叠加（A 小调）
    freqs = [55, 110, 165, 220, 330, 440]
    audio = np.zeros_like(t)
    
    for i, f in enumerate(freqs):
        env = 0.25 * (1 + 0.25 * np.sin(2 * np.pi * 0.08 * (i + 1) * t))
        phase = 2 * np.pi * f * t + 0.3 * np.sin(2 * np.pi * 0.03 * t)
        audio += env * np.sin(phase) / len(freqs)
    
    # 低频 sub-bass
    audio += 0.12 * np.sin(2 * np.pi * 27.5 * t)
    
    audio = audio / np.max(np.abs(audio)) * 0.65
    
    # 淡入淡出
    fade = int(sr * 1.5)
    audio[:fade] *= np.linspace(0, 1, fade)
    audio[-fade:] *= np.linspace(1, 0, fade)
    
    audio_int16 = (audio * 32767).astype(np.int16)
    
    # 写 WAV
    import struct
    with open(output_path, 'wb') as f:
        f.write(b'RIFF')
        f.write(struct.pack('<I', 36 + len(audio_int16) * 2))
        f.write(b'WAVE')
        f.write(b'fmt ')
        f.write(struct.pack('<I', 16))
        f.write(struct.pack('<H', 1))  # PCM
        f.write(struct.pack('<H', 1))  # mono
        f.write(struct.pack('<I', sr))
        f.write(struct.pack('<I', sr * 2))
        f.write(struct.pack('<H', 2))
        f.write(struct.pack('<H', 16))
        f.write(b'data')
        f.write(struct.pack('<I', len(audio_int16) * 2))
        f.write(audio_int16.tobytes())


def make_gradient_bg(color1, color2, horizontal=False):
    """创建渐变背景 numpy 生成器"""
    def make_frame(t):
        frame = np.zeros((H, W, 3), dtype=np.uint8)
        for i in range(H if not horizontal else W):
            ratio = i / (H if not horizontal else W)
            color = (color1 * (1 - ratio) + color2 * ratio).astype(np.uint8)
            if not horizontal:
                frame[i, :] = color
            else:
                frame[:, i] = color
        return frame
    return make_frame


def make_particle_bg(color_base, n_particles=40):
    """创建粒子背景（缓慢移动的光点）"""
    # 随机粒子位置和速度
    particles = []
    for _ in range(n_particles):
        x = np.random.randint(0, W)
        y = np.random.randint(0, H)
        vx = np.random.uniform(-15, 15)
        vy = np.random.uniform(-15, 15)
        size = np.random.randint(2, 6)
        brightness = np.random.uniform(0.3, 0.8)
        particles.append([x, y, vx, vy, size, brightness])
    
    def make_frame(t):
        frame = np.tile(color_base, (H, W, 1)).astype(np.float32)
        
        for p in particles:
            x = int((p[0] + p[2] * t) % W)
            y = int((p[1] + p[3] * t) % H)
            size, brightness = p[4], p[5]
            
            # 绘制光晕
            y1, y2 = max(0, y - size), min(H, y + size + 1)
            x1, x2 = max(0, x - size), min(W, x + size + 1)
            
            for dy in range(y1, y2):
                for dx in range(x1, x2):
                    dist = np.sqrt((dx - x)**2 + (dy - y)**2)
                    if dist <= size:
                        alpha = brightness * (1 - dist / size)
                        frame[dy, dx] = frame[dy, dx] * (1 - alpha * 0.6) + ACCENT_CYAN * alpha
        
        return frame.astype(np.uint8)
    
    return make_frame


def make_title_card(title: str, subtitle: str, duration: float) -> VideoClip:
    """创建标题卡"""
    bg = VideoClip(make_gradient_bg(BG, SURFACE_1), duration=duration)
    
    title_txt = TextClip(
        font=FONT_PATH, text=title, font_size=90, color='white'
    ).with_duration(duration).with_position('center').with_effects([vfx.FadeIn(0.5)])
    
    subtitle_txt = TextClip(
        font=FONT_PATH, text=subtitle, font_size=42, color=tuple(TEXT_GRAY)
    ).with_duration(duration).with_position(('center', H * 0.58)).with_effects([vfx.FadeIn(0.8)])
    
    return CompositeVideoClip([bg, title_txt, subtitle_txt])


def make_feature_card(title: str, subtitle: str, bullets: List[str], duration: float) -> VideoClip:
    """创建特性卡片（动态背景 + 文本叠加）"""
    # 动态粒子背景
    bg = VideoClip(make_particle_bg(SURFACE_1), duration=duration)
    
    # 标题
    title_txt = TextClip(
        font=FONT_PATH, text=title, font_size=64, color=tuple(ACCENT_CYAN),
        stroke_color=tuple(BG), stroke_width=2
    ).with_duration(duration).with_position(('center', 120)).with_effects([vfx.FadeIn(0.3)])
    
    # 副标题
    subtitle_txt = TextClip(
        font=FONT_PATH, text=subtitle, font_size=38, color=tuple(TEXT_WHITE),
        stroke_color=tuple(BG), stroke_width=1
    ).with_duration(duration).with_position(('center', 200)).with_effects([vfx.FadeIn(0.5)])
    
    layers = [bg, title_txt, subtitle_txt]
    
    # 特性列表
    if bullets:
        y_start = 360
        for i, bullet in enumerate(bullets):
            bullet_txt = TextClip(
                font=FONT_PATH, text=f"• {bullet}",
                font_size=30,
                color=tuple(TEXT_GRAY)
            ).with_duration(duration).with_position((120, y_start + i * 60)).with_effects([vfx.FadeIn(0.6 + i * 0.15)])
            layers.append(bullet_txt)
    
    return CompositeVideoClip(layers)


def make_end_card(duration: float) -> VideoClip:
    """结尾卡"""
    bg = VideoClip(make_gradient_bg(SURFACE_1, BG), duration=duration)
    
    logo_txt = TextClip(
        font=FONT_PATH, text="PrismaRev", font_size=80, color=tuple(ACCENT_GREEN)
    ).with_duration(duration).with_position(('center', H * 0.35)).with_effects([vfx.FadeIn(0.5)])
    
    tagline = TextClip(
        font=FONT_PATH, text="Modern · Mobile-First · Modular",
        font_size=36, color=tuple(TEXT_GRAY)
    ).with_duration(duration).with_position(('center', H * 0.50)).with_effects([vfx.FadeIn(0.8)])
    
    github = TextClip(
        font=FONT_PATH, text="github.com/YourUsername/PrismaRev",
        font_size=32, color=tuple(ACCENT_CYAN)
    ).with_duration(duration).with_position(('center', H * 0.62)).with_effects([vfx.FadeIn(1.0)])
    
    return CompositeVideoClip([bg, logo_txt, tagline, github])


def main():
    output_dir = Path(__file__).parent.parent / "target" / "showcase"
    output_dir.mkdir(parents=True, exist_ok=True)
    
    audio_path = output_dir / "ambient.wav"
    video_path = output_dir / "prismarev_showcase.mp4"
    
    print("=" * 60)
    print("PrismaRev Showcase Video Generator")
    print("=" * 60)
    
    # 场景定义
    scenes = [
        ("title", "PrismaRev", "A Modern Mobile-First Game Engine", [], 4.0),
        
        ("feature", "Mobile-First Architecture",
         "Optimized for TBDR GPUs · Vulkan Powered",
         [
             "Tile-based deferred rendering",
             "Transient attachment optimization",
             "Automatic bandwidth reduction",
             "Android + Desktop unified pipeline"
         ], 10.0),
        
        ("feature", "Modular Render Graph",
         "Extensible Pass-Based Design",
         [
             "RenderPassNode architecture",
             "Automatic resource barriers",
             "Pluggable feature passes",
             "Runtime capability detection"
         ], 10.0),
        
        ("feature", "Advanced Lighting",
         "PBR · IBL · Cascaded Shadows",
         [
             "Physically-based materials",
             "Image-based lighting (PMREM)",
             "Cascaded shadow maps",
             "Baked probe-volume GI (SH)"
         ], 10.0),
        
        ("feature", "Data-Oriented ECS",
         "High-Performance Entity Component System",
         [
             "Cache-friendly archetype queries",
             "Reflection-driven serialization",
             "Component versioning",
             "Cross-platform scenes (RSCN v4)"
         ], 10.0),
        
        ("feature", "Asset Pipeline",
         "Offline Cooker · Runtime Pak Loading",
         [
             "glTF 2.0 mesh import",
             "Texture compression (BC7/ASTC)",
             "Scene packaging (.pak format)",
             "Hot-reload ready architecture"
         ], 10.0),
        
        ("feature", "Web-Based Editor",
         "Tauri 2 + Vue 3 · Live Editing",
         [
             "Schema-driven component inspector",
             "Drag-and-drop hierarchy",
             "Prefab system with instance links",
             "Field-level undo/redo merging"
         ], 10.0),
        
        ("feature", "Bindless Everything",
         "Modern Descriptor Management",
         [
             "Descriptor indexing (Vulkan 1.2)",
             "Dynamic material batching",
             "GPU-driven rendering ready",
             "Legacy descriptor fallback"
         ], 9.0),
        
        ("feature", "Slang Shaders",
         "Cross-Platform Shader Compilation",
         [
             "Unified shader language",
             "Reflection-driven bindings",
             "Auto-generated Rust structs",
             "SPIR-V output with validation"
         ], 9.0),
        
        ("feature", "Built-In Automation",
         "Screenshot · MCP · CDP Integration",
         [
             "PRISMREV_SCREENSHOT env capture",
             "MCP server for editor automation",
             "CDP-driven UI testing",
             "Deterministic frame capture"
         ], 9.0),
        
        ("end", "", "", [], 5.0),
    ]
    
    # 1. 生成音频
    print("\n[1/3] Generating audio...")
    total_duration = sum(s[4] for s in scenes)
    generate_ambient_audio(total_duration, str(audio_path))
    
    # 2. 创建视频片段
    print("\n[2/3] Composing video...")
    clips = []
    
    for scene_type, title, subtitle, bullets, dur in scenes:
        print(f"  - {scene_type}: {title or '(end card)'}")
        
        if scene_type == "title":
            clip = make_title_card(title, subtitle, dur)
        elif scene_type == "feature":
            clip = make_feature_card(title, subtitle, bullets, dur)
        elif scene_type == "end":
            clip = make_end_card(dur)
        else:
            continue
        
        clips.append(clip)
    
    # 3. 拼接并输出
    print("\n[3/3] Rendering video...")
    final = concatenate_videoclips(clips, method="compose")
    
    if audio_path.exists():
        audio = AudioFileClip(str(audio_path))
        final = final.with_audio(audio)
    
    print(f"\nWriting to {video_path}...")
    final.write_videofile(
        str(video_path),
        fps=FPS,
        codec='libx264',
        audio_codec='aac',
        preset='medium',
        bitrate='6000k',
        threads=4
    )
    
    print("\n" + "=" * 60)
    print(f"[SUCCESS] Video created: {video_path}")
    print(f"  Duration: {final.duration:.1f}s")
    print(f"  Resolution: {W}x{H} @ {FPS}fps")
    print("=" * 60)


if __name__ == '__main__':
    main()
