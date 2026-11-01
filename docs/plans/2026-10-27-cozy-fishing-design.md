# 2D 像素钓鱼游戏（Cozy Fishing）设计

> 状态：设计稿（M0 起点）
> 目标项目：`projects/fishing/`（独立 workspace，与 `projects/prisma` 同级）
> 引擎改动遵循 `docs/DESIGN.md` 的架构分层：新 API 先写规格，语义等价优先。

---

## 1. 目标与非目标

### M0 垂直切片（本阶段唯一交付目标）

一个**能玩 3 分钟**的单场景切片：

1. 俯视 2D 湖面，船在水上缓慢漂移；
2. 抛竿 → 浮标落水 → 等待 → 咬钩提示 → 收线小游戏 → 钓上鱼；
3. 钓到的鱼进入图鉴，HUD 显示提示与已钓数量；
4. 环境音（水声/鸟鸣）+ 抛竿/入水/收线音效；
5. 昼夜随时间缓慢变化（全局色调 + 天空渐变）。

**不做**：联网、多场景、任务系统、经济系统、骨骼动画、物理引擎接入。

### 非目标（明确排除，避免范围蔓延）

- 不做 C# 脚本系统。
- 不在 M0 做 3D 场景视口与地形笔刷（M2 再评估）。
- 不为像素画引入第二套 ECS；精灵仍是普通组件。

---

## 2. 分层契约（硬边界）

这条边界是本次工作的核心约束：**引擎只提供通用能力，玩法只存在于用户项目**。

| 层 | 位置 | 允许 | 禁止 |
|---|---|---|---|
| 引擎运行时 | `crates/prism-*` | 通用组件、通用系统、渲染通道、资源导入、音频/存档/时间等通用服务 | 出现任何鱼类、钓鱼、船、关卡、HUD 布局等具体玩法概念 |
| 编辑器数据层 | `crates/prism-editor-api` | 把注册表投影成属性面板 schema、文档读写 | 链接 engine/render/Vulkan |
| 编辑器 UI | `editor/`（Vue） | 面板、拖拽、预览 | 写死某个游戏的业务字段 |
| 用户项目 | `projects/fishing/` | 钓鱼状态机、鱼种数据、船只控制、HUD、关卡内容、音频清单 | 直接使用 `vk::*`、直接改 render graph、绕过 `ComponentRegistry` 挂组件 |

判定方法：**换一个游戏（比如种田游戏）还能原样复用吗？** 能 → 引擎；不能 → 用户项目。

组件命名沿用引擎现有词汇：`SpriteRenderer`、`Sprite`、正交相机、`AudioSource`、`TimeOfDay`。

---

## 3. 引擎侧缺口与 API 设计

### 3.1 2D 渲染管线（最大缺口）

现状：只有 3D 前向 PBR（`forward_pass.rs` + `scene_frag.slang`），`Camera` 已支持正交投影
（`orthographic` / `orthographic_size`，见 `crates/prism-scene/src/components.rs`），
但没有精灵、没有 2D 排序、没有像素完美采样。

#### 坐标与相机约定

沿用项目既有约定（右手系、相机看 −Z、Vulkan y-flip）：

- 2D 场景相机放在 **+Z 朝 −Z**，精灵位于 `z ≈ 0`；
- `z` 越大越靠近相机（与 3D 一致）；
- 世界单位 ↔ 像素由 **PPU（pixels per unit）** 决定：
  `world_size = texture_size / pixels_per_unit`；
- 像素完美：`orthographic_size = 渲染目标高度 / (2 * ppu)`，采样器用 `NEAREST`。
- **内部渲染分辨率**（M0.2 必做）：像素画要按 480×270 / 640×360 渲染再整数倍放大，
  否则 32px 精灵在 1080p 上只占屏高 3%。方案：`RenderSettings.internal_resolution: Option<[u32; 2]>`
  —— ForwardPass 的 HDR 目标按内部分辨率分配，PostPass 用 nearest 放大到 swapchain。
  在实现前，先用较大的占位精灵验证管线（不影响排序/采样正确性）。

