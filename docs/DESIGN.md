# PrismaRev 设计目标与设计原则

> 本文件是 PrismaRev 的**权威设计蓝图（source of truth for intent）**。
> 代码会变，但这里的约束不可违背。新增渲染特性、pass、平台支持前，
> 先对照本文确认方向一致。具体的特性设计见 `docs/` 下的分文档
> （如 `mobile-raytracing-gi-design.md`）。

## 1. 一句话定位

为**移动端（Android / TBDR GPU）优先**设计的、可扩展的**模块化渲染引擎**；
一套统一管线覆盖桌面与移动端，按运行时探测到的 Vulkan 能力与扩展**自动降级 / 升级**，
不携带任何历史单体渲染器的包袱。

## 2. 三条核心设计目标

### 2.1 移动端 TBDR 优化优先，抛弃历史包袱

- **TBDR 友好**是首要约束，不是事后优化：
  - pass 之间用 **transient attachment / lazy allocation**（移动端 tile memory），
    避免全分辨率中间纹理在系统内存来回搬运。
  - 严格遵守 **load/store op** 最小化；中间 RT 能 `DONT_CARE` 就 `DONT_CARE`。
  - 避免跨 tile 的随机全局内存访问（bindless 大数组的访问模式要可控）。
  - 半分辨率阴影 / GI 等重 pass 默认降采样（见 `RayQueryPass` 的 `scale = 0.5`）。
- **不回移植旧架构**：不存在"为了兼容旧 renderer 将就"的妥协。
  旧的单体 `renderer.rs`（995 行）已被拆掉，新代码一律走 **RenderGraph + pass 节点**。
  任何"临时塞进 legacy_renderer"的写法都是 bug（legacy 仅作过渡，目标是彻底删除）。

### 2.2 PC / 移动端 / 全平台统一的可扩展模块化渲染管线

- **一套管线，多端运行**：桌面（x86_64 Vulkan）与移动端（aarch64 Vulkan）走**完全相同的
  RenderGraph 定义**，差异只来自运行时能力探测（见 2.3）与可选特性开关，**不写平台分支**。
- **模块化 = pass 即节点**：每个渲染阶段（GBuffer / Shadow / RayQuery / SHARC GI /
  Lighting / Post）是独立的 `RenderPassNode`，通过 `RenderGraphBuilder` 组合。
  新增特性 = 新增一个 pass 节点，不改动既有节点。
- **特性可开关、可降级**：光追、GI、阴影、调试视图等都由 `RenderSettings` 控制。
  中端 GPU 撑不住时关 RT / 把 GI 切到 Off 即可，**架构本身不因此变形**。
- **资源与渲染解耦**：源格式（glTF / PNG / HDR）由 `prism-cooker` 离线导入并 cook 成
  RTEX / RMES / RSCN，运行时经 `prism-asset` 的 `.pak` + `ResourceManager` 读取；GPU 侧的
  `RenderMaterialManager` / `BindlessTextureTable` 在 `prism-render`。引擎 crate 不依赖
  具体资源格式。

### 2.3 运行时自动判断 Vulkan 版本与扩展支持

- 引擎启动时**探测** `VkPhysicalDevice` 的 Vulkan 版本、可用扩展、可用的
  descriptor-indexing / ray-query / dynamic-rendering 等特性，据此决定启用哪条路径。
- **能力驱动降级**，不靠 `#[cfg(target_os)]` 平台硬编码：
  - 有 `VK_KHR_ray_query` → 走 RayQuery 软阴影 / 反射；否则退化为 raster 硬阴影。
  - 支持 descriptor indexing → 走 bindless SRV 表；否则退化为传统 descriptor set。
  - 高版本 Vulkan 可用 dynamic rendering / transient 附件 → 自动采用以省带宽。
- 探测逻辑集中、可测试，不被散落到各 pass 里。

> **阴影实现状态（2026-08-21）**：当前已实现**单张光栅化阴影贴图**
> （`ShadowMapPass` 深度预渲染 + `ForwardPass` 用 comparison sampler 采样，
> 见 `shaders/slang/shadow_depth.slang` / `scene_frag.slang`）。`RenderSettings::
> shadow_mode` 支持 `Auto`/`Raster`/`RayQuery`/`None`，由 `resolve_shadow`
> 按 `VK_KHR_ray_query` 能力自动选择。`ShadowMapPass::SHADOW_CASCADE_COUNT = 1` 为预留。
>
> **计划（CSM）**：级联阴影贴图（Cascaded Shadow Maps）尚已实现，仅单张
> 固定范围正交阴影。下一步扩展在 `ShadowMapPass` 内按相机视锥切片拆成多张级联，
> 并在 `scene.frag.slang::sample_shadow` 中按距离选择级联 —— 已预留常量与接口，不在本次 MVP 范围。

## 3. 派生约束（从目标推出来的硬规则）

| 规则 | 理由 |
|------|------|
| 不写 `target_os` / `target_arch` 平台分支决定渲染路径 | 2.2 / 2.3：平台差异由能力探测吸收 |
| 新渲染特性必须实现为 `RenderPassNode`，不得塞进 legacy renderer | 2.1：抛弃历史包袱 |
| 中间 RT 默认 `DONT_CARE` store + transient/lazy 分配 | 2.1：TBDR 带宽 |
| 重 pass（阴影/GI/反射）默认半分辨率 | 2.1：移动端带宽/算力 |
| 所有跨端布局（push constant、UBO、SSBO）显式 padding 并验证 | 全平台一致 ABI |
| 阴影 / GI / RT / 调试视图由 `RenderSettings` 统一开关 | 2.2：可降级不形变 |
| 源格式（glTF / 纹理）由 `prism-cooker` 在构建期导入/烘焙，运行时经 `prism-asset` 读 `.pak`，引擎不直读源文件 | 2.2：解耦 |

## 4. 当前架构落点（与目标的对应关系）

| 设计目标 | 当前落地 |
|----------|----------|
| 模块化管线 | `prism-render/src/render_graph.rs`（`RenderPassNode` 图）+ `forward_pass.rs` / `shadow_map_pass.rs` / `skybox_pass.rs`。**现状（2026-08-21）**：`RenderGraph::execute()` 统一驱动四个 pass（`ShadowMapPass` -> `ForwardPass` -> `GtaoPass` -> `PostPass`，按注册顺序线性执行）。passes 通过 `read_usage` / `write_usage` 声明图边依赖，graph 据此自动插入跨 pass 的 `vkCmdPipelineBarrier`（layout cache 按 `(handle, image_index)` 跨帧持久，`recreate_swapchain` 时 `reset_layouts`）。跨帧延迟边（GTAO 双缓冲 AO 回喂）与 swapchain->`PRESENT_SRC_KHR` 保留手动，标注为图边界特例。环检测已实现（`validate_edges`），执行顺序不重排（接线顺序见 `GraphRenderer::new`）。`GraphRenderer::prepare` 已接入 `render_system`/`render_runner` 五阶段链（不再空转），资源生命周期区间已声明，TBDR 内存 aliasing 待下一步扩展。|
| bindless / 全平台统一 | `prism-render/src/bindless.rs`（分离 SRV + 全局 sampler 表） |
| 资源管理解耦 | 职责拆成三个 crate：`prism-cooker`（Import→Cook，仅构建期）、`prism-scene`（组件 schema + RSCN v4）、`prism-asset`（Package→Runtime，`.pak` + `ResourceManager`）。引擎侧 `GpuAssetResolver` 持 `ResourceManager`，用 `load_with_deps` 从 `.pak` 懒加载，再经 `GraphRenderer::apply_asset_requests` 在渲染线程上传；运行时链路不依赖 cooker。 |
| 移动端 GI | **Baked probe-volume GI**（2 阶 SH，9 系数 × RGB，`R32G32B32A32_SFLOAT` 3D texture；float16 仅是后续可选压缩），非实时 SHARC。设计见 §6。SHARC 实时 slang 已移除，不再恢复（移动端跑不动每帧 ray 填 cache）。|
| 阴影 / RT | 光栅化阴影贴图：`ShadowMapPass`（深度预渲染，见 `shadow_depth.slang`）+ `ForwardPass`（comparison sampler 采样，见 `scene_frag.slang`） |
| 能力探测 | `prism-render/src/capabilities.rs`（集中探测，扩展中） |
| 帧生命周期 | `GraphRenderer` 已提供 `begin_frame` → `prepare` → `execute` → `present` → `end_frame` 阶段（2026-08-21 起 `prepare` 已在 `render_system` 与 `render_runner::render_one_frame` 中实际调用，不再空转）；旧 `render()` 仅作为兼容门面。`prepare` 当前实现脏标记日志与输入校验，下一步扩展在此调度批量上传。细粒度 Runtime/Plugin 拆分仍待 PR-L2。 |
| 场景同步（CPU→GPU） | **阶段性实现（2026-08-21）**。`DirtyRouter` 已扩展 `exposure`/`pt_lights`/`draw_count` 标志并接入 `prepare`，`FramePacket` 抽取与脏检测闭环已打通；但 `RenderTexture/Mesh/Material/Instance` 的统一 `DirtyDispatchPlan` 与 `SceneReadView` 仍待 PR-S2/S3。当前仍由 `GpuAssetResolver` 点对点触发上传，`prepare` 批同步为预留。设计见 §9。 |
| 只读场景视图 | `prism-engine::scene::SceneReadView` 已覆盖环境资源路径和扁平 `DrawItem` 提取；相机/灯光快照仍在逐步迁移。 |

## 5. 反目标（明确不做什么）

- **不**维护兼容旧单体 renderer 的兼容层。
- **不**为桌面 / 移动写两套管线或两套 shader 主路径。
- **不**用平台宏代替能力探测来决定渲染特性。
- **不**引入未经验证、不可降级的"全开"硬依赖（如强制要求某个 Vulkan 扩展）。

---

## 6. Baked GI 与 RenderGraph 重构（规划）

> 本节规格驱动 `RenderGraph` 的接口设计，避免"先空改架构再被 GI 打脸第二遍"。
> GI 不是独立 pass，是 `ForwardPass` 内部的一个 diffuse 间接光采样分支；但它反过来
> 要求图能区分**三类资源生命周期**，这是重构的核心约束。

### 6.1 资源分类与生命周期（图必须显式建模三类）

| 类别 | 生命周期触发 | 示例 | 销毁责任 |
|------|--------------|------|----------|
| **场景级（scene）** | 场景/关卡加载/卸载时 | probe volume 3D texture、IBL env cube、材质表 | 场景管理器（非 swapchain 回调） |
| **交换链级（swapchain）** | swapchain recreate（resize / 旋转 / 设备丢失恢复） | ForwardPass 的 HDR color / depth / normal MRT，**按 swapchain image 数**分配 | 图的 recreate：先 drop 这些资源的 framebuffer，再 recreate swapchain（见 lessons §21、§29 的 device-lost 警告） |
| **帧级（frame）** | 每帧 in-flight | AO 双缓冲（GTAO 读上一帧、写本帧，1-frame latency）、per-frame-in-flight descriptor set | 图的帧循环（按 `frame_index`，不是 `image_index`） |

**关键陷阱（提前标出）**：probe volume 3D texture 是**场景级**，绝不能挂到 swapchain recreate 回调上。换关卡才换，resize 不动。图需要一个 `SceneScope` 资源表，独立于 `SwapchainScope` / `FrameScope`。

### 6.2 RenderGraph 接口修订

目标：资源句柄声明 + 图托管 + 自动屏障 + 生命周期范围。

- `RenderPassNode::setup(&mut self, graph: &mut RenderGraphBuilder, settings)` — 声明
  读/写哪些 `ResourceHandle`（图边）**及资源的作用域**（scene / swapchain / frame），
  物理资源由图在 `allocate_resources` / `import` 时统一建。
- `RenderPassNode::execute(&mut self, ctx: &RenderContext, resources: &GraphResources)`
  — 只拿 command buffer + 已绑定/可查询的资源句柄，**不**自己管 framebuffer 生命周期。
- 资源句柄是图内 ID（`ResourceHandle(u32)`），pass 不持有裸 `vk::Image` / `vk::Framebuffer`。
- 图在编译期做拓扑排序；图在运行时逐帧推导并插入 **自动屏障**——
  `RenderGraphExecutor` 维护逐资源的 `ResourceStateTracker`，跟踪每个 image 的
  `layout` / `access` / `stage`，在 pass 切换时自动插入 `vkCmdPipelineBarrier`，
  不再依赖 pass 内显式手工 barrier。状态追踪按 `(handle, image_index, layer_count)` 分帧，
  swapchain recreate 时重置全部 tracked state。
  第一阶段可先过渡：手动依赖表排顺序 + pass 内显式 barrier，行为不变后再启用自动屏障。
- `ShadowMapPass` 已正确实现 `RenderPassNode`，作为参照，**不动**。

### 6.3 Pass 拓扑（重构后）

```
ShadowMapPass → ForwardPass → GtaoPass → PostPass
   (图边)         (图边)       (图边)
```

- GI 不是独立 pass：是 `ForwardPass` 内部一个 `if (flag(PBR_FLAG_GI))` 分支，采样 probe volume。
- 图边契约：
  - `ShadowMapPass` 写 `shadow_map`（depth） → `ForwardPass` 读。
  - `ForwardPass` 写 `hdr_color` / `normal_mrt` / `depth`（交换链级，按 image_index） →
    `GtaoPass` 读 depth+normal；`PostPass` 读 hdr_color。
  - `GtaoPass` 写 `ao[frame]`（帧级双缓冲） → `ForwardPass`（下一帧读，`ao[(frame+1)%2]`，1-frame latency）。
    **跨帧依赖由 `GtaoPass::setup` 声明"读上一帧 AO / 写本帧 AO"，图据此不把 GTAO 排在它自己读的那个 slot 前面**；首帧上游 view 为 null，shader 不采样（PBR_FLAG_AO 默认 off）。

