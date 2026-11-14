# PrismaRev Sponza Path Tracing & SHaRC

这是与 `projects/fishing/` 同属用户项目目录的独立 workspace，演示引擎的实时路径追踪管线与 NVIDIA RTXGI (SHARC v1.6) 全局光照。

## 架构与设计原则

1. **架构硬隔离**：
   - 渲染管线改动与 SHaRC 核心实现全部归属引擎层（`crates/prism-render` / `crates/prism-engine` / `crates/prism-app`），通过 Cargo features 模块化开关编译。当前只有 `sharc`（SHARC）有实现，`nrc` / `dlss` 是预留开关，见「实现范围与未接入功能」。
   - 用户项目（`projects/sponza`）保持纯净（UI-framework-free），不引入第三方 UI 框架，通过引擎公开的 `FrameHook` 挂载测试调参 UI，只消费引擎暴露的设置与 API。
2. **双构建形态支持**：
   - `player`（默认）：静态玩家形态，链接引擎，构建桌面测试二进制或 Android JNI 库。
   - `preview-module`：编辑器预览动态模块形态，仅依赖跨 ABI 纯数据层，不链接引擎。
3. **资产方案（方案 A）**：
   - 采用轻量级经典 Crytek Sponza 模型（来自 RTXGI 官方 Assets），秒级加载，避免高负荷触发 GPU device lost。

## 运行方式

> **别直接 `cargo run`**：运行时只从 `assets/scenes.pak` 读场景，没有磁盘回退。
> 缺包时窗口只显示清屏色（0.5 灰），日志里只有一条 `no resolvable scene`。

从仓库根一条命令搞定（打场景包 + 启动）：

```bash
bash scripts/dev.sh sponza
```

下面是手动分步。

### 1. 资源构建

`scenes.pak` 是生成产物（不入库），首次运行或修改场景文档后必须打包：

```powershell
# 一键脚本（位于 projects/sponza/ 目录下）：
.\rebuild-pak.ps1        # PowerShell
.\rebuild-pak.bat        # CMD / 双击
./rebuild-pak.sh         # Linux / macOS / Git Bash

# 或原生 Cargo 命令：
cargo run -q --manifest-path ../../Cargo.toml -p prism-build-pipeline -- build-scenes
```

`build-scenes` 按当前目录定位 `assets/`，所以在仓库根执行会打到根目录的 assets 上。

### 2. 启动应用

在项目目录下启动：

```powershell
cargo run
```

SHaRC 现在**默认编译并启用**（`default = ["player", "sharc"]`）。只想在运行时关掉 GI，
不用改编译选项：`SPONZA_RENDER_MODE=pt_off`（见下节）。

如果连编译都想去掉（`sharc.rs` + resolve 管线 + 3 个 GPU 缓冲不进二进制），用项目
自己的 `sharc` 开关——它是真的开关，会同时关掉 `prism-app/sharc` 与 `prism-engine/sharc`：

```powershell
cargo run --no-default-features --features player
```

### 3. 渲染模式

启动时可以用 `SPONZA_RENDER_MODE` 选默认模式：`raster` = 光栅（GI 关闭）、
`pt_off` = 路径追踪但不做全局光照、其它值或未设置 = 路径追踪 + SHaRC。

运行中不再需要重启：按 `M`，或选中面板里的 `Render Mode` 行用 `Left` / `Right`
即可在光栅与路径追踪之间切换（切换会重置路径追踪累积）。

## 测试调参 UI (参考 RTXGI ImGui)

调参面板参考 NVIDIA RTXGI 官方示例（`Samples/Pathtracer/UI.cpp`）组织，包含以下功能分区：

### 1. 交互快捷键

| 按键 | 功能 |
| :--- | :--- |
| `F1` / `Tab` | 显示 / 隐藏完整调参面板（隐藏时显示紧凑状态栏） |
| `Up` / `Down` | 上下选择要调节的参数行 |
| `Left` / `Right` | 减少 / 增加选中参数的值 |
| `Space` / `Enter` | 切换布尔开关或执行动作 |
| `1` / `2` / `3` / `4` | 快速切换 GI 模式：`1` = Reference (纯 PT), `2` = SHaRC, `3` = NRC（占位，暂无实现）, `4` = ReSTIR GI（光栅，复用同一个 SHaRC 缓存） |
| `T` | 循环切换色调映射算子（Reinhard → ACES → Passthrough） |
| `M` | 在光栅与路径追踪之间切换（会重置路径追踪累积） |
| `R` | 手动触发路径追踪累积缓存重置 |
| `V` | 切换垂直同步 (VSync) |