#### 新组件 `SpriteRenderer`（`prism-scene`）

```rust
pub struct SpriteRenderer {
    pub sprite_path: String,      // 资源路径
    pub frame_size: [u32; 2],     // [0,0] = 整张纹理；否则为精灵表单帧尺寸
    pub frame_index: u32,         // 由 SpriteAnimation 或游戏代码驱动
    pub pixels_per_unit: f32,     // 默认 100
    pub pivot: [f32; 2],          // 0..1，默认 [0.5, 0.5]
    pub color: [f32; 4],          // 顶点色乘算，默认白
    pub flip_x: bool,
    pub flip_y: bool,
    pub sorting_layer: i32,       // 排序层编号
    pub order_in_layer: i32,      // 层内顺序
}
```

运行时 GPU 句柄放 `SpriteRef`（`prism-engine/src/scene/runtime_components.rs`，与 `MeshRef`/`MaterialRef` 同构）：

```rust
pub struct SpriteRef {
    pub texture: AssetTextureHandle,
    pub size: [u32; 2],           // 纹理像素尺寸（上传后回填）
    pub generation: u32,          // 0 = 未解析
}
```

#### 资源解析

`AssetResolveRequest` 增加 `texture: Option<TextureUploadInput>`，
`AssetResolveResult` 增加 `texture_handle: Option<AssetTextureHandle>`；
`AssetResolver::prepare_requests` 为持有 `SpriteRenderer` 且 `SpriteRef.generation == 0`
的实体投递请求。纹理走已有的 `TextureImporter`（PNG → RGBA8）与 bindless 槽。

#### `SpritePass`（`prism-render/src/sprite_pass.rs`）

- 单个 instanced draw：unit quad + per-instance SSBO
  `{ model: mat4, uv_rect: vec4, color: vec4, texture: u32, _pad }`；
- 采样走 **set 1 bindless**（与 `scene_frag.slang` 相同的布局），sampler 用 `SamplerType::Nearest`；
- 混合 `SRC_ALPHA / ONE_MINUS_SRC_ALPHA`，**关闭深度测试与写入**（2D 用 CPU 排序保证顺序）；
- 排序键 `(sorting_layer, order_in_layer, z, entity)`，在 `SceneReadView::sprite_items()` 里做稳定排序；
- 绘制目标：HDR 场景色（在 `PostPass` 之前），因此 `PostPass` 需要新增 **`tonemap_mode = 2`（None/passthrough）**
  以保证像素画调色板不被 ACES 改写；
- 2D 场景不创建 `Skybox` / `Gizmo` 实体，相机 `clear_flags = SolidColor`。

#### 2D 帧的 pass 裁剪（后续优化，不阻塞 M0）

`RenderMode` 增加 `Sprites2D`：跳过 shadow / GTAO / GI / PT / forward。M0 先接受「前向 pass 空跑」的成本，
在 `docs/plans` 里记为 M1 项。

### 3.2 精灵动画

```rust
pub struct SpriteAnimation {
    pub frames: u32,        // 精灵表横向帧数
    pub fps: f32,           // 0 = 手动
    pub looped: bool,
    pub playing: bool,
}
```

引擎系统 `sprite_animation_system` 每帧累加时间并写 `SpriteRenderer.frame_index`
（`SpriteAnimation` 是纯数据，不持有时间状态，时间放系统侧 `Resource` 以免污染场景格式）。

### 3.3 线渲染（鱼线 / 浮标轨迹）

```rust
pub struct LineRenderer {
    pub points: Vec<[f32; 3]>,   // 世界空间折线
    pub color: [f32; 4],
    pub width: f32,              // 世界单位
    pub sorting_layer: i32,
}
```