### 6.4 Baked GI 数据规格（PR-5 更新）

- **SH 阶数**：2 阶，9 个系数 × RGB。每系数 `float32`（当前实现 `R32G32B32A32_SFLOAT`；下一步扩展可切 `float16` 省带宽，移动端带宽紧）。
- **SH 表示**：Probe volume 存储 **radiance SH**（非 irradiance SH）。Baker 不做 cosine 预卷积，仅对入射 radiance 做 Monte Carlo 积分投影到 SH 基。运行时通过两套求值路径区别使用：
  - **Diffuse** → `EvalSH9Irradiance()` 应用 Ramamoorthi & Hanrahan A_l 因子（A₀=π, A₁=2π/3, A₂=π/4），返回 E(n) = ∫ L(ω) max(0, n·ω) dω。调用方除以 π 得 Lambertian BRDF。
  - **Specular** → `EvalSH9Radiance()` 直接 SH 重建 L(ω)，输入分裂和（split-sum）近似代替 prefiltered env map。
  - 同一份 9 系数数据供两条路径使用，无需额外存储。
- **/π 一致性**：IBL irradiance cubemap 存储 E/π（cosine 加权平均），GI SH 存储 radiance（无 cosine）。运行时 diffuse 路径两者统一为 kd · albedo · E/π，消除旧实现中 GI 比 IBL 亮 π 倍的 bug。
- **Probe grid**：`origin: vec3`、`spacing: vec3`、`dims: ivec3`（grid 分辨率），经 cbuffer/UBO 传入 shader。
- **3D texture 打包**：每层一张 2D 切片（R32G32B32A32_SFLOAT），深度 = dims.z × 9
  （每系数一层 RGB）。采样用 integer `Load` + 手动三线性插值，**不用硬件 sampler**，防止系数层间串扰。
- **烘焙工具**（`prism-build-pipeline bake-gi` 子命令，不进运行时）：多 bounce 路径追踪（3 bounce + Russian roulette）对每条 Fibonacci 球面方向做完整 bounce 链，写出 `assets/gi/probe_volume.bin`（`prism-render::probe_loader::save_probe_volume`）；读回同样在 `prism-render`（`probe_loader::load_probe_volume` + `GraphRenderer::load_probe_volume_file`，上传到 `SceneScope`），**不经过 `prism-asset`**（后者只负责 `.pak`）。引擎侧目前还没有调用点，烘焙产物要手动接线才会生效。
- **内存预算**：2 阶 SH + float32，单个 probe = 9×3×4 = 108 bytes；grid 16³ ≈ 432KB，32³ ≈ 3.5MB。
  若下一步扩展切 float16：单个 probe = 54 bytes，32³ ≈ 1.8MB。

### 6.5 `scene_frag.slang` 改动（PR-5 更新）

- **Diffuse GI**：`SampleProbeVolumeIrradiance()` → `EvalSH9Irradiance(n)` 算出 E(n)，公式：
  `gi_diffuse = kd_ibl · ibl_intensity · (E(n) / π) · albedo`
  替代 IBL irradiance cubemap 采样。
- **Specular GI**：`SampleProbeVolumeRadiance()` → `EvalSH9Radiance(r)` 算出 L(r)，公式：
  `specular_ibl = ibl_intensity · L(r) · (f_ibl · brdf_LUT.x + brdf_LUT.y)`
  替代 IBL prefiltered env map 采样。SH 2 阶对镜面反射很模糊，但室内场景中 bounced light 本质低频，且比泄漏 env sky 正确。
- `PBR_FLAG_GI`（bit 14）控制开关；`RenderSettings::gi_mode` 复用（0=Off，非0=On；baked 无 Update 状态，故只 0/非0）。

### 6.7 SH 表示变更记录（PR-5）

| 版本 | SH 存储 | Diffuse 求值 | Specular 求值 | /π 一致性 |
|------|---------|-------------|--------------|----------|
| PR-3 前 | **Irradiance SH**（cosine 预卷积） | `EvalSH9(n)` = E(n) | 无 GI specular（走 IBL env） | ❌ GI 比 IBL 亮 π 倍 |
| PR-3 | **Irradiance SH** | `EvalSH9(n)` = E(n) | 复用 `EvalSH9(r)` 当 specular（错用 irradiance） | ❌ /π 缺失 + specular 物理错误 |
| **PR-5** | **Radiance SH**（无 cosine 预卷积） | `EvalSH9Irradiance(n)` = E(n) 含 A_l 因子 | `EvalSH9Radiance(r)` = L(r) 直接重建 | ✅ 两路径统一为 kd·albedo·E/π |

### 6.6 迁移步骤（可拆 PR，每步独立可验证、CI 不红）

- **PR-1：图资源模型 + ForwardPass 进图（不改 shader）**。把 `ForwardPass` 改造成
  `RenderPassNode`，HDR/depth/normal 改为图声明的交换链级资源；`GraphRenderer::render()`
  删掉手动 set_target / set_ao / execute 编排，改构造一次 `RenderContext` 调 `graph.execute(ctx)`。
  屏障先手工（pass 内显式），图只排顺序。行为不变 → CI 绿。
- **PR-2：GtaoPass / PostPass 进图（不改 shader）**。同上模式，声明图边依赖，删手动编排。
  重点验证 1-frame-latency AO 跨帧依赖的图表达正确。
- **PR-3：probe volume 场景级资源 + `scene_frag` GI 分支**。新增 `SceneScope` 资源表、loader
  接口（`prism-render::probe_loader`）、`PBR_FLAG_GI` 采样分支。此时 GI 接进来，图的"三类生命周期"被真实消费。

> **顺序原则**：PR-1/PR-2 先把图接口按"消费者（GI）需求"定下来（§6.1 三类生命周期），PR-3 才真正接入 GI。
> 不在 PR-1 时空改接口猜 GI 需求（避免第二遍返工）。

*相关文档：`docs/lessons-learned.md`（§21/§29 framebuffer 销毁顺序、§30 CI 漂移）。*

## 7. 纹理管线（Texture Pipeline）

> 本节规格驱动纹理从"源 PNG/HDR"到"GPU 采样"的全链路设计。当前实现是
> **运行时全量 RGBA8 上传**（§31 已记录：Intel Sponza 2022 4.5 GB 解压后
> 像素、~1.8s 加载、GPU 占 4.5 GB），本节定义目标架构：**离线预处理 +
> 块压缩 + 按需流式**。这是移动端 TBDR 之外另一条"mobile-first"硬约束 --
> 移动端 GPU 内存和带宽比桌面更紧，不压缩根本跑不起来。

### 7.1 设计原则

| 原则 | 理由 |
|------|------|
| **离线预处理，运行时零解码** | PNG 解码（701ms）+ 块压缩编码（数十秒/张）都不该每次启动重做。源文件指纹变化才重导入。 |
| **移动端格式优先，桌面能力探测降级** | 契合 §1/§2.1 mobile-first 定位。ASTC 是移动端新设备原生，桌面现代 GPU 也支持；BC 作为桌面老设备回退。 |
| **产物不进 git，本地 / CI 生成** | 72 张 4K BC7 ≈ 2.25 GB，进 git 仓库爆。靠源文件 SHA256 保证一致性。 |
| **glTF 不直读，经 `prism-cooker` 离线导入** | 契合 §3「源格式由构建期导入，引擎不直读文件」。导入 / cook 在 `prism-cooker`，运行时只读 `.pak` 内的 RTEX。 |
| **mip chain 由容器承载，支持下一步扩展流式** | KTX2 原生存完整 mip chain；阶段 3 流式加载以此为前提。 |

### 7.2 格式选型（移动端新设备优先）

**ASTC 是首选格式**，覆盖移动端新设备 + 桌面现代 GPU；BC 作为桌面老设备回退；

| 纹理类型 | 主格式（移动 + 桌面现代） | 桌面回退 | 移动低端回退 |
|----------|--------------------------|----------|--------------|
| Albedo / Color (LDR) | **ASTC 6×6 sRGB** (3.56 bpp) | BC7 sRGB | ETC2 |
| Normal map (tangent-space) | **ASTC 6×6** 或专用双通道变体 | BC5 | ETC2 |
| Metallic / Roughness (打包) | **ASTC 6×6** | BC7 | ETC2 |
| 单通道遮罩 / AO / 高度 | **ASTC 6×6** | BC4 | ETC2 R |
| Emissive (LDR) | **ASTC 6×6 sRGB** | BC7 sRGB | ETC2 |
| Emissive (HDR) / IBL env / 光照贴图 | **ASTC HDR 6×6** (`VK_EXT_texture_compression_astc_hdr`) | BC6H | RGBA16f（不压缩，回退） |
| UI / 字体 atlas | **ASTC 4×4** (8 bpp，画质优先) | BC7 | ETC2 |

**为什么 ASTC 而不是 BC**：
- ASTC 6×6 (3.56 bpp) 比 BC7 (8 bpp) **小 2.2×**，画质相近 -- 移动端带宽和内存更紧。
- ASTC 是移动端硬件原生（Adreno 6xx+ / Mali Midgard+ / Apple A7+ 全支持），桌面 RTX 20+ / Intel Ice Lake+ / AMD RDNA2+ 也支持。
- Vulkan 用 `vkGetPhysicalDeviceFeatures::textureCompressionASTC_LDR` 探测，契合 §2.3 "能力驱动降级"。
- ASTC HDR 走 `VK_EXT_texture_compression_astc_hdr` 扩展探测，单独覆盖 HDR 浮点场景（BC6H 的 ASTC 对应物）。

**BC 作为桌面回退**：老桌面 GPU（pre-RTX20 / pre-RDNA2 / pre-Ice Lake）无 ASTC，回退到 BC7/BC5/BC4/BC6H。回退路径由 `capabilities.rs` 探测，不写 `#[cfg(target_os)]`。

**神经纹理压缩（NTC）暂不采用**：NVIDIA NTC（SIGGRAPH 2023）和 Qualcomm Adreno 神经纹理压缩仍处研究/早期阶段，无标准 Vulkan 扩展、无跨厂商工具链、解码需 tensor core。**留待 Khronos 标准化后再评估**（见 `docs/lessons-learned.md` §31.6）。

### 7.3 资源生命周期分类（对齐 §6.1）

纹理资源归入 §6.1 三类生命周期中的**场景级**：

| 类别 | 触发 | 示例 | 销毁责任 |
|------|------|------|----------|
| **场景级（texture）** | 场景加载/卸载 | KTX2 解出的 BC/ASTC 纹理、IBL env cube、BRDF LUT | 场景管理器（非 swapchain 回调） |

**关键**：纹理绝不能挂到 swapchain recreate 回调上。换关卡才换纹理，resize 不动。
`RenderTextureManager` 的资源表是 `SceneScope`，独立于 `SwapchainScope` / `FrameScope`。

### 7.4 离线导入管线（xtask 子命令）

新增 `xtask texture-import`，把 glTF 引用的源图（PNG/HDR）预转 KTX2 缓存。

**输入**：glTF 文件（如 `NewSponza_Main_glTF_003.gltf`）
**输出**：`scene_cache/<scene>/` 目录 + `manifest.json`

```
assets/
  scenes.json
  scene_cache/                          ← .gitignore
    sponza/
      desktop/                          ← 桌面产物（BC7/BC5/BC4/BC6H）
        arch_stone_wall_01_BaseColor.bc7.ktx2
        arch_stone_wall_01_Normal.bc5.ktx2
        ...
      android/                          ← 移动产物（ASTC 6×6 / ASTC HDR）
        arch_stone_wall_01_BaseColor.astc6.ktx2
        arch_stone_wall_01_Normal.astc6.ktx2
        ...
      manifest.json                     ← 集中元数据
```

**manifest.json 结构**（每张图一条记录）：
```json
{
  "scene": "sponza",
  "source_gltf_sha256": "a3f2...",
  "textures": [
    {
      "name": "arch_stone_wall_01_BaseColor",
      "source_uri": "textures/arch_stone_wall_01_BaseColor.png",
      "source_sha256": "b7c4...",
      "width": 4096, "height": 4096,
      "kind": "albedo",                  // 决定 sRGB + 格式选型
      "mip_levels": 12,
      "desktop": "desktop/arch_stone_wall_01_BaseColor.bc7.ktx2",
      "desktop_format": "BC7_SRGB",
      "android": "android/arch_stone_wall_01_BaseColor.astc6.ktx2",
      "android_format": "ASTC_6x6_SRGB"
    }
  ]
}
```

**导入逻辑**：
1. 解析 glTF，枚举 image URI 列表。
2. 对每张图，按文件名启发式判定 `kind`（`*_BaseColor` -> albedo/sRGB，`*_Normal` -> normal/linear，`*_Roughness*Metalness` -> MR/linear，`*Normal` 后缀优先级高于 BaseColor）。
3. 计算源文件 SHA256，与 manifest 对比；**命中且 sha 一致 -> 跳过**（增量导入）。
4. 未命中 -> `image` crate 解码 PNG -> `bc7enc` / `astc-encoder` crate 编码 -> 写 KTX2。
5. 更新 manifest。

**kind -> 格式映射**（对齐 §7.2 表）：
```rust
match kind {
    TextureKind::Albedo | TextureKind::EmissiveLdr => {
        (DesktopFmt::BC7Srgb, MobileFmt::Astc6x6Srgb)
    }
    TextureKind::Normal => {
        (DesktopFmt::BC5, MobileFmt::Astc6x6)  // linear
    }
    TextureKind::MetallicRoughness => {
        (DesktopFmt::BC7, MobileFmt::Astc6x6)  // linear
    }
    TextureKind::HdrEnv | TextureKind::EmissiveHdr => {
        (DesktopFmt::BC6H, MobileFmt::AstcHdr6x6)
    }
    TextureKind::Mask => {
        (DesktopFmt::BC4, MobileFmt::Astc6x6)  // linear
    }
}
```