窗口拿到焦点即锁定鼠标（相机接管键鼠，`Esc` 解锁；点击画面也会锁定，切出去再切回来会自动恢复锁定）。**锁定态下面板自动收起**；`W` / `S` / `A` / `D` 严格按锁定状态分流，任何时刻只有一个消费者：锁定时只归相机（面板收着也不会被翻动），解锁时只翻面板菜单（相机不动）。想看参数按 `F1` / `Tab` 把面板叫回来，此时菜单用 `Up` / `Down` / `Left` / `Right` 导航。

`M` / `R` / `V` 三个功能键和 `F1` / `Tab` / `1` / `2` / `3` / `4` 一样任何状态都可用——相机只吃 `W` / `A` / `S` / `D` / `Q` / `E` / `Space` / `Ctrl` / `Shift`，这三个键不参与走位，锁定与否都不该屏蔽它们。

### 2. 核心调参参数

- **Render**:
  - `Render Mode`: 光栅 / 路径追踪切换（`Raster` / `Path Trace`）。
  - `Auto Exposure`: 直方图自动曝光开关（默认开；关闭后色调映射使用固定曝光，
    与引擎默认行为一致）。
- **Path Tracing**:
  - `GI Mode`: 全局光照算法选择（`Reference` 纯蒙特卡洛积分 / `SHaRC` 空间哈希辐射缓存 / `ReSTIR GI` 光栅路径实时 GI（复用同一个 SHaRC 缓存） / `NRC` 神经网络缓存，占位未接）。实际生效的是 `Reference`、`SHaRC` 与 `ReSTIR GI`。
  - `Max Bounces`: 路径最大弹射次数（1..24，默认 4）。
  - `Ray Max Dist`: 射线最远追踪距离（100..5000）。
  - `Tonemapper`: 色调映射模式（ACES / Reinhard / Passthrough）。
- **SHaRC（NVIDIA RTXGI v1.6）**:
  - `Enable Clear`: 每帧清零哈希网格与累积缓冲。
  - `Enable Update`: 允许主光追更新世界空间辐射缓存。
  - `Enable Resolve`: 允许 1D 计算管线合并时域样本并执行缓存淘汰。
  - `Anti-Firefly`: 开启抗噪高光抑制过滤。
  - `Scene Scale`: 控制体素空间大小（5.0..100.0，默认 50.0）。
  - `Roughness Thresh`: 粗糙度阈值（低于该阈值的镜面/光滑表面跳过缓存查询直接追踪，避免反射模糊）。
  - `Downscale Factor`: 稀疏更新降采样比例（默认 5，即约 4% 像素参与更新）。
  - `Accum Frame Num`: 时域累积平滑窗口帧数（1..120，默认 20）。
  - `Stale Frame Num`: 缓存槽位最大陈旧淘汰帧数（1..120，默认 60）。
- **DLSS**（占位，暂无实现）:
  - `DLSS Mode`: 深度学习超分辨率模式（Off / Quality / Balanced / Performance / Ultra Performance / DLAA）。切换只更新枚举值，不改变渲染结果。

## 实现范围与未接入功能

对齐目标是 RTXGI v2.9.0。该版本已把 DDGI 拆到独立的 RTXGI-DDGI 仓库，主线是
**世界空间辐射缓存 SHARC**，不是探针辐照度体积。当前接入情况：

| 功能 | 状态 | 说明 |
| :--- | :--- | :--- |
| Reference 纯路径追踪 | 已接入 | 蒙特卡洛积分，无辐射缓存。 |
| SHARC (`sharc`) | 已接入 | 哈希网格 + 时域累积 + `resolve`，已接进路径追踪积分器的弹射循环。 |
| 直方图自动曝光 | 已接入 | 移植自 RTXGI `AutoExposure.hlsl`，256 档对数亮度直方图。 |
| DDGI | 未接入 | 不在 RTXGI v2.9.0 内；引擎侧的探针表示保留在 `crates/prism-render/src/gi.rs`，但尚无烘焙数据加载路径。 |
| NRC (`nrc`) | 未接入 | NVIDIA Neural Radiance Cache 是闭源的 Tensor Core 二进制 SDK，既未内置也不可再分发，没有可链接的实现。 |
| DLSS / Streamline / NGX (`dlss`) | 未接入 | 需要 NVIDIA Streamline SDK 与驱动侧分发许可，不在依赖树内；仓库内也没有降噪器或超分器。 |

因此：

- `nrc` / `dlss` Cargo features 目前是空开关，没有任何 `#[cfg(feature = "nrc")]` / `#[cfg(feature = "dlss")]` 实现；`dlss_mode` 在渲染管线里也没有读取点。
- 调参 UI 的 `NRC (Neural)`、`DLSS Mode` 各项以及快捷键 `3` 只是枚举占位，切换后不会改变渲染输出。
- NVIDIA 参考图经过了 NRD 降噪与 DLSS Ray Reconstruction，本仓库输出是未降噪的原始累积，噪点与亮度分布不能逐像素对比。