`LineRenderer` 在 CPU 侧三角化为**互不重叠**的笔触（2D 场景下即 XY 平面上的加宽折线，
支持 miter/bevel 拐点与 butt/square/round 端点；#604），
并入 `SpritePass` 的实例缓冲（同一 pipeline，白色 1×1 纹理）。避免新增 pass。

### 3.4 音频

现状：`prism-audio` 只有 2D 一次性播放，无循环、无通道、无空间化。

新增（M0 只做最小集）：

- `AudioData` 循环播放：`play_looping` / `AudioClip { looped }`；
- 命名通道（`master` / `music` / `ambience` / `sfx`）与独立音量；
- 2D 空间化：按 `x` 距离做左右声像 + 距离衰减（像素游戏够用，不做 HRTF）。

### 3.5 时间与昼夜

```rust
pub struct TimeOfDay {
    pub day_length_seconds: f32,   // 0 = 冻结
    pub time: f32,                 // 0..1，0 = 午夜，0.25 = 日出，0.5 = 正午
    pub start_time: f32,
    pub paused: bool,
}
```

引擎系统 `time_of_day_system` 推进 `time` 并驱动：

- `DirectionalLight`（3D 场景）的方向与颜色；
- 2D 场景的全局色调：通过 `FrameUBO` 增加 `tint` 字段，或由 `PostPass` 的 color grade 实现（M0 用后者）。

### 3.6 存档

```rust
pub struct SaveGame { /* 版本化 JSON，位于用户数据目录 */ }
impl SaveGame {
    pub fn load(name: &str) -> Self;
    pub fn save(&self, name: &str) -> Result<()>;
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T>;
    pub fn set<T: Serialize>(&mut self, key: &str, value: T);
}
```

只做键值 + 版本迁移钩子；不碰场景序列化（那是 `scene_state.rs` 的职责）。

### 3.7 输入

已有只读 `Input` 门面（`input_facade.rs`），M0 够用；手柄/虚拟摇杆后置。

---

## 4. 编辑器侧

| 项 | M0 需要 | 说明 |
|---|---|---|
| 组件检查器 | 自动 | 组件注册进 `ComponentRegistry` 后 schema 自动出现 |
| 纹理资产识别 | 是 | 资产管理器需列出 `.png` 并允许拖到 `sprite_path` 字段 |
| 精灵预览缩略图 | 是 | 检查器里显示贴图预览 + 帧索引滑条 |
| 2D 场景视口 | 否（M2） | M0 用 Play 模式摆场景 |
| 排序层命名 | 否 | M0 用数值 |

编辑器仍不链接引擎：精灵预览通过 `prism-editor-api` 读文件 + 前端 `<img>` 呈现，不引入渲染依赖。

---

## 5. 用户项目 `projects/fishing/`

```
projects/fishing/
  Cargo.toml            # 独立 workspace，依赖 prism-app / prism-engine
  src/
    main.rs             # 桌面入口（build_app().run()）
    lib.rs              # build_app + Android android_main
    game/
      mod.rs
      fishing.rs        # 抛竿/等待/咬钩/收线状态机
      boat.rs           # 船体漂移与相机跟随
      fish_db.rs        # 鱼种数据（JSON 驱动）
      hud.rs            # 提示、图鉴计数
      ambience.rs       # 环境音与昼夜联动
  assets/
    sprites/            # 像素图（PNG）
    audio/              # wav/ogg
    scenes/lake.scene.json
    fish.json
```

玩法状态机（仅用户项目）：

```
Idle ──按下抛竿──> Casting ──落水──> Waiting ──咬钩──> Biting
  ^                                                      │
  └────────── 收线成功/失败 <── Reeling <── 按下收线 ──────┘
```

---

## 6. 里程碑

