# PrismaRev RTX 神经渲染接入设计（DLSS / 光线重构 / 帧生成）

> 目标：在**不破坏 mobile-first 与能力驱动降级**的前提下，把 NVIDIA RTX 神经渲染三件套接进 RenderGraph：
> DLSS-SR（超分）、DLSS-RR（光线重构）、DLSS-G（帧生成）。
> 参考实现：[NVIDIA-RTX/RTXPT](https://github.com/NVIDIA-RTX/RTXPT)（nvrhi + Donut，DX12/Vulkan 双后端）。
> **只借它的数据契约，不借代码结构**：RTXPT 是 C++/nvrhi，本引擎是 ash 直连 Vulkan，两者在 pass 编排层没有可复用的代码。

本文是 **L2 子系统契约**：规定 guide buffer 的图资源句柄、格式、生产/消费关系与里程碑切分。
实现细节以代码为准，本文与代码在同一个 PR 内同步更新。

---

## 0. 设计约束与决策

| 项 | 决策 | 理由 |
|----|------|------|
| 接入层 | **Streamline 2.x 动态加载**（仿 `crates/prism-render/src/anf.rs` 的 `AnfBinding` 函数指针表） | 与 `DlssMode` 枚举、`dlss` feature 开关共存；不装 SDK 的机器零影响 |
| 平台分支 | **不写 `#[cfg(target_os)]`**，探测集中在 `crates/prism-render/src/capabilities.rs` | DESIGN.md §2.1 硬约束 |
| 失败行为 | 缺 SDK / 非 NVIDIA / 驱动过旧 → 逐级降级到 TAA，**不静默黑屏** | 与 ANF 的 `AnfCaps::probe` 同一模式 |
| 顺序 | **先补齐 PT 的 guide buffer，再接 SDK** | guide 是 DLSS-RR 的输入契约；没有 guide 就没有 RR，也不会有 NRD 备选 |
| 拒绝 | 不把 NGX 原生 API 焊进 `pt_pass.rs` | Streamline 已封装 SR/RR/G + Reflex，NGX 是更低的、不可移植的一层 |
| 拒绝 | 不做"DLSS 专用渲染路径" | 一套管线多端：DLSS 是 `PostPass` 之前的**可选替换**，不是分支 |

---

## 1. 现状盘点（dev@18707482）

**已有**

- `crates/prism-render/src/render_graph.rs:37-55` 固定图资源句柄表：`FORWARD_DEPTH_H`=1000、`FORWARD_NORMAL_H`=1001、`FORWARD_COLOR_H`=1002、`PT_COLOR_H`=1003、`SHADOW_MAP_H`=1004、`TAA_COLOR_H`=1005、`FORWARD_VELOCITY_H`=1006、`ANF_COLOR_H`=1007。
- `crates/prism-render/src/render_graph.rs:251-261` 已定义 `DlssMode`（Off / Quality / Balanced / Performance / UltraPerformance / Dlaa），`RenderSettings::dlss_mode` 默认 `Off`（`render_graph.rs:307-308`）。**只有枚举，没有实现。**
- `crates/prism-render/Cargo.toml` 已声明空 feature `dlss = []`（默认不开）。
- `crates/prism-render/src/capabilities.rs:31` `RayTracingCaps` 已做 4 层光追探测，`capabilities.rs:165` `rt_extension_names()` 提供按需启用扩展名的范式。
- `crates/prism-render/src/taa.rs` 完整 TAA（3×3 邻域 clamp、reproject、velocity 优先、reactive 掩码），`assets/shaders/slang/taa.slang:99-137` 是它的 shader。
- `crates/prism-render/src/forward_pass.rs` 光栅路径已输出 velocity（`FORWARD_VELOCITY_H`），normal.w 用作 transparent/reactive 标记。
- `crates/prism-render/src/anf.rs:42-57` `AnfCaps::probe` + `AnfBinding` 动态加载，是高通分支的现成模板（SR + FG 双能力位）。
- `crates/prism-render/src/dummy.rs` `SolidImage` 提供 1×1 常量图，用于特性关闭时保持 descriptor layout 不变。

**缺失（本设计要补的）**

1. `PathTracePass` 只发布 `PT_COLOR_H`（`pt_pass.rs:1490-1492`），**不产深度与运动矢量** → PT 模式无重投影基础，TAA 在 PT 模式下被 `post.rs:962-967` 的 `!is_pt` 硬闸门挡掉。
2. 没有 roughness 通路（只有 normal，且 `.w` 已被 reactive 占用）。
3. PT 着色没有 path-space 分层（diffuse/specular 混合写进同一个 accumulator），出不了 RR 需要的分层 radiance 与 hit distance。
4. 无 Streamline / NGX 接入。

---

## 2. 降级阶梯

能力驱动降级，从高到低，每一级都必须在缺少上一级能力时**自动**成立：

```
DLSS-RR（NVIDIA + Streamline + 驱动支持 RR）
   ↓ 无 RR
NRD（开源、免授权，AMD/Intel/NVIDIA 通用）+ DLSS-SR
   ↓ 无 DLSS
TAA（已有，taa.rs）
   ↓ 关闭 TAA
直出（PostPass 读 PT_COLOR_H / FORWARD_COLOR_H）
```

ANF 是同一阶梯在高通 Adreno 上的分支（SR + FG），不参与上述顺序，由 `AnfCaps` 单独决定。

**关键决策**：NRD 与 DLSS-RR 共用同一套 guide buffer（§3）。因此 PR1–PR3 产出的图资源不绑定任何 NVIDIA SDK，
在 AMD/Intel 上由 NRD 消费，在 NVIDIA 上由 DLSS-RR 消费。这是"一套管线多端"原则在本特性的落地方式。

---

## 3. Guide buffer 契约

新增句柄从 1008 起顺延（`render_graph.rs:55` 之后）。**格式一旦定下就是 ABI**：改格式等于改 shader 与所有消费者。

| 句柄 | 值 | 格式 | 生产者 | 消费者 | 里程碑 |
|------|----|------|--------|--------|--------|
| `PT_DEPTH_H` | 1008 | `R32_SFLOAT`（NDC 深度，与 `FORWARD_DEPTH_H` 同语义） | `PathTracePass` | TAA / NRD / DLSS-RR | PR1 |
| `PT_VELOCITY_H` | 1009 | `R32G32_SFLOAT`（屏幕空间 UV 位移，与 `FORWARD_VELOCITY_H` 同单位）。选 32 位是因为 `R16G16_SFLOAT` 不在 Vulkan 强制 `STORAGE_IMAGE` 格式表里，而本 pass 用 compute 写 | `PathTracePass` | TAA / NRD / DLSS-RR / DLSS-G | PR1 |
| `PT_NORMAL_ROUGHNESS_H` | 1010 | `R16G16B16A16_SFLOAT`（view-space normal.xyz + roughness.w） | `PathTracePass` | NRD / DLSS-RR | PR2 |
| `PT_DIFFUSE_H` | 1011 | `R16G16B16A16_SFLOAT`（diffuse 路径 radiance） | `PathTracePass` | NRD / DLSS-RR | PR3 |
| `PT_SPECULAR_H` | 1012 | `R16G16B16A16_SFLOAT`（specular 路径 radiance） | `PathTracePass` | NRD / DLSS-RR | PR3 |
| `PT_HIT_DISTANCE_H` | 1013 | `R16_SFLOAT`（主命中点距离，RR 的 `kBufferTypeDepth` 配套） | `PathTracePass` | DLSS-RR | PR3 |

约定：

- **不做格式版本号**。1.0 前零兼容（`docs/versioning.md`）：格式变了就改句柄格式与全部消费者，不保留双格式分支。
- 句柄在 `PathTracePass::setup`（`pt_pass.rs:1290-1314`）里用 `create_resource_at` 声明；`execute` 内手工 barrier + `set_image_view` 发布，与 `PT_COLOR_H` 现有写法一致。
- **resize 必须覆盖新图**：`pt_pass.rs:718-762` `resize_images` 现在只重建 accum / sampleCount / output 三组，新增图要一起加进去，否则换分辨率会持有一张旧尺寸 image。
- 特性关闭时用 `dummy.rs` 的 1×1 常量图占位，保证 descriptor layout 不随开关变化（`dlss = []` 关闭时同样成立）。
- **法线空间 = view space，roughness = 感知值（perceptual）**。依据：`sl_dlss_d.h:83-87` 的 `worldToCameraView` / `cameraViewToWorld` 说明 DLSS-RR 的 `kBufferTypeNormals` 期望相机空间；`ePacked` 模式（`sl_dlss_d.h:53-59`）把 roughness 放进 normal 的 w 通道，正是本表的布局。roughness 写 `SurfaceMaterial.roughness`（已 clamp 到 `0.04..1.0` 的艺术空间值），**不要**平方成 GGX 的 `alpha`：`path_integrator.slang` 只在 `ndf_ggx` 等函数内部平方。RTXPT 走的是"world normal + 真实 view 矩阵"的等价路线（`PrepareGuideBuffers.hlsl:54`），两种都合法，本仓库沿用 `scene_frag.slang:509-514` 既有的 view-space G-buffer ABI。
- 主射线 miss 的像素写 `float4(normalize(-view_ray_dir), 0.5)`（朝向相机的一个假平面，与 RTXPT `PrepareGuideBuffers.hlsl:56` 的 `float4(-normalize(rayDir), 0.5)` 一致）：背景没有表面，但全零法线会让去噪器把它当成退化平面。

---

## 4. 管线拓扑

```
                   ┌─ 光栅 ─ ForwardPass ──────────────┐
Camera ──▶ TLAS ───┤                                    ├─▶ [AA/超分/重建] ─▶ PostPass ─▶ Present
                   └─ 光追 ─ PathTracePass ─────────────┘        │
                              │                                   ├─ DLSS-RR  (NVIDIA + SL)
                              ├─ PT_COLOR_H ──────────┐           ├─ NRD      (通用)
                              ├─ PT_DEPTH_H           ├──────────▶└─ TAA      (已有)
                              ├─ PT_VELOCITY_H        │
                              ├─ PT_NORMAL_ROUGHNESS_H│
                              ├─ PT_DIFFUSE/SPECULAR_H│
                              └─ PT_HIT_DISTANCE_H ───┘
```

注册位置沿用现有顺序（`graph_renderer.rs:722-743`）：`ForwardPass → GtaoPass → AnfPass → RtScheduler → PathTracePass → TaaPass → AutoExposure → PostPass`。
`TaaPass` 排在 `PathTracePass` 之后（PR1 调整）：resolve 要消费路径追踪写出的 depth / velocity guide，注册顺序就是执行顺序。
新增的神经渲染 pass 插在 **`PathTracePass` 之后、`AutoExposure` 之前**，即"颜色已经最终确定、还没做曝光与 tonemap"的位置——
DLSS 要求线性 HDR 输入，`AutoExposure` 之后就不合法了。

---

## 5. 里程碑

每个 PR 独立可合并、可回滚，diff 控制在 ~500 行内（`AGENTS.md` 硬规则）。

### PR1 — PT 输出深度与运动矢量（无 NVIDIA 依赖）

- 新增 `PT_DEPTH_H` / `PT_VELOCITY_H`；`pt_render.slang` 在 `ptMain` 尾部（累积写出之前）增加两个 `RWTexture2D` 写入（set 0 binding 15/16），并用新增的 set 0 binding 17（`PtGuides` UBO：`viewProj` + `prevViewProj`）在 CPU 侧提供两帧投影矩阵。之所以不用 push constant：`PtPush` 已占满 176 字节，`pt_pass_tests.rs` 与 `push_constant_sizes.rs` 都把它钉死为契约。
- 深度：`float4 clip = mul(view_proj, float4(hit_pos, 1.0)); depth = clip.z / clip.w;`，与光栅 `D32_SFLOAT` 同语义；主射线 miss 写 1.0。
- 运动矢量：用**去 jitter 的**当前 `view_proj` 与上一帧 `prev_view_proj` 对 `hit_pos` 求两次投影，`velocity = uv - prevUv`；miss 写 0。
- 上一帧矩阵走 **set 0 新增 UBO（binding 17，128 字节：cur_view_proj + prev_view_proj）**，由 `PathTracePass` 每帧通过映射指针覆写（单块 host-visible buffer + CPU 侧缓存上一帧矩阵，不需要 `taa.rs` 那样的双缓冲，因为这张 UBO 只在 dispatch 内被读）。**不扩 push constant**：`PT_PUSH_RANGE_SIZE = 176`（`pt_pass.rs`）已经贴着 `max_push_constants_size` 的常见上限，再涨会撞 `context.rs` 的 176 校验。
- 解开 `post.rs:962-967` 的 `!is_pt` 闸门，让 PT 模式也能读 `TAA_COLOR_H`。
- 验收：Sponza PT 模式开 TAA 后静态画面无鬼影、相机运动无拖影；`cargo test -p prism-render --test taa_gpu` 通过。

### PR2 — 法线 + roughness 打包

- 新增 `PT_NORMAL_ROUGHNESS_H`；PT shader 写 view-space normal 与感知 roughness。
- **不扩 push constant**：`PtGuides` UBO 从 128 字节扩到 192 字节，多一个 `worldToView` 矩阵。CPU 侧从 `GraphFrame::inv_view_rot`（view 旋转的转置）转置回来，因此 shader 里的写法与 `scene_frag.slang` 的 `mul((float3x3)frame.view, worldNormal)` 完全一致。
- 注意 `FORWARD_NORMAL_H.w` 已被 TAA 当 reactive 用（`taa.slang:113`），**不能**把 roughness 塞进去——这是本 PR 另起一张图而不是复用 G-buffer 的原因。
- **光栅侧 MRT 推迟到 PR5**：DLSS-RR 只消费路径追踪的输出，现在给 `ForwardPass` 加一路 MRT 会立刻多付全分辨率 RGBA16F 的带宽却没人读。等 PR5 真正接 RR 时再按当时的光栅 G-buffer 形态补（届时可能与 ReSTIR GI 的 `FORWARD_ALBEDO_H` 合并成一张 surface 图）。

### PR3 — 路径空间分层

- `path_integrator.slang` 按首次 bounce 的 lobe 把 `throughput * radiance` 累到 diffuse 或 specular 两个 accumulator（`SurfaceMaterial` 已有 metal-roughness，判断用现有的 BRDF 选择分支）。
- 新增 `PT_DIFFUSE_H` / `PT_SPECULAR_H` / `PT_HIT_DISTANCE_H`；`PT_COLOR_H` 仍写总和（Post 直出路径不变）。
- 分层会翻倍 accumulator 带宽，移动端需在 `capabilities.rs` 之外用 `RenderSettings` 开关控制（默认跟随 `dlss_mode != Off || nrd`）。

### PR4 — Streamline 动态加载层（先只接 DLSS-SR）

- 新建 `crates/prism-render/src/dlss.rs`，形状照 `anf.rs`：`DlssCaps { available, sr, rr, fg }` + `probe(properties, driver_version)` + `DlssBinding` 函数指针表，加载失败返回 `None` 而不是 panic。
- **Vulkan 特有难点**：Streamline 通过 interposer 包装 `vkCreateDevice` / `vkCreateSwapchainKHR` / `vkQueuePresentKHR`。ash 是直接 `vkGetInstanceProcAddr`，所以要在 `context.rs` 创建 instance 后**把 GIPA 换成 SL 的**，并且 swapchain 与 present（`swapchain.rs` 的 `Swapchain::present`）必须走 interposer 返回的函数指针。这是 PR4 的全部风险所在，先做 DLSS-SR 验证通路，再扩 RR/G。
- 每帧 `sl::setTagForFrame`：color / depth / velocity / jitter / `inv_view_proj` / `prev_view_proj` / `sl::kBufferTypeScalingOutputColor`。
- `DlssMode` 映射到 SL 的 quality 枚举；`Dlaa` = 输出尺寸与输入相同。

### PR5 — DLSS-RR（光线重构）

- 五路信号按 RTXPT 的契约接：`kBufferTypeScalingInputColor`、`kBufferTypeAlbedo`（diffuse）、`kBufferTypeSpecularAlbedo`、`kBufferTypeNormals` + `DLSSDNormalRoughnessMode::ePacked`、`kBufferTypeSpecularMotionVectors`，外加 `kBufferTypeTransparencyLayer` 与 `kBufferTypeDepth`。
- **打包模式的确切通道顺序必须在动手前对照 `sl_dlss_d.h` 的 `ePacked` 注释核实**，不要凭记忆写；核实结果回填到本文 §3 表格。
- RR 输入深度/运动矢量要用**副本**：RR 会改写它们的尺度（RTXPT 的 `EnsureRRDepthAndMotionVectors()` 就是为此复制一份），不能直接喂 `PT_DEPTH_H`。

### PR6 — DLSS-G（帧生成）

- 需要 present 支持"一次提交输出 N 帧"：`swapchain.rs` 的 `Swapchain::present` 目前是单次 `queue_present`，要改成由 SL 驱动并读回 `numFramesActuallyPresented`。
- 需要 `inputsProcessingCompletionFence` 与 UI/色调分离层（HUD 必须在插值帧上正确合成）。
- 依赖 PR4/PR5 稳定后再做；FG 的失败模式（画面撕裂/卡顿）比 RR 严重得多。

---

## 6. 验证

- 单元：`pt_pass_tests.rs` 的 push/push-constant 契约测试必须继续通过（PR1 不动 push 就是为了这个）。
- GPU：`cargo test -p prism-render --test taa_gpu`（PR1）、`golden_frame`（PT 颜色不变）。
- 手工：Sponza `SPONZA_RENDER_MODE` 三态（raster / pt_off / 默认 PathTrace+SHaRC）各跑一遍，DLSS 各级别与 Off 逐一切换，确认降级不黑屏。
- 面板：`projects/sponza/src/tuning_ui.rs` 已有一行 `DLSS Mode` 循环（Off/Quality/Balanced/Performance/UltraPerformance/Dlaa），PR4 之后这一行才真正生效。

---

## 7. 明确不做

- 不做 DLSS 的 DX12 后端（本引擎只有 Vulkan）。
- 不做 RTXPT 的 `nvrhi` / Donut 移植（框架层不兼容，收益为零）。
- 不做"检测到 NVIDIA 就强制开"的隐式行为：`dlss_mode` 默认 `Off`，必须显式打开。
- 不保留旧 guide 格式的兼容分支（1.0 前零兼容）。
