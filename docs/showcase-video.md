# PrismaRev 展示视频

本项目包含自动化生成的引擎展示视频。

## 生成的内容

### 视频：`target/showcase/prismarev_showcase.mp4`

- **时长**：约 96 秒（1分36秒）
- **分辨率**：1920×1080 @ 30fps
- **编码**：H.264 (libx264) + AAC 音频

### 视频结构

1. **标题卡**（4秒）
   - PrismaRev 品牌标题
   - "A Modern Mobile-First Game Engine" 副标题

2. **特性展示片段**（10个场景，每个 9-10 秒）
   - **Mobile-First Architecture** — TBDR 优化、transient attachment、带宽优化
   - **Modular Render Graph** — RenderPassNode 架构、自动屏障、可插拔 pass
   - **Advanced Lighting** — PBR 材质、IBL、CSM 阴影、烘焙 GI
   - **Data-Oriented ECS** — archetype query、反射序列化、RSCN v4
   - **Asset Pipeline** — glTF 导入、纹理压缩、.pak 打包
   - **Native Rust Editor** — egui + ash 零拷贝采样、split-tree docking、反射检查器
   - **Bindless Everything** — descriptor indexing、动态批处理
   - **Slang Shaders** — 统一着色器语言、反射驱动绑定
   - **Built-In Automation** — 本地 TCP 控制通道、MCP 服务器、原生窗口与离屏抓图

3. **结尾卡**（5秒）
   - PrismaRev logo
   - "Modern · Mobile-First · Modular" 标语
   - GitHub 链接

### 视觉设计

**配色方案**：
- 基础背景：深空蓝 (#05070C → #0F131C)
- 强调色：青绿色 (#38BDF8, #6EE7B7)
- 文字：白色 (#F0F2F5) + 灰色 (#A0AAB4)

**动效**：
- 粒子背景（缓慢移动的光点）
- 文本淡入效果（分层级联）
- 渐变背景过渡

### 音乐

程序生成的 ambient drone 音乐：
- 多层泛音叠加（A 小调和弦）
- 低频 sub-bass rumble
- 1.5 秒淡入淡出

## 使用方法

### 生成视频

```bash
python scripts/showcase_video_simple.py
```

输出位置：`target/showcase/prismarev_showcase.mp4`

### 依赖

自动安装：
- `moviepy` — 视频合成
- `numpy` — 数学运算

### 系统要求

- Python 3.8+
- FFmpeg（moviepy 会自动下载 imageio-ffmpeg）
- 约 2GB 临时空间

## 自定义

编辑 `scripts/showcase_video_simple.py`：

### 修改配色

```python
# 配色（深空蓝 + 青绿强调）
BG = np.array([5, 7, 12], dtype=np.uint8)
ACCENT_CYAN = np.array([56, 189, 248], dtype=np.uint8)
```

### 添加/修改场景

在 `main()` 函数的 `scenes` 列表中添加：

```python
("feature", "你的标题",
 "副标题",
 [
     "特性 1",
     "特性 2",
     "特性 3"
 ], 10.0),  # 时长（秒）
```

### 调整分辨率

```python
W, H = 1920, 1080  # 修改为你需要的分辨率
FPS = 30           # 帧率
```

## 技术细节

### 背景生成

- **渐变背景**：numpy 逐行生成颜色插值
- **粒子背景**：40 个随机粒子，每个有独立的位置、速度、大小和亮度
- 粒子移动速度：15px/s（缓慢移动）

### 音频合成

- 采样率：44.1kHz
- 频率：55Hz (A1) 及其泛音（110, 165, 220, 330, 440Hz）
- 包络调制：0.08Hz 慢速 LFO
- Sub-bass：27.5Hz (A0)

### 渲染参数

```python
codec='libx264'      # H.264 编码
audio_codec='aac'    # AAC 音频
preset='medium'      # 编码预设（速度/质量平衡）
bitrate='6000k'      # 6Mbps 视频比特率
threads=4            # 并行编码线程
```

## 未来扩展

可能的改进方向：

1. **实时截图集成**
   - 使用 `PRISMREV_SCREENSHOT` 捕获引擎实际渲染画面
   - 当前为纯动效展示

2. **更复杂的动效**
   - 代码雨效果（Matrix 风格）
   - 3D 线框动画
   - 着色器特效叠加

3. **音乐升级**
   - MIDI 合成
   - 真实乐器采样
   - 动态音乐（根据场景变化）

4. **多语言版本**
   - 中文/英文字幕切换
   - 配音支持

## 许可

本视频生成脚本与 PrismaRev 引擎使用相同的许可证（Apache 2.0 / MIT dual license）。

生成的视频可用于项目宣传、技术演讲、教学等用途。
