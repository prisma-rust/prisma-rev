# PrismaRev 移动端光追 + 实时 GI 模块化管线设计

> 目标:移动端优先、画面质量优先、稳定 60fps。模块化渲染特性(光追可开关、GI 可切换)。
> 移植参考:TruvisRenderer 的 SHARC world-space radiance cache(v1.6, NVIDIA RTXGI 移植)与 RayQuery inline 用法。
> 本文件是**架构蓝图**,实现前需确认。所有 shader ABI 遵循 Truvis `.agents/rules/shader-abi-layout.md` 精神(显式 padding、跨端布局验证)。

---

## 0. 设计约束与决策

| 项 | 决策 |
|----|------|
| 管线架构 | **全新 RenderGraph**(模块化 pass 节点),抛弃现有 `renderer.rs` 单体(995 行) |
| 光追模型 | **RayQuery inline**(不是 RT pipeline)——移动端 RT pipeline 太重,inline query 只做 visibility,性价比最高 |
| 光追范围(首版) | **阴影 + 反射** 两项都做(用户决策) |
| GI 方案 | **直接移植 SHARC**(world-space hash-grid voxel radiance cache)——最现代、Truvis 已验证 |
| 基础层 | 永远跑的 **raster GBuffer → deferred/forward+ PBR**,质量优先 |
| 质量 vs 性能 | 质量优先,目标稳定 60fps(非极限性能) |
| TBDR 友好 | transient attachment + subpass 内联(见 §6) |
| Bindless | descriptor-indexing 运行时数组(现有 `bindless.rs` 升级为分离 SRV + 固定 sampler 数组,见 §5) |

---

## 1. 模块化管线拓扑

```
                         ┌─ [光追:关] ── 走 raster PBR 直接光照 + IBL
   Scene ──▶ GBuffer ────┤
   (raster)              ├─ [光追:开] ── RayQuery pass:
                         │                  · 软阴影 (visibility query vs TLAS)
                         │                  · 反射   (query vs TLAS, 采样命中材质)
                         │
                         └─ [GI 模式] ── SHARC cache:
                                       · Off     → IBL 仅环境
                                       · Update  → 稀疏 path 写缓存(画面=Off)
                                       · On      → 下一步扩展 bounce 查缓存
```

**RenderGraph 节点(模块化,可插拔):**
1. `GbufferPass`(raster,基础)—— 永远运行
2. `RayQueryPass`(compute,raster GBuffer 输入)—— 光追开关控制,输出 shadow/reflection 贡献
3. `SharcUpdatePass`(compute,RayQuery 驱动)—— GI=Update/On 时运行
4. `SharcResolvePass`(compute,1D dispatch)—— 跨帧合并/淘汰
5. `LightingPass`(raster/compute)—— 合成 direct(±RT) + IBL + SHARC GI → HDR
6. `PostPass`(tone map / SDR)—— 输出

每个 pass 是 `RenderPassNode` trait 实现,有 `inputs/outputs/execute()`。开关 = 节点是否加入图。

---

## 2. GBuffer 布局(移植 Truvis,Android 格式适配)

| Attachment | 格式 | 内容 |
|-----------|------|------|
| GBufferA | `R16G16B16A16_SFLOAT` | world normal.xyz + roughness |
| GBufferB | `R16G16B16A16_SFLOAT` | world pos.xyz + linear depth |
| GBufferC | `R8G8B8A8_UNORM` | albedo.rgb + metallic |

> 移动端注意:`RGBA32F` GBuffer 在 TBDR tile 内存压力大但可读;若带宽吃紧,GBufferA/B 可降为 `R10G10B10A2` + 法线 octa-pack(质量优先先保 SFLOAT)。

---

## 3. RayQuery 光追(移动端 hybrid)

**只做 inline RayQuery,不建 RT pipeline。**