### 7.5 运行时加载路径（KTX2 直读，已被 §10 取代）

> **2026-10 分层重构后已废弃**：`gltf_loader` / `SceneStore` 随旧 `prism-asset` 拆分移除，
> 导入 / cook 迁到 `prism-cooker`，运行时只读 `.pak` 内的 RTEX（见 §10.12）。本节 7.5–7.7
> 保留为历史设计，不再按此实现。

`gltf_loader::load` 曾计划改为优先读 KTX2 缓存：

```
解析 glTF 拿 image URI 列表
  -> 查 scene_cache/<scene>/manifest.json
     ├─ 命中（sha256 匹配）：
     │    mmap KTX2 字节（按平台选 desktop/ 或 android/ 子目录）
     │    -> 直接传给 BatchUploader（已是 BC/ASTC 压缩块，无需解码）
     │    -> 记录 (asset_h, vk::Format, width, height, mip_levels)
     │    省掉：PNG 解码（701ms）+ to_rgba8 转换 + 压缩格式运行时编码
     └─ 未命中：
          回退到现有 PNG -> RGBA8 路径（保留，便于无缓存时仍能跑）
          log::warn!("texture cache miss for {uri}, run `cargo run -p xtask -- texture-import <scene>`")
```

**关键约束**：
- 缓存未命中只是 warn，不是 error -- 首次运行或缓存被清时仍能跑（走 RGBA8 老路径），保证开发体验。
- 运行时**不做编码**（BC7 编码 4K 图要几十秒，体验灾难）。编码只在 xtask 离线做。
- KTX2 是 GPU-ready 字节，mmap 后 `vkCmdCopyBufferToImage` 直接传，**接近零 CPU 成本**。

### 7.6 BatchUploader / TextureUploadInput 改造

**已实现（2026-08-21）** `TextureUploadInput` 已扩展支持压缩格式：

```rust
pub struct TextureUploadInput {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat, // Rgba8 | Bc7* | Bc5 | Bc4 | Bc6H | Astc* | Etc2
    pub mip_levels: u32,       // 0=自动推导(Rgba8) / 1=单级(压缩) / >1=KTX2 完整链
    pub pixels: Vec<u8>,       // 压缩块字节（BC/ASTC）或 RGBA8
}

pub enum TextureFormat {
    Rgba8, Rgba8Srgb,
    Bc7Unorm, Bc7Srgb, Bc5Unorm, Bc4Unorm, Bc6HUfloat,
    Astc4x4Unorm/Srgb, Astc6x6Unorm/Srgb, Astc8x8Unorm/Srgb,
    Etc2R8G8B8A8Srgb, // 覆盖 CookProfile 全量
}
impl TextureFormat { fn is_compressed()->bool; fn compressed_byte_len()->usize; fn vk_format()->vk::Format }
```

`BatchUploader::upload_image` 按格式分支：
- **Rgba8**：创建 image + staging + copy + 生成 mip blit chain。
- **BC/ASTC**：创建 image（带 `vk::Format`）+ staging + copy 全部 mip level（**不做 blit**，压缩格式不能 blit，mip chain 由 KTX2 预生成）。直接 transition 到 `SHADER_READ_ONLY_OPTIMAL`。
- `RenderTextureManager::reserve/reserve_into` 已按 `is_compressed` 做 `compressed_byte_len` 校验与 `mip_levels` 分支。

**mip chain 由 KTX2 承载**：离线工具用 `bc7enc` 等编码器从 mip 0 逐级降采样 + 编码，写入 KTX2 的 mip level 数组。运行时一次性 copy 全部 mip，无需运行时降采样。

### 7.7 阶段拆解（可独立 PR，每步 CI 绿）

- **PR-T1：BC/ASTC 上传支持（不改加载路径）** ✅ **已完成 2026-08-21**。`TextureFormat` 全量扩展 + `mip_levels` + `is_compressed/vk_format` + `RenderTextureManager` 校验/上传分支已落地。下一步扩展测试补充 BC7 编码走新路径的采样近似验证。**此 PR 不动 glTF 加载，运行时仍走 RGBA8（压缩路径可手动构造验证）**。
- **PR-T2：xtask texture-import 离线工具**。新增 `xtask/src/bin/texture-import.rs`，依赖 `ktx2` + `bc7enc` + `astc-encoder` crate。扫 glTF -> 编码 -> 写 KTX2 + manifest.json。命令行：`cargo run -p xtask -- texture-import --scene sponza --platform desktop,android`。**此 PR 只产工具，不改引擎**。
- **PR-T3：`prism-asset` 运行时优先读 KTX2**。`gltf_loader::load` 加 cache 查询分支，命中走 KTX2 路径，未命中回退 RGBA8（打 warn）。`SceneStore` 加 KTX2 解析（`ktx2` crate decode）。**此 PR 上线后，跑过一次 `xtask texture-import` 的场景加载时间从 ~1.8s 降到 ~0.5s 量级**。
- **PR-T4（可选）：mip chain 流式加载**。KTX2 mip level 按可视距离动态加载/卸载，首帧只加载低 mip。需要 `SceneStore` 支持部分加载 + 渲染管线容忍"纹理未就绪"。工作量大，放下一步扩展里程碑。

> **顺序原则**：PR-T1 先把"能传压缩格式"的能力做出来（不依赖导入工具），PR-T2 再做导入工具（不依赖引擎改造），PR-T3 才把两者接起来。每步独立可验证，避免"先改引擎再发现导入工具没法跟上"的返工。
> **状态**：T1（压缩格式上传）已完成；T2 / T3 未按本节实施，纹理导入 / cook 改由 §10 的
> `prism-cooker` 承担，运行时读 `.pak` 内的 RTEX。

### 7.8 不做 / 反目标

- **不**在运行时做 BC/ASTC 编码。编码慢（秒级/张），必须在离线工具做。
- **不**把 KTX2 产物进 git。太大，靠 SHA256 保证一致性。
- **不**用 Basis Universal UASTC 跨平台单编码。画质略损，且 PrismaRev 桌面/移动都支持 ASTC（桌面现代 GPU 全支持），不需要"一次编码到处转"。多平台产物分别生成更清晰。
- **不**用神经纹理压缩（NTC / Adreno 神经纹理）。无标准扩展、无跨厂商工具链，留待 Khronos 标准化。
- **不**为 BC1/BC2/BC3 单独支持。BC2/BC3 是 DX9 时代格式被 BC7 取代；BC1 仅极致体积场景用，移动端 ASTC 12×12 已覆盖该 niche。

---

## 8. 帧生命周期与架构分层（规划）

> **2026-08-21 更新**：`GraphRenderer::render()` 已拆为 `begin_frame` → `prepare` → `execute` → `present` → `end_frame`，`prepare` 已在 `render_system`/`render_runner` 中实际调用（脏检测预留）。随着场景资产增多，下一步扩展需进一步拆分为 `RenderRuntime`/`RenderAppShell`/`Plugin` 三层（见 §8.2）以保证线程安全和资源契约。

### 8.1 一帧的不同阶段

```
Before Render = begin_frame + update + prepare + after_prepare
Render        = app.render (RenderGraph 录制 + 提交)
After Render  = present + end_frame
```

各阶段的访问权限：

| 阶段 | 可访问 | 不可访问 |
|------|--------|----------|
| `update` | 修改 `World`（CPU 场景语义：增删 instance、换材质、改 transform） | GPU 资源、bindless 表 |
| `prepare` | 读 `World` 当前快照，写 GPU scene buffer、descriptor、bindless | 修改 `World` |
| `after_prepare` | 只读 query（批量 raycast、scene 版本检查） | 修改 `World` 或 GPU scene |
| `render` | 只读 GPU scene（`SceneReadView`），通过 RenderGraph 录制 command | 修改 scene state |

### 8.2 三层职责

| 层 | 职责 | 对应代码 |
|----|------|---------|
| **RenderRuntime** | GPU 资源 owner：`VulkanContext`、bindless manager、descriptor system、swapchain、command pool、render world（scene GPU 快照）。提供阶段化访问入口。 | 当前 `GraphRenderer` 拆出，新 crate `prism-render-runtime` |
| **RenderAppShell** | 一帧顺序编排者：按固定顺序调用 runtime 的各阶段钩子，把窄化后的上下文传给 App。 | 新模块，依赖 runtime |
| **App / Plugin** | 具体业务：决定 pass 顺序、持有窗口尺寸资源（RT targets、main view）、camera、GUI。 | 当前分散在 `app.rs` + `render_system.rs`，逐步拆为 Plugin |

### 8.3 启动与 Resize

- 启动入口唯一：平台层创建窗口，渲染线程通过 `Box<dyn RenderApp>` 驱动 App。
- resize 只在 render loop 安全点处理。`RenderRuntime::handle_resize` 重建 swapchain + present 状态后，返回 `ResizeCtx`，App 据此通知需要重建窗口尺寸的 Plugin（RT working target、main view、GBuffer）。
- 关闭流程：先调用 App 和 Plugin 的 `shutdown()`，再销毁 `RenderRuntime`（GPU idle → release 子资源 → destroy `Gfx` owner）。

### 8.4 Plugin 模型（扩展路线）

渲染管线能力拆为可插拔的 Plugin，每个 Plugin ：

- 在 `setup` 时注册自己的 pass 节点到 RenderGraph。
- 在 `update` / `prepare` / `render` 阶段被 App 依次调用。
- 持有自己的资源生命周期（如 `RtPipeline` 持有 RT working target、main view target）。

当前 `ShadowMapPass` / `ForwardPass` / `GtaoPass` / `PostPass` 顺序固定写死在 `GraphRenderer::new` 中，下一步扩展改为 App 在 `render` 钩子中按需添加 pass。

### 8.5 迁移步骤

- **PR-L1：阶段化拆分** ✅ **已完成 2026-08-21**。`GraphRenderer::render()` 已拆为 `begin_frame` → `prepare` → `execute` → `present` → `end_frame` 并接入调用链，`prepare` 实现输入校验/脏日志预留。当前仍在单线程内顺序调用，CI 绿。
- **PR-L2：Runtime / App 分离** ✅ **已完成 2026-08-21（实现）**。新增 `prism-render/src/render_runtime.rs:RenderRuntime`（薄封装 `VulkanContext`+`GraphRenderer`）与 `prism-render/src/plugin.rs:RenderPlugin + PluginRegistry`，满足 §8.2 三层职责的接口契约；`GraphRenderer::context_arc()` 已暴露，下一步扩展可拆为独立 crate。
- **PR-L3：Plugin 接入** ✅ **已完成 2026-08-21（实现）**。`PluginRegistry::register/setup_all` 已接入，`ShadowMapPass`/`GtaoPass` 保持现有硬编码顺序，下一步扩展改为 `RenderSettings` 驱动的 `PluginRegistry` 注册即可。

---

## 9. 场景数据同步（CPU→GPU 设计）

> **2026-08-21 部分完成**：`DirtyRouter` 已具备 `camera/dir_light/point_lights/exposure/draw_count` 脏检测并接入 `prepare`，但仍为“点对点手动触发”向“统一 `DirtyDispatchPlan` + `prepare` 批同步”（§9.1）过渡阶段。`app.rs` 的各路 upload 尚未收敛到 `prepare`，`SceneReadView` 仍为扁平 `DrawItem` 提取。

### 9.1 同步管道总览

```
World (CPU 语义权威)
  │
  ├─ AssetHub 后台加载 glTF/纹理 → 发 ModelLoaded 事件
  │
  └─ sync_for_render (每帧 update → prepare 边界)
       │
       ├─ SceneAssetIngestor：把 loader 产出的 CPU bytes 转为 typed handle
       │   （TextureHandle / MeshHandle / MaterialHandle）
       │
       ├─ SceneChanges：本帧 CPU 语义变化集合
       │   （added/removed/changed instance / material / light / texture / sky）
       │
       └─ DirtyRouter：SceneChanges + 静态规则 → DirtyDispatchPlan
            │
            ├─ RenderTextureManager：上传 texture → bindless 注册
            ├─ RenderMeshManager：submesh geometry upload + mesh BLAS
            ├─ RenderMaterialManager：material buffer upload
            ├─ RenderInstanceManager：instance slot 分配 + active render list
            ├─ RenderSkyManager：HDRI importance alias table + sky dispatch
            ├─ RenderAnalyticLightManager：analytic light buffer
            └─ RenderEmissiveLightTable：emissive triangle records + alias table
```

### 9.2 Dirty 路由规则

Dirty dispatch 不采用通用事件总线，而是使用**静态规则集**，每类 SceneChange 映射到一个或多个 render manager 的 dispatch：

| CPU 变化 | 触发的 render dispatch |
|----------|----------------------|
| texture added / changed / removed | `RenderTextureManager` → 上传 / 更新 / 释放 bindless slot |
| texture ready 完成 | → 标记依赖该 texture 的 material dirty |
| mesh added / changed / removed | `RenderMeshManager` → geometry upload + BLAS rebuild |
| material changed | `RenderMaterialManager` → material buffer upload |
| instance changed (transform / active / binding) | `RenderInstanceManager` → instance slot update + `RenderEmissiveLightTable` dirty |
| sky changed | `RenderSkyManager` → sky dispatch |
| analytic light changed | `RenderAnalyticLightManager` → light buffer rebuild |
| emissive table rebuild (由 mesh / material / instance dirty 级联触发) | `RenderEmissiveLightTable` → emissive triangle records + alias table |

