# 代码与着色器命名及内容抽象规范

本文档是 PrismaRev 代码库中 **Rust 源码与 Slang 着色器** 的内容抽象与命名契约指南。

核心宗旨：**文件名即抽象职责。拒绝名不副实、拒绝模式名词滥用、消除图形学缩写冲突，并在 1.0 前坚决落实零兼容垃圾代码原则。**

---

## 🎨 一、着色器体系抽象与命名规范

着色器位于 `assets/shaders/`，源码使用 Slang 语言（`slang/`），经 `compile.sh` 编译为 SPIR-V（`.spv`），并通过 `xtask shader-bindgen` 导出强类型 Rust 绑定（`crates/prism-render/src/shader_bindings/`）。

### 1. 文件角色二元划分

着色器文件根据其内容抽象严格划分为两类，禁止混合定义：

| 类别 | 角色定义 | 包含内容 | 产物形态 | 命名规范 |
|---|---|---|---|---|
| **Entry Shaders**<br>（管线入口着色器） | 对应具体的 RenderPass 或 Compute 任务 | 包含 `[shader("vertex")]`、`[shader("fragment")]` 或 `[shader("compute")]` | 产出 `<name>.<stage>.spv`<br>及 `reflection/<name>.json` | `<pass_name>.slang`<br>（如 `gizmo.slang`, `post.slang`） |
| **Module Libraries**<br>（共享算法与结构库） | 纯算法求值、数据结构定义或工具函数 | `struct`、公共常量、`import` 模块，**无入口点** | **不生成独立 .spv**，仅供其它 shader `import` | `<concept>.slang`<br>（如 `common.slang`, `probe_volume.slang`） |

### 2. 图形学缩写去歧义铁律（Disambiguation）

在渲染管线中，多个不同领域的技术常共享类似缩写，必须在抽象命名中彻底消除歧义：

| 领域技术 | 规范前缀 / 缩写 | 严禁与歧义写法 | 理由与边界 |
|---|---|---|---|
| **光线追踪 / 路径追踪**<br>(Ray / Path Tracing) | `pt_` / `ray_` | 严禁缩写为 `rt_` | `pt_render.slang`、`pt_reference.slang`。<br>`pt_` 专指路径积分与光线查询。 |
| **离屏渲染纹理**<br>(Render Texture) | `rtt_` / `render_texture_` | 严禁简写为 `rt_` | 历史上的 `rt_render` / `rt_preview` 极易被误解为 Ray Tracing。<br>用于离屏纹理生产与消费测试时统一使用 `rtt_` 前缀。 |
| **探针全局光照**<br>(Probe-Volume SH) | `probe_` / `probe_volume_` | 严禁笼统命名为 `gi_` | 探针体积求值库（SH Radiance/Irradiance）只负责规则网格探针，命名必须具体到 `probe_volume`，不得冒充整体全局光照（GI）。 |
| **屏幕空间遮蔽**<br>(Screen-Space Ambient Occlusion) | `gtao` | 保持小写行业标准名 | 对应 Ground Truth Ambient Occlusion。 |
| **时间性抗锯齿**<br>(Temporal Anti-Aliasing) | `taa` | 保持小写行业标准名 | 对应 TAA Resolve Pass。 |

### 3. 管线阶段后缀与重复命名反模式

- **标准模式**：单一 Pass 的顶点与片段入口合并在一个 `<pass_name>.slang` 中，由 `compile.sh` 分别编译为 `<pass_name>.vert.spv` 与 `<pass_name>.frag.spv`。
  - 正例：`post.slang` → `post.vert.spv` + `post.frag.spv`
  - 正例：`gtao.slang` → `gtao.vert.spv` + `gtao.frag.spv`
  - 正例：`shadow_depth.slang` → `shadow_depth.vert.spv` + `shadow_depth.frag.spv`
- **反模式**：在基础文件名中硬编码阶段名，导致产出重复冗余的物理后缀。
  - 历史反模式：`mesh_vert.slang` → `mesh_vert.vert.spv`（`vert` 重复两次）
  - 历史反模式：`scene_frag.slang` → `scene_frag.frag.spv`（`frag` 重复两次）
  - **规范化演进路线**：前向主渲染管线抽象为 `pbr_forward.slang`，共享统一的 `PbrPush` 常量块，输出 `pbr_forward.vert.spv` 与 `pbr_forward.frag.spv`。

---

## 🦀 二、Rust 代码体系抽象与命名规范

### 1. 领域概念驱动 vs 设计模式名词

在系统和引擎层代码中，模块和文件名应直接表达其所代表的**领域模型与物理/逻辑实体**，严禁使用空泛的设计模式名词污染抽象：

- **拒绝设计模式后缀**：
  - ❌ `input_facade.rs`（把设计模式 Facade 当成业务核心名）
  - ✅ **`input_view.rs`**（准确抽象：当帧输入只读借用视口 `Input<'a>`）
- **拒绝口语化俚语，使用精准的技术抽象名词**：
  - ❌ `dummy.rs`（测试替身口语，模糊了真实 GPU 资源的意义）
  - ✅ **`placeholder.rs`**（准确抽象：在特性关闭时填充 Descriptor 的 1x1 常量可采样资源 `SolidImage`）
- **拒绝模糊的 `compat`，精准标识架构意图**：
  - ❌ `vulkan_compat.rs`
  - ✅ **`device_fallback.rs`**（准确抽象：当硬件未支持某项高级 Vulkan 扩展时的指令级降级调度分发）

### 2. 1.0 前零兼容逻辑与命名清洁

在 1.0 版本发布前，坚决贯彻“**旧版本兼容被明确视为冗余的垃圾代码**”硬规则：

1. **严禁引入与保留带有 `legacy_` 前缀的函数或接口**：
   - 彻底清除 `legacy_settings_path`、`legacy_manifest_path`、`parse_legacy_toml` 等回退逻辑；
   - 配置文件唯一源为 `assets/settings.ini`，场景清单唯一源为 `assets/scenes.json`。读端与写端只认最新格式，不编写回退兼容分支。
2. **严禁在公开 API 或内部模块中保留 `#[deprecated]` 垫片**：
   - 需要修改签名、数据结构或协议时，直接按最优设计进行破坏性重构，清理所有旧实现，不留任何过渡兼容包袱。

---

## 📋 三、命名与抽象审查速查清单（Checklist）

在新增或重构代码、着色器文件时，必须核对以下维度：

- [ ] **语义真实性**：该文件名是否精确描述了文件内部的核心数据结构与算法？有没有借大词（如把局部离线追踪叫 `gi_render`）？
- [ ] **缩写排他性**：若涉及图形学缩写，是否严格遵循了去歧义规则（`pt_` 光追 vs `rtt_` 离屏纹理）？
- [ ] **对称性与阶段解耦**：着色器是否由 Pass 逻辑名称统一驱动，避免在源码主名中拼接 `_vert` / `_frag`？
- [ ] **无模式名词污染**：模块名是否直接反映领域概念（如 `input_view`），而非设计模式名词（`input_facade`）？
- [ ] **无口语化与俚语**：底层资源是否使用了正式的技术实体名（如 `placeholder`），杜绝使用 `dummy`、`temp`、`fake`？
- [ ] **零兼容垃圾**：代码中是否不存在任何 `legacy_*`、`*_compat`、回退解析旧格式的代码？