### 3.1 阴影
```hlsl
RayQuery<RAY_FLAG_ACCEPT_FIRST_HIT_AND_END_SEARCH | RAY_FLAG_SKIP_CLOSEST_HIT_SHADER> rq;
rq.TraceRayInline(tlas, RAY_FLAG_NONE, 0xff, origin, t_min, dir, t_max);
rq.Proceed();
bool shadowed = rq.CommittedStatus() == STATUS_CANDIDATE_NON_OPAQUE
             || rq.CommittedStatus() == STATUS_COMMITTED_TRIANGLE_HIT;
```
- 每光源一次 query(或 clustered light 合批)
- α-test 材质:需 `RAY_FLAG_FORCE_OPAQUE` 或 non-opaque candidate(参考 Truvis `material_access.slangi` 的 alpha mask 处理)

### 3.2 反射
- 对粗糙表面:1 次 query 取命中点,采样命中材质 albedo/emissive + 递归查 SHARC GI
- 镜面:可做 1-bounce query,低粗糙度时查 SHARC 而非递归 ray
- 质量优先:反射可每像素 1 query(60fps 预算)

### 3.3 TLAS
- 移动端 `VK_KHR_acceleration_structure`:每帧从 PrismaECS mesh 组件构建/更新 BLAS+TLAS
- 实例 mask 区分 opaque / alpha-mask(匹配 Truvis `FORCE_OPAQUE` 约定)

---

## 4. SHARC GI(直接移植 Truvis,适配移动端)

### 4.1 三个持久 buffer(独立 descriptor set `SHARC_SET_NUM`)
| Buffer | 类型 | 用途 |
|--------|------|------|
| `sharc_hash_entries` | `RWStructuredBuffer<uint64>` | 空间 hash key(0=空槽),64-bit atomic 抢占 |
| `sharc_accumulation` | `RWStructuredBuffer<SharcAccumulationData>` | 本帧原子累积(u32 量化 radiance) |
| `sharc_resolved` | `RWStructuredBuffer<SharcPackedData>` | 跨帧累积(fp16 radiance + 帧元数据) |

元素类型(严格 ABI,来自 Truvis `realtime_rt.slangi`):
```hlsl
struct SharcAccumulationData { uint4 data; };           // xyz=量化radiance, w=sample数
struct SharcPackedData { float16_t4 radianceData; uint sampleData; uint sampleDataExt; };
```

### 4.2 移植文件映射
| Truvis 源 | PrismaRev 目标 |
|-----------|---------------|
| `sharc_common.slangi` | `shaders/slang/sharc/common.slang` |
| `sharc_hash_grid.slangi` | `shaders/slang/sharc/hash_grid.slang` |
| `sharc_integration.slangi` | `shaders/slang/sharc/integration.slang` |
| `realtime_rt.slangi`(buffer ABI 段) | `shaders/slang/sharc/buffers.slang` |
| RayQuery 调用(`raygen_direct_lighting`) | `shaders/slang/rt/ray_query.slang` |

### 4.3 移动端适配要点
- **64-bit atomics**:需 `shaderBufferInt64Atomics`(Android Vulkan 1.2+ Adreno/Mali 支持)。Truvis 已强制 `SHARC_ENABLE_64_BIT_ATOMICS=1`,我们同。
- **voxel 容量**:移动端显存小,容量设为桌面 1/4~1/8(如 2^20 而非 2^23),`sharc_scene_scale` 调粗
- **分辨率降采样**:SHARC Update/Query 可在半分辨率跑(质量优先可接受轻微模糊)
- **着色器编译**:SHARC 用 `#define SHARC_UPDATE 1` + push-constant `sharc_phase` 分阶段(Update/Resolve),同 Truvis,不拆多 pipeline

### 4.4 GI 模式状态机(移植 Truvis `realtime_rt` 常量)
```
SHARC_MODE_OFF     = 0  // 完全不碰 SHARC buffer,走 IBL
SHARC_MODE_UPDATE  = 1  // 只维护缓存,画面=Off
SHARC_MODE_ON      = 2  // 下一步扩展 bounce 查缓存
SHARC_PHASE_NONE/UPDATE/RESOLVE  // 同一条 compute dispatch 分阶段
```

---