**关键规则**：
- prepare 阶段**一次性**消费 `SceneChanges` 并生成 `DirtyDispatchPlan`，不允许多帧累积脏状态。
- `RenderEmissiveLightTable` 依赖 `RenderInstanceManager` 的 prepare 结果（active instance list），因此它的 rebuild 排在 instance dispatch 之后。
- texture → material → instance 的级联目前只由 `prism-engine::dirty_router` 对 `SceneChanges` /
  draw item 签名做帧间比对；`RenderTextureManager` / `RenderMaterialManager` 按槽位更新，
  没有跨层引用拒绝。上面的统一 `DirtyDispatchPlan` 是目标形态，尚未实现。

### 9.3 只读场景视图

Pass 不直接访问 manager 内部状态，改为通过只读 `SceneReadView`：

```rust
pub struct SceneReadView<'a> {
    /// scene root buffer（instance / material / light SSBOs）
    scene_buffers: &'a SceneGpuBuffers,
    /// TLAS handle（ray query）
    tlas: vk::AccelerationStructureKHR,
    /// bindless texture table
    textures: &'a BindlessTextureTable,
    /// sky distribution alias table（HDRI importance sampling）
    sky: &'a SkyGpuState,
}
```

- `SceneReadView` 在 prepare 之后产生，render 阶段只读。
- 版本号快照（`accum_signature`）暴露 scene 语义是否变化，供 App 判断是否需要重置离线累积状态。
- 不暴露 `RenderWorld` owner 或具体 GPU buffer 布局。

### 9.4 资源生命周期与同步阶段

按 scope 区分，prepare 阶段按以下顺序执行：

1. **帧级同步**：acquire present target、FIF wait、command pool reset。
2. **Dirty dispatch**：消费 `SceneChanges`，写入 render manager 的 pending 队列。
3. **GPU upload**：texture → submesh geometry → material buffer → instance buffer。
4. **Emissive table**：在 instance dispatch 之后、scene buffer upload 之前构建。
5. **Descriptor update**：per-frame descriptor 写入（scene UBO、bindless table 版本）。
6. **SceneReadView 生成**：所有 upload 完成后，构造不可变快照。

### 9.5 迁移步骤

- **PR-S1：SceneChanges 提取** ✅ **已完成**。`prism-engine::render_system::SceneChanges` + `extract_frame_packet` 已统一抽取相机/光照/`draw_items`，并在 `DirtyRouter::update` 中集中对比。
- **PR-S2：DirtyRouter 接入 RenderManager** ✅ **部分完成 2026-08-21**。`DirtyRouter` 已扩展 `exposure/draw_count/pt_lights` 并接入 `prepare`；`DirtyDispatchPlan` 预留已在 `prepare` 中日志，下一步将 `RenderTexture/Mesh/Material` 的 `upload` 收敛到 `prepare` 批处理并禁止外部直调。
- **PR-S3：SceneReadView 替换 pass 中的 manager 直接引用** ⏳ **接口已定义**。`SceneReadView` 扁平 `DrawItem` 已覆盖绘制路径，`FrameInput` 即为过渡态只读视图；下一步扩展将 `GraphResources` 句柄化并让 `ForwardPass::execute(ctx, view)` 取代裸 `&mut Manager`。
- **PR-S4（可选）：后台 AssetHub 异步加载** ✅ **实现 已接线**。`prism-app::io_runner` 与 `asset_bridge::AssetResolveRequest/Result` 通道 + `render_runner` 异步 `GraphRenderer` 构建已打通，后台解码 `ModelLoaded` 事件以 `flume` 投递，当前同步 `GpuAssetResolver` 为回退。

---

## 10. 资源管线 v2 —— 离线预处理管线

> 2026-07-25 新增，2026-10 分层重构。资产职责拆成三个 crate：
> **`prism-asset`**（运行时 `.pak` / `ResourceManager`）、**`prism-scene`**
> （组件 schema + `.scene.json` + RSCN v4 编解码）、**`prism-cooker`**
> （importer / cooker / db / `prism-cook` CLI）。旧的 7-crate 拆分不再恢复。
> 构建编排在 `prism-build-pipeline` 与编辑器中，不进入游戏运行时。

### 10.1 架构总览

```
源文件 (Assets/)
    │
    ▼  [Import]  ← importer 模块 + db 模块
  ┌─────────────┐
  │  中间格式     │  RTXI（纹理）、RMXI（网格）
  └──────┬──────┘
         │
         ▼  [Cook]  ← prism-cooker（+ CookProfile）
  ┌─────────────┐
  │  运行时格式    │  RTEX（纹理 mip 链）、RMES（交错顶点）、RSCN v4（场景）
  └──────┬──────┘
         │
         ▼  [Package]  ← prism-asset::package
  ┌─────────────┐
  │  .pak 归档   │  RPAK 格式 + xxh3 校验和
  └──────┬──────┘
         │
         ▼  [Runtime]  ← prism-asset::runtime
  ┌─────────────┐
  │  Handle<T>  │  懒加载 + 内存预算 + 依赖解析 + 热重载
  └─────────────┘
```

**核心设计原则**：
- **编辑器离线完成重计算**，运行时只读 `.pak`，零解码、零编码。
- **运行时无构建工具依赖**：`prism-asset` 不依赖 `prism-cooker` / `prism-scene`；
  `prism-cooker` 只在构建机/编辑器中使用。
- **场景 schema 只有一份**：cooker 与运行时都通过 `prism-scene` 的
  `ComponentRegistry` + `TypeDesc` 解释组件，RSCN v4 不按场景重复存 schema。
- **Handle<T> 与 ECS Entity 分离**：Handle 是资产生命周期概念（代沟计数 slot），
  Entity 是帧级 ECS 概念，二者不混用。

### 10.2 crate 分层及其职责

| crate | 角色 | 职责 | 主要依赖 |
|-------|------|------|----------|
| `prism-asset` | 运行时 | `AssetId`（64 bit gen+serial）、`AssetType`、`Handle<T>`、`.pak`（RPAK + zstd + xxh3）、`ResourceManager`（懒加载/预算/热重载）、streaming | serde, zstd, tokio（feature-gated） |
| `prism-gfx` | 共享数据 | 形状的渲染状态枚举（`CullMode`/`CompareFunction`/`BlendMode`/`RenderQueue`/`ShadowCastingMode`/光照与相机枚举）+ 反射编码；Vulkan-free、ECS-free | `prism-reflect`、serde、glam |
| `prism-scene` | schema | 场景/UI 组件、`ComponentRegistry`、`.scene.json` 作者格式、RSCN v4 编解码 | `prism-ecs`、`prism-reflect`、`prism-gfx`、serde |
| `prism-cooker` | 构建期 | importer 框架、cooker 框架、CookProfile、资产数据库/导入缓存、`SceneCooker`、CLI `prism-cook` | `prism-asset`、`prism-scene`、image、gltf |
| `prism-build-pipeline` | 编排 | `bake-gi`、`cook`、`heightmap` 等批处理入口；调用 cooker，不进入运行时 | `prism-cooker`、`prism-engine` |

`prism-asset` 的 feature 只服务运行时裁剪（`runtime` / `package` / `types` /
`streaming` / `hot-reload`），不再包含 `cooker` / `importer` / `db` / `cli`。
构建工具消费 `prism-asset` 的 `core` / `package`，但不会反向进入游戏依赖链。

```sh
cargo build -p prism-asset -p prism-scene -p prism-cooker
cargo test  -p prism-asset -p prism-scene -p prism-cooker
cargo run   -p prism-cooker --bin prism-cook -- init
```

### 10.3 资源类型系统（core 模块）

**`AssetId`**（`id.rs`）：
```
高 32 位 = generation（单调 epoch），低 32 位 = serial
AssetId::generate()       → 进程级原子递增
AssetId::tombstone(n)     → 删除标记（gen=u32::MAX，排序在所有活 ID 之后）
AssetIdGenerator           → 编辑器侧持久化 ID 发生器
```

**`AssetType`**（`type.rs`）—— `#[repr(u32)]` 枚举，运行时通过 u32 判别：
```
Binary(0) / Texture(1) / Mesh(2) / Material(3) / Shader(4) /
Prefab(5) / Scene(6) / Audio(7) / Font(8) / Unknown(0xFF)
```
- `from_extension()` 由扩展名推断类型（字体：`ttf` / `otf` / `ttc`）
- `Font` 原样进包，不做 cook：运行时读出字节交给 UI 文本光栅器（`Text.font`，#583）
- `AssetRef { id, asset_type }`——可序列化的轻量跨资产引用

**`Handle<T>`**（`handle.rs`）——代沟计数句柄，`u64` 大小：
```
低 32 位 = slot index，高 32 位 = generation
null = (0, 0)，static 区 = index < 1024
AnyHandle = 类型擦除版本，支持异构存储
```

### 10.4 中间格式（Importer→Cooker 契约）

| 格式 | 魔数 | 载荷 |
|------|------|------|
| **RTXI**（纹理中间） | `b"RTXI"` | `[magic:4][w:4][h:4][ch:1][fmt:1][RGBA8 pixels:N]` |
| **RMXI**（网格中间） | `b"RMXI"` | `[magic:4][ver:1][verts:4][idxs:4][uv_ch:4][pos:N][nrm:N][uv:N][idx:N]` |

中间格式是纯 CPU 数据，无 GPU 依赖。Importer 负责从源文件解码，
Cooker 负责消费中间格式生成运行时格式。

### 10.5 运行时格式（Cooker→Package 契约）

| 格式 | 魔数 | 结构 |
|------|------|------|
| **RTEX**（cooked 纹理） | `b"RTEX"` | header + mip 偏移表 + 各级 mip 数据（box filter 降采样） |
| **RMES**（cooked 网格） | `b"RMES"` | header + 属性偏移表 + **平面**顶点块（positions → normals → uv，偏移字段为权威寻址）+ 索引数据 |

> RMES 头里的 `stride` 字段不用于定位属性：`MeshCooker::write_rmes` 写的是平面块，
> `stride` 只是记录交错尺寸。`prism-render/src/asset_decoders.rs` 的 `decode_rmes`
> 按偏移字段解析，不按 `stride` 交错读取。

RTEX 的 mip 链由 Cooker 通过 2×2 box filter 生成（`TextureCooker::generate_mips`），
运行时无需降采样。后期可替换为更高质量的 Kaiser/ Lanczos 滤波。

### 10.6 .pak 归档格式（package 模块）

```
┌─ PackageHeader ────────────────────────┬── 52 bytes ─┐
│ magic(4)=b"RPAK"  version(4)=1          │
│ asset_count(4)  registry_offset(8)      │
│ registry_size(8)  data_offset(8)        │
│ data_size(8)  checksum(8)               │
├─ RuntimeAssetRecord[n] ────────────────┬── 48 bytes × n ─┐
│ id(8)  type_id(4)  flags(4)             │
│ offset(8)  size(8)  compressed_size(8)  │
│ dep_start(4)  dep_count(4)              │
├─ Dependency Array[m] ──────────────────┬── 8 bytes × m ─┐
│ [AssetId(u64)]                          │
├─ Data Chunks ──────────────────────────┤
│ [asset 0 data][asset 1 data]...         │
└─────────────────────────────────────────┘
```
- **压缩**：zstd（按 asset 粒度，`FLAG_COMPRESSED` 标记）
- **校验**：xxh3-64 覆盖 `header[4..44]`（version / asset_count / 各偏移与长度；`magic` 与
  `checksum` 自身除外）+ registry + deps + data。读取端在**解析 registry 之前**校验，
  篡改 `version` / `asset_count` 会直接得到 `ChecksumMismatch`（#89）。
- **读取**：`PackageReader` 零拷贝访问（未压缩 asset 直接 memcpy）

### 10.7 Cook Profile 系统（`prism-cooker`，src/cooker/profile.rs）

**优先级链**（高→低）：
```
1. CLI 覆盖（命令行参数）
2. 活动项目配置（`--profile`；没有持久化的 active 配置文件，缺省用平台默认）
3. 平台默认配置（--platform → desktop/android/ios/embedded）
4. base.json（最低）
```

**内置 5 个配置**：

| 配置 | 纹理压缩 | 最大尺寸 | 生成切线 | 顶点压缩 | 流式 |
|------|---------|---------|---------|---------|------|
| base | RGBA8 | 不限 | 否 | 否 | 否 |
| desktop | **BC7** | 4096 | **是** | 否 | 否 |
| android | **ASTC 8×8** | 2048 | 否 | **是** | **是** |
| ios | ASTC 8×8 | 2048 | 否 | 是 | 是 |
| embedded | ETC2 RGBA | 1024 | 否 | 是 | 是 |

**重要说明（2026-08-21 更新）**：`TextureCooker` 仍生成 RGBA8 RTEX，BC7/ASTC/ETC2 编码器集成待下一步扩展 PR；但运行时的 `TextureFormat/vk_format` 与 `RenderTextureManager` 上传侧已就绪（PR-T1 完成），Cooker 产出的 RGBA8 可直接经压缩路径验证，待编码器接入后切换为压缩 RTEX 即可生效。

**`CookSettings`** 提供 `settings_hash()`：确定性 JSON → xxh3-64，
用于增量构建缓存键。

**覆盖语义**：profile 文件里的字段是逐字段 `Option`（`TextureOverrides` / `MeshOverrides` /
`ShaderOverrides` / `CompressionOverrides`），只有**写出来的**字段才覆盖父级或内置值，
缺省字段保持继承结果。所以 `{"texture":{"quality":95}}` 不会把父级的 `max_size` 清成 0，
显式写入默认值（`"compression":"rgba8"`、`"generate_mips":false`）也能生效。

**`ProfileManager`：**
- `resolve(name)` → 递归继承合并 → `CookSettings`
- 循环检测（环路径文字报告）
- `apply_cli_overrides()` → 命令行覆盖叠加
- 内置配置由 `BUILTIN_DEFAULTS` 静态 `LazyLock<HashMap>` 承载，
  用户配置从磁盘 `profiles_dir/{name}.json` 加载；`prism-cook build` 的 `profiles_dir`
  是 `<project>/Library/Profiles/`，解析顺序 `--profile` → `--platform` → `desktop`。