| 里程碑 | 内容 | 出口标准 |
|---|---|---|
| **M0.1** | 精灵管线：`SpriteRenderer` + `SpriteRef` + `SpritePass` + `sprites.slang` + tonemap None | 场景里能渲染一张 PNG，正交相机，nearest 采样 |
| **M0.2** | 工程脚手架：`projects/fishing` + 湖面场景 + 船与相机跟随 | 双击能跑，船能在水面移动 |
| **M0.3** | 线渲染 + 钓鱼状态机 + HUD | 能完成一次「抛竿→上鱼」闭环 |
| **M0.4** | 音频循环/通道 + 昼夜 | 有环境音，天色随时间变化 |
| **M0.5** | 存档 + 图鉴 | 重启后图鉴保留 |
| **M1** | 内容扩充、2D pass 裁剪、精灵表编辑、更多鱼种 | 20+ 鱼种，稳定 60fps |

---

## 7. 美术规格

M0 采用 **640×360 虚拟分辨率 + 多层视差 + 每层 4 个时段变体**，而不是「一张大背景图 +
全局色调」。内部渲染分辨率因此从 M1 提到 M0 必做项。

| 项 | 规格 |
|---|---|
| 背景层 | 宽 500–4000 px、高 300–430 px，按 ×0.04–×1.05 的视差系数由远到近排布 |
| 鱼 | 分级尺寸 14 / 16 / 20 / 30 / 42 px，占视口高度 4–12%，必须低分辨率渲染再放大 |
| 图层 | 每层独立 X/Y 视差；命名 `<层名>@<dawn|day|dusk|night>` |
| 昼夜 | 每层 4 份贴图整层替换，不做全局调色 |
| 鱼种 | `<Species>_<Tier>` + `<Species>_UI`；游动两帧 |
| 音频 | 四条总线：Master / Music / UI / WorldSFX |
| 字体 | LanaPixel（CC-BY 4.0）、Silkscreen，致谢中署名 |

### 7.1 背景贴图按裁切块摆放

每张背景贴图是关卡里一块美术的矩形**裁切**，硬边就在贴图边界上。摆放时必须把被裁的
一侧推到屏幕外，否则画面里会看到一条笔直的竖边：

| 判据（alpha 图） | 含义 | 摆放 |
|---|---|---|
| 最左一列整列不透明 | 左侧被裁 | 左边贴屏幕左缘（左缘 x < 0） |
| 最右一列整列不透明 | 右侧被裁 | 右边贴屏幕右缘（右缘 x > 视口宽） |
| 底边整行不透明 | 底边被裁 | 底边沉到水线以下，由不透明水面遮住 |
| 四边都有透明 | 完整道具（岩石/云/鱼） | 随意摆放 |

- 反过来用也会踩坑：把这些山体摆在画面中间，裁切边就露成「矩形」——曾误判为
  **精灵 alpha 丢失**，实际 alpha 一路完好（RTEX 里 26% 全透明像素仍在，单层单独渲染
  镂空也正确），只是几何位置错。
- 每层的真实 transform 在关卡的 MonoBehaviour 里（无 typetree，`m_Name` 之外读不出），
  所以 `x/y` 只能按上面这条规则反推，不能指望精确复刻。
- 水面必须是**不透明**的深色块：半透明水面压在亮天空上会变成灰白色雾带，看起来像
  alpha 坏了。

## 8. 风险与取舍

1. **前向 pass 空跑成本**：M0 接受，M1 加 `RenderMode::Sprites2D`。
2. **线性空间混合**：精灵在 HDR 线性空间混合与经典 2D 引擎（sRGB 混合）观感不同；
   M0 先接受，若调色偏差明显再引入独立 2D 目标。
3. **无精灵资产格式**：M0 用「路径 + 帧尺寸 + 帧索引」，不做 `.sprite.json`；
   等编辑器需要切片时再引入（并写迁移）。
4. **动画**：M0 只做帧动画；角色/狗的骨骼动画是独立大工程，不进 M0。