## 5. Bindless 升级(分离 SRV + 固定 sampler)

现有 `bindless.rs`(combined `SamplerCube[]`)→ 升级为 Truvis 风格:
- `bindless_srvs[]`:`texture_2d` / `texture_cube` 分离采样图数组
- `global_samplers[]`:固定小型 sampler 数组(linear-wrap / linear-clamp / nearest / shadow)
- `BindlessSrv::sample(handle, uv, samplerType)` 内部 `NonUniformResourceIndex`
- **`INVALID_TEX_ID` fallback**:无效 handle → `INVALID_TEX_COLOR`(粉紫),避免读未定义描述符崩溃(移动端关键)

Rust 侧 `TextureHandle` 改为 `struct { u32 index }` + `INVALID` 常量。

---

## 6. TBDR 友好(移动端极致优化)

| 优化 | 做法 |
|------|------|
| Transient attachment | 所有 GBuffer/depth 用 `LAZILY_ALLOCATED`,不占系统内存 |
| Subpass 内联 | GBuffer → Lighting 同 renderpass,input attachment 传递,不写回主存 |
| 紧凑格式 | HDR 用 `R11G11B10` / `RGB10A2`,GBuffer 优先 SFLOAT(质量) |
| 避免全屏读回 | RayQuery/SHARC 走 compute,不强制 GBuffer resolve 到可读 |
| Tile 并行 | 光栅 GBuffer 天然 tile 友好;compute pass 注意 tile 局部性 |

> 60fps 稳定优先:若某中端 GPU 撑不住全特性,开关降级(RT 关 / GI=Off)即可,不牺牲架构。

---

## 7. 实现里程碑(建议顺序)

1. **RenderGraph 骨架**:`RenderPassNode` trait + 图构建 + 基础 GBuffer pass(raster PBR,质量优先) [完成]
2. **Bindless 升级**:`bindless.rs` → 分离 SRV/sampler + INVALID fallback [完成]
3. **TLAS 构建**:从 PrismaECS mesh 建 BLAS/TLAS(acceleration-structure 模块) [完成]
4. **RayQuery 阴影**:`RayQueryPass` + `rt/ray_query.slang`(阴影先,最稳) [完成]
5. **RayQuery 反射**:扩展 `RayQueryPass`(反射) [完成]
6. **SHARC GI (RTXGI v1.6)**:模块化 `rtxgi` feature + `sharc.rs` (1M entries Hash Grid + Resolve Pass) + Vulkan 1.2+ `buffer_device_address` / 64位原子操作支持 [完成]
7. **Sponza 验证与 Goldenframe 对比**:`projects/sponza` 用户项目接入 + 调参 UI + 离屏实测渲染与 RTXGI 官方参考图量化对比 [完成]
8. **Lighting 合成与降噪扩展**:direct(±RT) + IBL + SHARC 混合 [推进中]
9. **CI 扩展**:SHARC/GI shader 编译 + drift guard(复用现有 slang workflow) [完成]

---

## 8. Sponza 与 RTXGI SHARC 验证与 Goldenframe 规范

- **架构隔离**：渲染管线改动与 RTXGI 接入严格属于引擎层（`crates/prism-render` 中的 `sharc.rs` 与 `render_graph`），用户项目 `projects/sponza` 仅包含资产与调参 UI（`tuning_ui.rs`），不越权污染管线逻辑。
- **Goldenframe 规范对比**：
  - 测试套件：`crates/prism-render/tests/rtxgi_sponza_golden.rs`
  - 差分阈值：通道容差 12，4 倍增益热力图生成。
  - 产出构件：`sponza_raster.actual.png`、`sponza_pt_cache_off.actual.png`、`sponza_pt_sharc_on.actual.png`、`sponza_sharc_diff.diff.png`、`sponza_raster_vs_pt.diff.png`、`rtxgi_reference_diff.diff.png`。
  - RTXGI 官方参考图对比（1920x940）：变动像素 94.32%，平均差分 103.752，有效验证 SHARC 对间接光照 Monte Carlo 方差的平滑收敛能力。