### 10.8 运行时 ResourceManager（`prism-asset::runtime`）

**核心流程**：
1. `load_package("game.pak")` → 注册所有资产（填充 slot 数组 + `AssetId→index` 映射）
2. `load::<T: Asset>(id)` → 首次按需从 `.pak` 读取数据，缓存到 slot，返回 `Handle<T>`
3. `get::<T>(handle)` → 代沟校验 → 反序列化 → 返回 `T`
4. `unload(handle)` / `unload_all()` → 释放数据，更新内存追踪

**内存预算**：
- `set_memory_budget(bytes)` + `set_eviction_policy(Lru|Fifo|None)`
- `load()` 时预算超额 → 自动淘汰最久未访问资源
- `evict(target_bytes)` 手动触发淘汰

**依赖解析**：
- `load_with_deps<T>(id)` → DFS 拓扑序加载所有依赖
- 循环检测（warn + 跳过）

**热重载**（feature-gated `hot-reload`）：
- `HotReloadWatcher`：轮询 `.pak` 文件修改时间
- `on_pak_changed()`：重读数据 → **原地**更新 slot（不递增 generation：句柄身份不变，
  热重载前发出的 `Handle` 继续可用）

**`Asset` trait**：
```rust
pub trait Asset: Sized + Send + 'static {
    fn asset_type() -> AssetType;
    fn from_bytes(data: &[u8]) -> Result<Self, RuntimeError>;
    fn into_bytes(self) -> Vec<u8>;
}
```
内建 `impl Asset for Vec<u8>`（二进制 blob）。

### 10.9 CLI 工具（`prism-cooker`，bin: `prism-cook`）

| 命令 | 功能 |
|------|------|
| `init` | 创建 `Assets/` + `Library/` 目录结构 |
| `scan` | 扫描 `Assets/`，按扩展名推断类型，写入数据库 |
| `import` | 对每个文件运行匹配的 Importer，增量缓存 |
| `build --output game.pak` | 烹饪所有资产 → 打包 `.pak`；`--profile <name>` / `--platform <p>` 先解析 profile（`--compression` 覆盖 profile 的 level） |
| `validate game.pak` | 验证魔数 + 版本 + 校验和 |
| `list` | 列出数据库全部资产 |
| `inspect <id>` | 查看单资产详情（依赖树可见） |

### 10.10 场景 cook 与运行时的接缝

场景资产走一条独立的类型化路径，不再把 JSON 塞进 RSCN：

```text
.scene.json
   │  prism-cooker::SceneCooker（parse + validate）
   ▼
prism-scene::rscn::encode_scene
   │  type_id + schema_hash + 位置编码载荷
   ▼
.rscn  ──►  prism-engine::SceneLoader（decode_scene + ComponentRegistry）
```

- `prism-build-pipeline build-scenes` 读取 `assets/scenes.json`，按其中的路径 cook 场景、
  把 manifest 中 `.scene.json` 路径重写为 `.rscn` 后打进包，并产出运行时使用的
  `assets/scenes.pak` + `.pak.meta.json`；这是发布入口。
  它同时**递归收集场景引用的资源**：环境 HDR、`SpriteRenderer.sprite_path`、
  `MeshRenderer.mesh_path`（glTF/GLB → RMES）与 `material_path`（`.mat.json` → RMAT，
  连同 5 张贴图）、`Style.image` / `Tilemap.tileset_path` / `Text.font`。任何引用解析
  不到（文件不存在、绝对路径、逃出 `assets/`）都会让 `build-scenes` **非零退出**并列
  出路径——否则打包产物会在运行期静默缺资源（#530）；
- `prism-build-pipeline cook <input.scene.json>` 单场景 cook，直接调用 `SceneCooker`；
- 通用纹理/网格/二进制资产仍走 `prism-cook import/build`；
- `prism-build-pipeline pack <cooked-dir>` 把已 cook 的目录打成 `.pak`，并写
  `.pak.meta.json` 路径清单；资产 ID 由**包内相对路径**派生（FNV-1a，
  [`AssetId::from_path`]），因此往目录里加文件不会重编号已有资源——材质数据的
  `RMAT` 里烘的是贴图 `AssetId`，只有路径派生才能与包内 ID 对上（#530）；
- 运行时只读 `.rscn` / `.pak`，不依赖 `prism-cooker`；
- 开发期也可以从松散的 `.rscn` 文件加载，路径由 `SceneSource` 决定。

**开发模式与发布模式**：二者共用同一条运行时解码路径；区别只在资源来源
（松散文件 vs `.pak`），不是两套格式。

### 10.11 接入引擎的待办清单（Integration Gate）

要完成"离线预处理 → .pak → 引擎运行时"的闭环，需要以下 PR：

- **[G1] 运行时格式解码器** 🟡 **部分完成**：
  - `prism-asset/src/formats.rs`（`decode_rtex` / `decode_rmes`）解析成纯数据 `RtexInfo` / `RmesInfo`，
    由 `runtime/typed.rs` 的 `TextureAsset` / `MeshAsset` 使用；
  - `prism-render/src/asset_decoders.rs`（`decode_rtex` / `decode_rmes`）按 Cooker 真实布局
    （RTEX 版本字段 + mip 偏移表、RMES 属性偏移 + 平面顶点块）解析成
    `TextureUploadInput` / `MeshUploadInput`，含格式码映射（RGBA8 / BC5 / BC6H）与越界校验；
  - **尚未完成**：两个 GPU 解码器目前没有调用点，还没接到 `BatchUploader`；RTEX 预烘焙 mip 链
    只上传 base level（`TextureUploadInput` 只能承载单级紧密像素），见 #106。
    因此 `.pak → GPU` 的纹理/网格端到端仍待补。

- **[G2] Cooker 输出格式与引擎对接** ✅ **场景链路已打通**：`SceneCooker` 产出 RSCN v4，`prism-engine` 的 `SceneLoader` 用同一份 `prism-scene` schema 解码；`prism-cooker` 中有真实 `.scene.json` 的 cook/decode 测试。纹理/网格的压缩数据端到端对比仍待补。

- **[G3] ResourceManager → Engine 桥接** ✅ **部分完成**：`GpuAssetResolver` 已持 `ResourceManager` 并支持 `load_package/load_package_bytes` 与 `load_with_deps`，`App::new` 中 `load_resource_package()` 会挂载 `assets/` 下**所有** `.pak`（每个包旁有同名 `.pak.meta.json` 时一并加载路径清单），不再写死 `scenes.pak`；`Handle<T>`→`Entity` 的映射由 `GpuAssetResolver` 的引用缓存维护（`MeshRef` / `MaterialRef` /
  `SpriteRef` 持 `Handle`），全链路 `CLI build → .pak → ResourceManager → GPU` 可手动验证。

- **[G4] 构建脚本集成** ✅ **已落地**：`prism-build-pipeline build-scenes` 统一完成
  `scenes.json → cooked RSCN → scenes.pak + path manifest`；泛资产仍走 `prism-cook build`，
  `scripts/run.ps1` 在 `cargo build` 后自动调用它。`scripts/build-android.ps1` /
  `build-android-game.sh` 按扩展名复制 `assets/` 下**全部** `.pak` 与 `.pak.meta.json`
  到 APK 的 `assets/game/`（一个都没有就报错）；Android 启动时 `run_on_android` 再把
  `game/` 复制到内部存储并切 cwd。
  CI 的资产构建步骤仍待补。

- **[G5] CookProfile 集成到引擎设置** ⏳ **配置已就绪**：`CookProfile` 5 档 + `settings_hash` 已实现，`--profile` CLI 透传待接入 `AppConfig` 启动参数。

- **[G6] 热重载管道** ✅ **实现 已接线**：`ResourceManager::on_pak_changed` + `HotReloadWatcher`（`hot-reload` feature）已实现轮询，`prism-engine::hot_reload` 预留模块已预留，编辑器 `HotReloadWatcher` 启动后 `.pak` 变更可原地刷新驻留槽（`Handle` 保持有效）。

> **里程碑建议**：G1+G2+G3 为"闭环 MVP"✅ 实现 已打通（可手动端到端），G4+G5 为"开发体验完善"⏳，G6 为"编辑器体验"✅ 实现。

### 10.12 §7 纹理管线与 §10 的关系

§7（纹理管线）设计的 KTX2/BC/ASTC 离线压缩方案，在概念上属于 §10 管线中
`TextureImporter` → `TextureCooker` 链的增强。具体来说：

- §7 的 **PR-T1（压缩格式上传支持）** 属于 §10 G1 的一部分——扩展
  `BatchUploader::upload_image` 支持 BC/ASTC `vk::Format`。
- §7 的 **PR-T2（xtask texture-import）** 属于 §10 Cooker 的离线编码增强——
  在 `TextureCooker` 中添加 `compress: Some(Bc7|Astc|...)` 路径，
  替代当前的 RGBA8 直通路径。
- §7 的 **PR-T3（运行时优先读 KTX2）** 被 §10 `.pak` 方案取代——
  运行时不再读 KTX2 文件，而是读内含已压缩 RTEX 数据的 `.pak`。

因此 §7 不删除，而是被 §10 框架吸纳为内部实现细节。新增纹理压缩功能
应在 §10 的 Cooker 和 Package 层级实现，而非绕过管线直读文件系统。

### 10.13 作者格式 v3：实体身份（设计决策）

**状态**：**已实现**（#31）。`prism-scene`（`id` 字段 / 校验 / 迁移 / cook）与 `prism-editor-api`（子树操作改用 id、删掉下标重映射）同批落地；前置改动见 #23（transform 单一来源）与 #29（cook 线性化）。

#### 现状与问题

`.scene.json` v2 的实体用**数组下标**当身份：

```json
{ "entities": [ { "name": "Root" }, { "name": "Child", "parent": 0 } ] }
```

顺序同时承担三件事：遍历顺序、实体身份、父引用目标。代价在 `prism-editor-api` 里全部显性化：
`move_entity` / `delete_entities` / `sync_prefab_instances` / `instantiate_prefab` 各自维护一套
`remap[]` 并**全量重写每个实体的 `parent`**；任何一处漏改都是静默错挂，且无法表达
「跨场景引用」或让外部工具往返编辑。

#### 决定

v3 引入稳定 id，`parent` 指向 id：

```json
{
  "version": 3,
  "entities": [
    { "id": 0, "name": "Root",  "parent": null, "transform": { }, "components": { } },
    { "id": 7, "name": "Child", "parent": 0,    "transform": { }, "components": { } }
  ]
}
```

- **id 单调递增**（`max(id) + 1`），由编辑器在新建 / 复制 / 实例化时分配；删除后**不复用**，
  避免旧引用"复活"到新实体。
- **数组顺序不再是身份**，只表示文件布局与编辑顺序。
- **位宽定为 `u32`**（不是 `u64` / UUID），理由三条：
  1. UUID 让 diff 充满噪声、难以人工 review，还会污染测试与 fixture 的确定性（需注入生成器）；
     本项目是单人 + agent 的 git 流，并发创建实体的冲突概率极低，且重复 id 会被 `validate` 抓住。
  2. `u64` 的唯一卖点是"高位留命名空间"，但这条路在本技术栈里走不通：`parent` 在编辑器中是
     TS `number`，安全整数上限 **2^53**；把命名空间放进高 32 位会让 id 越过该上限，
     `JSON.parse` 静默丢精度 —— 那比撞号更危险（撞号会被校验拦住，精度丢失不会）。
     而 JSON 里位宽本身不可见（小数字序列化等长），`u64` 的可读性收益也是零。
  3. 真要命名空间就得用字符串 id，等于回到 UUID 的代价，已被否决。
- 代码侧把 id 当作**不透明不变量**（只保证"文档内唯一且不复用"），不假设它等于下标或连续，
  这样将来若有强度更大的需求，改动面被限制在类型宽度 + 一次迁移。

#### 不变量（`SceneJson::validate`）

1. 至少一个实体；`id` 唯一。
2. `parent` 必须指向**存在**的 id，且不得指向自身。
3. 环：**加载侧容错**（`topological_order` 把未访问到的实体按数组顺序追加到末尾，结果确定），
   **编辑器侧拒绝**成环（`reparent` 已有的 descendants 检查保留）。
   硬失败会让一个手写错环直接打不开场景，代价大于收益；cook 时对环记 `warn`。

#### 迁移 v2 → v3

- `id = 数组下标`（恒等映射），`parent` 数值**不变** → 迁移是纯增量，老文档 diff 只有 `id` 字段。
- 幂等：已含 `id` 的文档不重写；缺 `id` 的补齐。
- 旧引擎读 v3 会命中 `migrate_scene_doc` 的「版本高于支持范围」错误（明确失败，不静默降级）。
- `.prefab.json` 与场景共用同一套 `SceneJson` 形状与迁移，因此一并升级。

#### 编辑器职责（净删代码的位置）

| 操作 | v2（现状） | v3 |
|---|---|---|
| 移动实体 | 移动数组 + **全量重写所有 parent** | 只移动数组元素 |
| 改父级 | 走 `move_entity` 的下标重映射 | 只写目标实体的 `parent` 一个字段 |
| 删除 | 算 `removed[]` + `remap[]` 全量重写 | 移除行；把指向被删 id 的子级 `parent = null` |
| 复制 / 实例化 | 数组尾部追加 + 计算 `base` 偏移 | 子树分配连续 id 块，根挂到目标 parent |
| 存为预制体 | 提取子树并平移内部父索引 | 子树 id **重新编号为 `0..k`**（预制体自带 id 空间） |
| 同步预制体实例 | 按数组位置匹配 + 全量平移 | 替换子树，内部重新分配 id；`PrefabInstance` 元数据不变 |

#### 与 RSCN 的关系

**RSCN v4 不变**。运行时只按拓扑序建立层级，不需要 id：

- cook 仍写出 `parent: i32`（拓扑序下标），比存 id 更小、解码更快；
- `encode_scene` 用 `id → 拓扑位置` 的 `Vec` 映射（O(n)，见 §10.10 与 #29）；
- 只有在出现「运行时跨实体引用」（UI 事件绑定、脚本字段指向实体）时才考虑给 RSCN 加可选 id 表，
  在此之前不引入。

#### 否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 嵌套 `children` 树 | 与 ECS 平铺冲突；移动子树要搬整块数据；diff 噪声大；随机访问退化 |
| UUID / GUID | 单场景内无收益，diff 与可读性变差；冲突率在本项目可忽略 |
| 维持数组下标 | 编辑器每加一个层级功能就多一套 remap，成本线性增长，且无法跨场景引用 |
| 在 `components` 里存 `Entity` | 运行时句柄（index+generation）不可持久化，加载即失效 |

#### 验收判据

1. 所有现存 `.scene.json` / `.prefab.json` 迁移后层级**完全等价**（迁移前后 `topological_order`
   与父子关系快照一致）。
2. 移动 / 删除 / 复制 / 实例化的随机操作序列之后，`validate()` 通过且无悬空 id。
3. 编辑器 `scene.rs` 里 `remap` 相关代码行数下降 ≥ 60%（本决策的主要收益是**删代码**）。
4. 真实场景 cook + decode 的层级与迁移前一致（扩展 `prism-cooker` 的 `cooks_real_default_scene`）。

#### 分期

id 的语义和编辑器的下标重映射**不能分开落地**：只要 `parent` 从下标变成 id，
`move_entity` / `delete_entities` 里那套 `remap[]` 就会把 id 当索引用而静默改写。因此：

- **PR-1（数据层 + 编辑器，必须同一个 PR）**：
  - `prism-scene`：`id` 字段、`validate` 不变量、`migrate` v2→v3、`topological_order` / `encode_scene`
    走 `id → index` 映射、现有 `.scene.json` / `.prefab.json` 迁移、测试；
  - `prism-editor-api`：新建 / 复制 / 实例化时分配 id，删除全部 `remap[]` 路径，
    `extract_subtree` 重编号；
  - 前端只需确认 `parent` 仍是 `number`（语义变化对 TS 透明）。
- **PR-2（可选）**：RSCN 可选 id 表，仅在需要运行时实体引用时。

#### 未决问题

- `extract_subtree` 重编号后，预制体实例的「差异对比」是否要按结构路径而非 id 匹配 —— 留到
  预制体工作流真正落地时再定。

### 10.14 项目配置文件（`settings.ini` / `scenes.json`）

项目级配置只有两个文件，都放在 `assets/` 下：

| 文件 | 作用 | 格式 |
|------|------|------|
| `assets/settings.ini` | `[app]` / `[build]` / `[render]` / `[window]` 段 | 自写 INI 方言（见下） |
| `assets/scenes.json` | 场景清单（顺序 = 构建 = 启动顺序） | `{"scenes":[{"name","path","enabled"}]}` |

**为什么不用 TOML**：配置只有几十个标量字段，TOML 引入的是
`toml` + `toml_parser` + `winnow` + `toml_datetime` 整条链（玩家树里 60–80 KB）。
场景清单本来就是数组，`serde_json` 已经在树里（`.scene.json` / `.pak.meta.json`），
换成 JSON 不增加任何依赖。`serde_json` 保留，**`toml` 不再出现在任何 crate 里**
（`cargo tree -p prism-app -e normal | grep toml` 必须为空）。

#### 内部分辨率 / 整数缩放（`[render]`，#613）

2D 项目按设计分辨率作画（`projects/fishing` 是 640×360），但窗口往往大得多：
在 2000×1125 上直接按窗口分辨率渲染，最近邻放大会得到 3.125× 这种非整数比例，
一块美术像素变成大小不一的矩形。两个键把「渲染分辨率」和「输出分辨率」分开：

```ini
[render]
; 留空 / off / native = 按窗口分辨率渲染（默认，行为与之前完全一致）
internal_resolution = 640x360
; stretch = 铺满窗口（像素不等宽）；integer = 最大整数倍 + 居中黑边
internal_scale_mode = integer
```

- 两个轴都写了才生效；只写一边时另一边按窗口宽高比推导；
- 场景（ForwardPass 的 HDR / 深度 / 法线中间目标、路径追踪累积）按内部尺寸渲染，
  UI 叠加仍按窗口尺寸（文字保持清晰）；
- `PostPass` 把内部图放大到窗口：`stretch` 铺满，`integer` 取
  `min(窗口宽/内部宽, 窗口高/内部高)` 的整数倍并居中，四周留黑边；
  `integer` 时采样器切到最近邻，像素画不会被插值糊掉；
- 相机投影用**内部**宽高比（`RenderSettings::render_aspect`），否则 2D 构图会被拉伸；
- 编辑器 Game 视图走同一条链路（`EngineHost::render` + `GraphRenderer::set_internal_resolution`），
  所以 Play 看到的像素比例就是打包产物的比例。

#### INI 方言（唯一实现标准）

1. `[section]` 行，节内 `key = value`；节与节之间顺序无关；
2. 整行注释：`#` 或 `;` 开头（前导空白忽略）；**行内不解析注释**
   ——路径里的 `#` / `;` 不会被截断；
3. 值可加双引号，引号内支持 `\"` `\\` `\n`；不加引号时取到行尾并 trim；
   未知转义（如 `C:\temp`）保留反斜杠本体；
4. 解析器**只产出 `String`**；`u32` / `bool` / `f32` 转换由调用方做，
   失败时用默认值 + `log::warn!`（配置文件坏了也要能启动）；
5. 未知 section / 未知 key 一律忽略，`log::debug!` 记一条（编辑器会把
   `[build]` 写进同一份文件，引擎不该报错）；
6. 同一 section 出现两次：**合并**，同名 key 后者覆盖前者。

写回时数字 / 布尔裸写（`width = 1600`、`vsync = false`），其余字符串加引号
（`company = "Acme"`）；注释不在模型里，写回会丢（与之前的 TOML 往返一致）。

实现（三处，方言必须一致，改一处要同步另外两处）：

- `crates/prism-engine/src/config/ini.rs` —— 引擎读取（`AppConfig::from_ini`）；
- `crates/prism-editor-api/src/ini.rs` —— MCP / 工具的文档模型（保留未知段）；
- `editor/crates/prism-editor-gpui-core/src/ini.rs` —— 编辑器数据层（独立 workspace）。

#### 加载顺序与迁移

两个文件都遵循「新文件优先、旧文件只读回退」：

1. `assets/settings.ini` → 回退 `assets/settings.toml`（`log::warn!` 提示迁移）；
2. `assets/scenes.json` → 回退 `assets/scenes.toml`（同上；`.pak` 内的旧清单
   `scenes.toml` 同样可解析，所以旧场景包不会失效）。

**写端只产出新格式**：编辑器与构建流水线都不再写 `.toml`。

一次性迁移由 `prism-build-pipeline build-scenes` 完成：
`migrate_legacy_config(assets/)` 在**新文件不存在**时把 `settings.toml` 重排成
`settings.ini`、把 `scenes.toml` 转成 `scenes.json`，并打印一行 `migrated …`；
旧文件保留（引擎优先读新文件），确认无碍后可手工删除。迁移是幂等的，
重复执行不会再写。

---

## 11. 依赖预算（玩家树）

「轻量」必须被 CI 强制，否则一定会回涨。玩家二进制唯一的顶层依赖是
`prism-app`，它多带一个 crate，每个用户项目、每次 Android 打包就多带一个
（历史峰值：255 个 crate，含 rapier3d/parry3d、tokio、exr、jpeg 解码器）。

规则：

| 规则 | 落地 |
|------|------|
| 构建期依赖（`gltf` / `clap` / `rayon` / 图片编解码器）不得进入玩家树 | `scripts/check_deps.py` 黑名单 |
| 可选能力必须是 feature，默认关闭 | `prism-app/physics`、`prism-app/audio`、`prism-asset/async-io` |
| `image` 的 workspace 依赖零默认 codec，各 crate 只声明自己用到的格式 | 根 `Cargo.toml` |
| 玩家树 crate 总数 ≤ `MAX_PLAYER_CRATES`（当前 155） | 同上 |

检查在 `cargo check` 之外单独跑，只做依赖解析（不需要 `.spv`、不编译）：
`python3 scripts/check_deps.py`，已接入 `scripts/pr_ready.py` 与 CI 的 `deps` job。
超预算时**不要**调高常量：要么把依赖做成 feature，要么在 PR 里说明它为什么必须随玩家发布。

### 实测基线

桌面玩家二进制（`projects/prisma` 的 `prisma`，Termux aarch64，`cargo build --release`，
`lto = "thin"` + `opt-level = 3`）。复现：

```bash
cd projects/prisma && cargo build --release
ls -l target/release/prisma        # 未 strip
cp target/release/prisma /tmp/p && strip /tmp/p && ls -l /tmp/p   # strip 后（发布形态）
```

| 版本 | 玩家树 crate | 未 strip | strip 后 |
|------|-------------|---------|---------|
| 初始（rapier3d + tokio + exr/jpeg + 整条 audio 默认带） | 255 | 15.49 MB | 10.37 MB |
| #532 依赖瘦身（rapier/tokio/exr 移除） | 196 | 14.72 MB | 9.78 MB |
| #532 之后 + audio feature 化（默认关） | **144** | **10.38 MB** | **6.70 MB** |

总计：crate −44%，strip 后体积 −35%（10.37 MB → 6.70 MB）。

两次减重的收益结构完全不同，这决定了下一步该做什么：

- **构建期依赖（rapier / tokio / exr / jpeg）几乎不改变体积**：−59 个 crate 只换来
  −0.60 MB。别再为了体积去砍零散 crate。
- **audio 栈是体积大头**：一个 feature 开关就 −3.09 MB（strip 后 −32%），因为
  cpal + firewheel + symphonia（mp3/flac/vorbis/ogg/isomp4 全套编解码 + FFT 重采样）
  是真正的运行时大件。体积敏感的项目（小游戏、demo）关掉它即可。
- 剩下的 6.7 MB 主要是 winit / x11 / wayland 的窗口栈、ash + Vulkan 加载层、
  字体与 2D（fontdue / tiny-skia）——都是渲染与窗口真正需要的，除非做 headless 构建。

**结论**：依赖数量与编译时间是"默认清爽"的指标；**二进制体积靠 feature 关掉大件能力**，
不靠删零散依赖。

---

## 12. Lego：项目模板决定编译哪些子系统

引擎不替项目做取舍。**能力 = 砖块，`Cargo.toml` = 图纸**：项目模板挑哪些 feature，
就只编译、只链接哪些子系统。引擎自身的 `default` 保持「全量」，这样库和示例不会被
某个项目的口味绑架；精简来自**模板**，不是来自删默认值。

### 12.1 两层机制（不要混用）

| 层 | 机制 | 例子 |
|---|---|---|
| **子系统** | cargo feature（编译期，真的不生成代码/不链接依赖） | `pathtrace`、`audio`、`physics`、`async-io` |
| **组件** | `ComponentRegistry` 分组（运行期注册） | 场景里用不到的组件组不注册，加载时按「未知组件」处理 |

组件**不要**做成 cargo feature：N 个组件就是 2^N 个组合，CI 编不完、模板也写不出来。
组件级的取舍交给注册表分组 + 场景 cook（未注册组 = 不被提取，代码不链接）。

### 12.2 当前砖块

| 砖块 | feature | 默认 | 关掉后省什么 |
|---|---|---|---|
| 路径追踪 | `prism-render/pathtrace` | on | `pt_pass.rs`（~1.6k 行）+ 累积/BLAS-TLAS 资源 |
| 音频 | `prism-app/audio` + `prism-engine/audio` | off | cpal + firewheel + symphonia（~60 crate，−3.1 MB） |
| 物理 | `prism-app/physics` | off | rapier3d + parry3d（~40 crate） |
| 异步资产 IO | `prism-asset/async-io` | off | tokio |
| 窗口后端 | `prism-app/x11`、`prism-app/wayland`（透传 `prism-platform`） | 两个都 on | Linux 上只留一个：−0.46 MB（wayland 独留）～ −1.05 MB（x11 独留） |
| UI 叠加 | `prism-render/ui`（透传 `prism-app/ui`） | on | 字形图集 + ui_overlay 管线；关掉后 UI 命令照常填充、渲染侧忽略。文本光栅由下面的后端决定 |
| 文本光栅 A | `prism-render/ui-fontdue`（透传） | on | fontdue + ttf-parser（全平台，约 90 KB）；换成 `ui-directwrite` 可省掉 |
| 文本光栅 B | `prism-render/ui-directwrite`（透传，**Windows 专属**） | off | 用系统 `dwrite.dll` 光栅，fontdue/ttf-parser 不进二进制 |
| 组件组 3D | `prism-scene/group-3d`（透传 `prism-app`/`prism-engine`） | on | 灯光/天空盒/网格/相机控制/GI 配置不注册，serde 与反射代码不链接 |
| 组件组 2D | `prism-scene/group-2d` | on | 精灵/线段/精灵动画不注册 |
| 组件组 UI | `prism-scene/group-ui` | on | Node/Style/Text/布局类型不注册 |
| IBL | `prism-render/ibl`（透传） | on | 环境 cubemap 卷积/irradiance/prefilter/BRDF LUT/HDR 解码不编译；退化为 1x1 中性灰，descriptor 布局不变。3D 档 −64 KB |
| 天空盒 | `prism-render/skybox`（依赖 `ibl`） | on | `skybox.vert/frag.spv` + 管线；`SkyboxPass` 退化成空实现，调用点不变 |
| 阴影贴图 | `prism-render/shadows` | on | `shadow_map_pass` 不编译；forward 的 set 3 退化为 1x1 远深度 dummy |
| GTAO | `prism-render/gtao` | on | 半分辨率 AO pass 不编译；forward 的 set 4 退化为 1x1 白色 dummy |

**文本光栅后端**（`ui` 只决定「画不画 UI」，后端决定「谁来做字形位图」）：

| | `ui-fontdue`（默认） | `ui-directwrite` |
|---|---|---|
| 平台 | 全平台 | 仅 Windows（其它目标开它 = `compile_error!`） |
| 字形来源 | `fontdue` 读系统字体文件（msyh/Noto/DejaVu…）；`PRISMREV_UI_FONT` 可指定项目自带字体 | 系统 `IDWriteFactory` → `IDWriteBitmapRenderTarget`（GDI interop） |
| 字体族 | 按路径候选表 | `Microsoft YaHei` → `Microsoft YaHei UI` → `SimHei` → `Arial` → 兜底 `Segoe UI` |
| 度量 | `horizontal_line_metrics` | `DWRITE_FONT_METRICS` |
| 表单 | `fontdue::Font::rasterize` 直接给覆盖度 | 位图刷白 + 黑色灰度 AA 画字形，覆盖度 = `255 − R` |
| 额外依赖 | fontdue + ttf-parser（~90 KB） | `windows` 绑定（`Win32_Graphics_DirectWrite` / `Gdi`，只在 Windows 目标） |

两条硬规则：

1. **互斥**：`ui-fontdue` 与 `ui-directwrite` 同时打开是 `compile_error!`——否则
   fontdue/ttf-parser 会静默留在二进制里，开关等于没生效。
2. **只开 `ui` 不开任何后端**是合法组合：只画屏幕空间四边形、不画文字（省 90 KB），
   启动时 `warn` 一次说明缺哪个 feature。

两个后端都只做**单字形 cmap 查表 + 覆盖度光栅**，明确不支持：复杂文本整形
（阿拉伯 / 印度语系的连写与上下文替换）、可变字体 axis、Emoji 彩色字形、
kerning / ligature。需要这些能力要引入 HarfBuzz / `IDWriteTextLayout` 级的整形管线，
不在当前两个后端的范围内。DirectWrite 后端目前**只做过交叉编译验证**，
真机还需冒烟：文本清晰度（灰度 AA）、DPI 缩放、字体回退（无微软雅黑时走 `Segoe UI`）。

**待拆**（按收益排序，每拆一块都要进 §12.2 表格与 `check_features.py` 矩阵）：

- `2d` / `3d`：`SpritePass` 目前寄生在 `ForwardPass` 的 render pass 里，拆成独立 pass
  之后，2D 项目可以完全不编译 3D（forward/shadow/skybox/mesh/primitives）。
- `gi`（probe 烘焙）、`screenshot`。
- `ui`（fontdue/tiny-skia + `ui_overlay`）。

### 12.3 硬规则

1. **每个 feature 必须能被单独关掉并编译通过**——`scripts/check_features.py` 的矩阵 +
   CI 的 `features` job 就是这条规则的执行者；新增 feature 必须加一条最小组合。
2. **默认值只增不减**：新能力默认进 `default`，模板要精简就显式 `default-features = false`；
   不要为了某个项目的体积去改引擎默认值（会让其它项目静默失去功能）。
3. **关掉要可诊断**：feature 关闭导致某能力不可用时，启动/使用时 `log::warn!` 说明
   「需要开哪个 feature」，不要静默降级（`Subsystem::Audio` 已按此实现）。
4. **模板 = `Cargo.toml` 的 feature 列表 + `build_app()` 的子系统列表**；两者必须一致
   （开了 feature 却不装子系统等于白编译，装了子系统却没开 feature 会告警）。

### 12.4 体积账本（每块砖值多少字节）

方法：`scripts/size_report.py` 临时改写 `projects/prisma` 的 feature 列表、按同一
`release` profile 构建、记录未 strip 二进制大小。差值只来自 feature。

| 砖块 | Δ 二进制（未 strip） | 说明 |
|---|---|---|
| `audio`（cpal+firewheel+symphonia） | **+4.14 MB** | 体积大头，50 个 crate 的运行时栈 |
| `pathtrace` | +135 KB | 1.6k 行 + SPIR-V + BLAS/TLAS |
| `physics` | +3 KB | 代码是死代码（`physics_runner` 未接线），只多编译时间 |
| base（全关） | 10.24 MB | 见下方构成 |

`ui-fontdue` → `ui-directwrite`（Windows / `x86_64-pc-windows-gnu` / `dist` 档 +
`build-dist.sh` 的 build-std，**已 strip**）：

| 配置 | 二进制 | 差值 |
|---|---|---|
| `ui-fontdue`（默认） | 3 145 728 B | 基准 |
| `ui-directwrite` | 3 084 288 B | **−61 440 B（−60 KiB，−2.0 %）** |

省下来的是 fontdue + ttf-parser；`windows` 绑定（DirectWrite/GDI 那几十个 import）
抵掉了其中一部分，所以差距小于 §12.2 里估的「fontdue 约 90 KB」。两个数字都是同一
profile、同一 target、同一条构建命令，差值只来自文本后端。

按 crate 归因（`nm -S`，base 构建）：自研代码合计 **~731 KB**（`prism-render` 205 KB、
`prism-scene` 192 KB、`prism-ecs` 107 KB、`prism-engine` 98 KB、其余 ~130 KB），
其余 ~4.7 MB 是 Rust 基线 + 窗口栈 + 第三方。

两个必须记住的结论：

1. **代码行数 ≈ 体积是错的**。1.6k 行的路径追踪只值 135 KB；一个「没接线」的物理模块
   几乎为 0（链接器会删掉），但它仍然消耗 40 个 crate 的编译时间和依赖面。
2. **3D/2D 拆分对体积几乎无收益**（3D pass 合计 ~200 KB 量级），它的收益是
   **依赖面、编译时间、API 清晰度**。真正能改变二进制量级的是：
   第三方大件（audio 4.1 MB、UI/字体栈、窗口栈）、release profile（`panic = "abort"`、
   `lto = "fat"`、`codegen-units = 1`、strip）。
3. **组件不是体积单位**。组件是 POD 数据 + 反射元数据，`prism-scene` 整个 crate
   才 192 KB；关掉一个组件不改变二进制（代码共享、链接器本来就只保留被调用路径），
   只影响场景数据与注册表。组件级的取舍是**运行期复杂度**问题，不是体积问题。

### 12.5 发布配置分层（debug / release / dist）

构建配置也属于「图纸」的一部分：不同消费者用不同 profile，不要用一个 release 硬扛。

| profile | 用途 | 关键设置 |
|---|---|---|
| `dev` | 开发、编辑器 | `opt-level = 1`、`line-tables-only` 调试信息（依赖不带）、`panic = unwind` |
| `release` | 编辑器 / 工具 / CI | `opt-level = 3`、`lto = "thin"`、`panic = unwind` |
| `dist` | 玩家 / Android 打包 | `lto = "fat"`、`codegen-units = 1`、`panic = "abort"`、`strip = "symbols"` |

`dev` 只保留行号表：workspace crate 仍能定位 panic 的 file:line，依赖 crate 不带 DWARF。
完整调试信息会让 `target/` 与 `incremental/` 膨胀到数 GB，对日常调试的收益很低。

`panic = "abort"` **不能**进 `release`：editor 在进程内跑引擎，abort 会让整个编辑器
崩溃而不是让某个视口失败。用法：`cargo build --profile dist`（Android 打包脚本已切到它）。

实测（`projects/prisma`，默认 feature，Termux aarch64）：

| 配置 | 未 strip | strip 后 |
|---|---|---|
| `release`（thin LTO, cu=16, unwind） | 10.38 MB | 6.70 MB |
| `release` + fat LTO + cu=1（仍 unwind） | 7.41 MB（−28.6%） | 5.64 MB（−15.7%） |
| `dist`（再加 panic=abort, strip） | **6.61 MB（−36.3%）** | **5.12 MB（−23.5%）** |

结论：**收益主要来自 fat LTO + 单 codegen unit（−28.6%），不是 panic=abort**（后者再省
0.81 MB）。因此编辑器完全可以留在 `release`（保留 unwind 与回溯），只让玩家/Android 走 `dist`。
本机实测 fat LTO 的构建时间几乎没变（1m31s vs 1m35s），不必为它担心 CI。

#### 5 MB 是怎么构成的（`dist`，5.12 MB strip 后）

按 section（`size -A`）：`.text` 3.86 MB（75%）、`.rodata` 0.49 MB（10%）、
`.eh_frame` 0.35 MB（7%）、`.rela.dyn` 0.20 MB（4%）、`.data` 0.15 MB。

按代码来源（`nm -S` 归因，fat LTO 后符号会记在调用方，故为近似值）：

| 类别 | 大小 | 占比 | 可否再减 |
|---|---|---|---|
| Rust 基线（core/std/alloc/hashbrown/gimli） | 0.88 MB | 22% | 只能靠 `build-std` + panic_abort（nightly） |
| 引擎自研（prism_*） | 0.85 MB | 22% | 拆 feature |
| 窗口栈（winit/wayland/x11/smithay） | 0.84 MB | 21% | Linux 税；Windows 上换成系统 DLL，Android 换 android-activity |
| 其它第三方 | 0.49 MB | 12% | 个案处理 |
| **日志正则（env_logger → regex）** | **0.47 MB** | **12%** | **最容易的一刀：换掉 env_filter/regex 即可** |
| 字体/2D（tiny_skia/ttf_parser/fontdue） | 0.23 MB | 6% | `ui` feature；Windows 上换 `ui-directwrite`（−61 KiB） |
| 序列化（serde_json；`toml` 已移除，−128 KB） | ~0.02 MB | <1% | 场景文档/清单共用，基本到底 |
| 压缩（zstd/flate2） | 0.04 MB | 1% | 几乎不值得 |

**窗口栈按平台裁剪**（实测，`dist` strip 后）：

| 组合 | 二进制 | 说明 |
|---|---|---|
| X11 + Wayland（默认） | 5.12 MB | 两种会话都能启动 |
| 仅 Wayland | 4.66 MB（−0.46 MB） | 纯 X11 环境无法启动 |
| 仅 X11 | **4.07 MB（−1.05 MB）** | 纯 Wayland 环境需要 XWayland 兜底 |

- **Windows / Android 本来就不付这笔钱**：winit 的平台依赖按 target 裁剪，
  `cargo tree --target aarch64-linux-android` 里 wayland/x11 计数为 0，Windows 用系统 DLL。
- 只有 Linux 需要选，而且**必须选一个**（winit 无后端会编译失败），所以模板的最小组合是
  `--no-default-features --features x11`，而不是"全关"。

**Windows：可以完全不要 winit**（`win32-native`，2026-10）。`prism-platform` 里新增了
一个只用 Win32 API 的窗口后端（`RegisterClassExW` / `CreateWindowExW` / `PeekMessageW`
消息泵），`prism-app` 的 `winit` 变成可选依赖，两个后端共用同一套引擎/渲染/关闭逻辑，
事件先落到中立的 `PlatformEvent` 再进 `InputManager`。启用 `win32-native` 时依赖树里
winit 计数为 0（`cargo tree --target x86_64-pc-windows-gnu | grep -c winit`），
不是"编进来再被 LTO 删掉"。

实测（`bash scripts/build-dist.sh projects/prisma x86_64-pc-windows-gnu`，dist + strip）：

| Windows 窗口后端 | `prisma.exe` | 说明 |
|---|---|---|
| `winit-window`（默认） | 3,146,240 B（3.00 MB） | winit 0.30 + 系统 DLL |
| `win32-window`（`win32-native`） | **2,978,816 B（2.84 MB）** | −167,424 B（−163.5 KiB / −5.3%） |

- 比 Linux 那边的 `winit ≈ 375 KB` 小得多：Windows 上 winit 的窗口/消息/DPI 全靠
  user32/dwmapi，不像 Linux 要自带 x11/wayland/smithay 一整套协议栈。
- 代价：Win32 后端没有 IME、拖放、多显示器 DPI 平滑、任务栏进度、触摸；
  指针锁定是「隐藏 + ClipCursor + 回中」而非 Raw Input。见
  `crates/prism-platform/src/win32.rs` 的模块文档与 `AGENTS.md`。

结论：**5 MB 里真正属于「引擎」的只有 0.85 MB**，其余是语言基线、窗口栈和两个
可以替换的第三方件（regex 日志、字体栈）。下一步优先级：去掉 env_logger 的 regex
（−0.47 MB）> `ui` 砖块（−0.23 MB）> 序列化（−0.14 MB）。

#### 跨平台二进制体积（`dist` profile，strip 后）

| 目标 | 产物 | 大小 | text |
|---|---|---|---|
| Linux x86_64 / aarch64（x11 + wayland） | `prisma` | 5.12 MB | 4.97 MB |
| Linux（仅 x11） | `prisma` | 4.07 MB | — |
| Windows `x86_64-pc-windows-gnu`（winit） | `prisma.exe` | 4.11 MB | 4.09 MB |
| Windows `x86_64-pc-windows-gnu`（`win32-native`，无 winit） | `prisma.exe` | 见下方「窗口栈按平台裁剪」 | — |
| Android `aarch64-linux-android` | `libprisma.so` | **3.26 MB** | 3.15 MB |

- Windows 与「Linux x11-only」几乎一样（4.11 vs 4.07 MB），差值就是 wayland 栈——**印证了
  0.84 MB 的窗口栈只在 Linux 双后端时存在**。
- Android 最小（没 winit 平台栈，只有 NDK 胶水）；APK 体积另算（`.pak` 资产 + Java 壳）。
- 交叉编译在本机可复现：Windows 用 mingw 链接器；Android 用
  `scripts/make-ndk-host-native.sh` 处理过的 NDK（aarch64 主机 + x86_64 prebuilt shim），
  需要同时设 `CC_/CXX_/AR_aarch64_linux_android`（`android-activity` 的 build.rs 会找 `clang++`）。

#### 回收日志正则（env_logger → env_filter → regex）

`env_logger` 的默认特性 `regex` 只为 `RUST_LOG` 的正则过滤服务，却会拉进整套正则引擎
（regex-automata + regex-syntax + aho-corasick）。我们只用 `module=level` 这种指令，
`env_filter` 在关闭 `regex` 后会用**子串匹配**，语义足够。

- 根 workspace 的 `env_logger` 改为 `default-features = false`，构建工具显式打开
  `["auto-color", "humantime", "regex"]`，**玩家运行时只开 `["auto-color", "humantime"]`**。
- Android 还有第二条路径：`android_logger` 的默认特性同样带 `env_filter/regex`，
  一并关掉。
- 代价：`RUST_LOG` 里不能再写正则（如 `prism_(render|engine)=debug`），
  需要时给对应 crate 打开 `regex` 特性。

实测（`dist`，strip 后）：

| 平台 | 改前 | 改后 | Δ |
|---|---|---|---|
| Linux（x11+wayland） | 5.12 MB | **4.47 MB** | −0.66 MB（−12.8%） |
| Windows | 4.11 MB | **3.26 MB** | −0.85 MB（−20.7%） |
| Android | 3.26 MB | 3.26 MB | 0（regex 代码在 fat LTO 下本来就被回收，但仍占编译时间与依赖面） |

#### 剩余"水分"地图（`dist`，strip 后，Linux）

| crate / 来源 | 大小 | 说明 / 可减性 |
|---|---|---|
| `winit` | 375 KB | 平台后端本身；已可按 x11/wayland 裁剪 |
| `core` + `std` + `alloc` + `hashbrown` | 678 KB | Rust 基线，只能 nightly `build-std` |
| `prism_scene` | 244 KB | 组件注册表 + 全组件 serde：**下一个大件** |
| `wayland_client` / `x11_dl` / `x11rb` / `smithay` / `tiny_skia` | ~450 KB | 只在 Linux 双后端；x11-only 或 Windows 直接消失 |
| `prism_render` / `prism_engine` / `prism_app` / `prism_ecs` / `prism_reflect` | ~640 KB | 自研主体 |
| `serde_json` | 未单列 | `.scene.json` / `scenes.json` / `.pak.meta.json` 共用，必需（`toml` 已移除，实测 −128 KB） |
| `fontdue` 系（`ttf_parser` 等） | ~90 KB→64 KB | `ui` feature；Windows 上 `ui-directwrite` 换系统 DirectWrite（实测 −61 KiB） |
| `gimli` / `addr2line` | 33 KB | std 的 panic 回溯；需 build-std |
| `libloading` | 33 KB | ash 动态加载 `vulkan-1`；必需的代价 |
| `image` + png | ~21 KB | 截图；可做成 `screenshot` feature |
| `zstd` / flate2 / crc32 | ~44 KB | `.pak` 解压；资产压缩必需 |

按这个地图，**还能稳定拿到的**是：`screenshot`（~20 KB）、`prism_scene` 组件分组（百 KB 量级，
但要先设计注册表分组）。其余要么是平台必需的，要么要 nightly。

#### 预设档位（`dist`，strip，x11+wayland；crate 数含 workspace 内 crate）

| 档位 | feature 组合 | crate | 体积 |
|------|-------------|-------|------|
| 3D 全量 | `pathtrace,ui,x11,wayland,group-3d,group-2d,group-ui` | 148 | 4.47 MB |
| 2D + UI | `pathtrace,ui,x11,wayland,group-2d,group-ui` | 148 | 4.27 MB（−197 KB） |
| 2D 极简 | `pathtrace,x11,wayland,group-2d` | 141 | 4.14 MB（−331 KB） |
| 2D 极简 + 仅 X11 | `pathtrace,x11,group-2d` | 130 | 3.02 MB（−1.45 MB） |
| **2D 极简 + 仅 X11 + 无光追** | `x11,group-2d` | 128 | **2.89 MB（−1.58 MB，−35%）** |

这几档就是 hub「新建项目」向导的选项组合（2D/3D × UI × 路径追踪 × 窗口后端）。

**Wayland 是 Linux 上最大的单项**：`winit` 的 `sctk-adwaita`（客户端装饰）会带进
`tiny-skia` + `ttf-parser` + `wayland-client` 整条链，共约 1.1 MB。仅 X11 的构建
连字体光栅化都不需要——所以「Linux 极简」档直接把 2D 项目压到 3 MB 出头。**预设是语义承诺**：
往某一档里加 feature 等于给该档所有项目加负担，改动必须写清理由。

#### 组件分组的实测

| 组合 | 大小 | 说明 |
|---|---|---|
| 全部组（默认） | 4.47 MB | 3D + 2D + UI 组件都注册 |
| 关掉 `group-3d` | **4.27 MB（−197 KB）** | 灯光/天空盒/网格/GI 配置的 serde 与反射代码不再链接 |
| 关掉 3D 再关 UI | 再 −50 KB 量级 | Node/Style/Text |

**关键坑**：`prism-scene` 的默认特性会在任何直接依赖它的地方（`prism-engine`、三个用户项目）
把组件组重新打开，`prism-app` 的 `--no-default-features` 就失效了。所以凡是直接依赖
`prism-scene` 的地方都必须写 `default-features = false`——这与 `prism-game-api` 那次是同一类
错误，已在工程里全部修掉。

组件分组的收益之所以远小于「一个组件 20 KB」的直觉：serde 的反序列化代码在多个类型间
共享（同一个 `serde_json::from_value` 泛型实例化被 ICF 合并），关掉一组只回收该组独有的部分。

#### 3D pass 级裁剪：代码 −24 KB，但 strip 后文件不变

`ibl`（#543，−64 KB）之后，`shadow_map` / `gtao` / `skybox` 也做成了 feature
（`skybox` 依赖 `ibl`）。测法：临时把 `projects/prisma` 的 `prism-app` 改成
`default-features = false, features = ["x11", "group-3d", ...]`，同一 worktree、
同一 target、`--profile dist`（fat LTO + cu=1 + panic=abort + strip）。

| 组合 | 文件字节 | Δ 文件 | `.text`+`.rodata`+… 合计 | Δ 代码 |
|---|---|---|---|---|
| 全开（ibl+skybox+shadows+gtao） | 3,022,664 | — | 2,993,578 | — |
| 关 `shadows` | 3,022,664 | **0** | 2,988,274 | −5,304 |
| 关 `gtao` | 3,022,664 | **0** | 2,979,746 | −13,832 |
| 关 `skybox` | 3,022,664 | **0** | 2,988,426 | −5,152 |
| 全关（无 ibl/skybox/shadows/gtao） | **2,957,128** | −65,536 | 2,931,602 | −61,976 |

两个必须记住的点：

1. **代码真的少了**，但**文件一个字节都没少**：aarch64 的 LOAD 段按 64 KB 对齐
   （`readelf -l` 的 `Align 0x10000`），RW 段偏移跟着 RO 段末尾向上取整，≤33 KB 的
   收缩全部落进段间 padding。只有 `ibl`（−64 KB）跨过了 64 KB 的台阶。
   所以这三项**不能当体积预设的卖点**（写进预设等于承诺一个不存在的收益），
   只当能力开关：2D / 不需要阴影、AO、天空盒的项目少编译 24 KB 代码 + 3 个 SPIR-V，
   在 4 KB 对齐的平台（x86_64 / Windows）与 debug 构建上差额才可见。
2. 这也是为什么 **3D/2D 拆分的价值是依赖面、编译时间和 API 清晰度，不是二进制大小**——
   §12.4 的结论在这里再次成立。

数字用的是 stable `--profile dist`（与 §12.5 左列同口径）；`scripts/build-dist.sh` 的
build-std 档绝对值更小，但 64 KB 段对齐的结论不变。

剩下能动的只有两块：

- Rust 基线 ~678 KB（core/std/alloc/hashbrown）——要 nightly `build-std`，会抬高贡献门槛（§12.5）；
- `serde_json`（配置、场景清单、`.pak` 路径清单共用；`toml` 已整条移除，实测 −128 KB）——
  再想减小只能换二进制配置，作者体验会明显变差。

#### 发布档：nightly `-Z build-std`（`scripts/build-dist.sh`）

发布包里再拿回 9~10%：`-Z build-std=std,panic_abort` 让 std 和工程一起走 fat LTO，
`-Cpanic=immediate-abort` 删掉 panic 格式化（`core::fmt`）那一大片。

| 配置 | stable `dist` | build-std + immediate-abort | 变化 |
|------|--------------|------------------------------|------|
| 3D 全量 | 4,465,304 | 4,006,440 | −459 KB（−10.3%） |
| 2D 极简 + 仅 X11 | 2,891,032 | 2,628,768 | −262 KB（−9.1%） |

关键设计：**参数只走命令行，不写进任何 `Cargo.toml`**。
`panic = "immediate-abort"` 写进 profile 会让 stable cargo 直接拒绝解析整个清单，
而 `RUSTFLAGS="-Zunstable-options -Cpanic=immediate-abort"` 只影响这一次构建——
所以开发仍是 stable（`cargo run` 一行不变），只有 `scripts/build-dist.sh` 需要 nightly + `rust-src`。

体积账本（`§12.4`）里的 dist 数字默认指 stable 档；发布包按本节再乘 ~0.9。

#### 极限体积档：`--size`（竞赛 / 演示用）

```bash
bash scripts/build-dist.sh --size projects/prisma
powershell -File scripts/build-dist.ps1 -Size projects/prisma   # Windows 主机，等价
```

= `tiny` profile（`opt-level = "z"` + fat LTO + `cu=1` + `panic=abort` + strip）
+ `-C llvm-args=-inline-threshold=0 -C llvm-args=-inlinehint-threshold=0`
+ `build-std` + `immediate-abort`。

| 档位（aarch64 Linux，3D 全量） | 体积 |
|------|------|
| stable `dist`（opt-level 3） | 4,465,304 |
| `dist` + build-std（`build-dist.sh`） | 3,940,904 |
| **`tiny` + build-std（`--size`）** | **2,695,720（−40%）** |

代价：禁内联后 CPU 热路径慢 10–30%，GPU-bound 场景基本无感、CPU-bound 场景明显。
想两者兼得就给关键 crate 单独开回 `opt-level = 3`：

```toml
[profile.tiny.package.prism-engine]
opt-level = 3
```

**边界要说清**：`--size` 的现实区间是 **2.5–3.2 MB**。真正的 64k intro（30–60 KB）
靠的是 `no_std` + 手写系统 API + Crinkler 类 PE 压缩器，和这里的 flag 不是一回事；
不要在向导里把「体积档」宣传成竞赛级。

#### 压缩壳（`--pack`）

```bash
bash scripts/build-dist.sh --size --pack projects/prisma   # Linux / macOS 主机
powershell -File scripts/build-dist.ps1 -Pack projects/prisma   # Windows 主机
```

构建完成后对产物跑一次 `upx --best --lzma <bin>`，再打印压缩前后体积与压缩率。
它与 `--size` / `--profile` 正交，是**最后一道**可选工序，位置参数不变。

**只在 dist / 发布档使用，且产物必须做代码签名。** 这条不是建议而是前提：

- UPX 的本质是**自解压壳**：原二进制被压缩后写进一段 stub，运行时先在内存里解压再跳进去。
  所以「能运行」依赖 stub 会 `mprotect` 出一段可执行内存——这正是主流 EDR / 杀软
  检测「打包恶意软件」的经典启发式（UPX 也长期是恶意样本最爱的壳）。
- **未签名的压缩产物在 Windows Defender 上大概率直接被报毒**，而签名（Authenticode /
  带时间戳的代码签名证书）通常能把「未知壳」降级成「已知发行者」，这是唯一可靠的降级手段。
  没有证书就不要给 Windows 产物加 `--pack`。
- 部分 AV **即使签名后仍可能报**（尤其是启发式严格的企业环境）：这是外部因素，
  引擎侧无法修，发行前要自己在目标环境跑一次。
- 找不到 `upx` 时脚本**只提示、不失败**，并保留未压缩产物：`--pack` 是可选项，
  不该让 CI / 别人的机器因为缺个工具就变红。Android 产物（`lib*.so`）自动跳过——
  系统加载器 / Play 的扫描主导，压缩壳只换来误报和更慢的首次 page-in。

**代价**（接受不了就别开）：

| 代价 | 说明 |
|---|---|
| 启动时间 +10~30 ms | 首次解压 + 校验；冷启动、SSD 上更明显 |
| 内存峰值 | 解压那一瞬间要同时持有压缩数据 + 解压后的映像（近似 2× 代码段） |
| 调试困难 | 回栈全是 stub 地址，符号/strip 信息与运行时映像对不上；崩溃转储也别指望 |
| 反作弊 / 部分商店渠道 | 壳会触发签名一致性检查或人工审核，别在受管发行链上想当然 |
| 体积收益随配置变化 | 已经 fat LTO + strip 的产物压缩率通常 40%~55%，不是固定的 |

**不要**把 `--pack` 加进默认流程、CI 的 `dist-size` job 或任何 `[profile.*]`：
它换的是体积与启动时间，和「代码更小」不是同一类手段，混进默认路径只会让
「产物为什么被杀软拦了」变成一个没人能复现的问题。

一个实现细节：UPX **就地覆盖** `target/<target>/<profile>/<pkg>` 下的产物，
所以同一个 target 里再跑一次不带 `--pack` 的构建不会把它「解压」回来
（cargo 认为产物是最新的）；需要干净产物就改一行代码触发重新链接，或直接删掉文件。
