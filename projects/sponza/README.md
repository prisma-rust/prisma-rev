# PrismaRev Sponza Path Tracing & RTXGI

这是与 `projects/fishing/` 同属用户项目目录的独立 workspace，演示引擎的实时路径追踪管线与 NVIDIA RTXGI (SHARC v1.6) 全局光照。

## 架构与设计原则

1. **架构硬隔离**：
   - 渲染管线改动与 RTXGI 核心实现全部归属引擎层（`crates/prism-render` / `crates/prism-engine` / `crates/prism-app`），通过 Cargo features (`rtxgi`, `nrc`, `dlss`) 模块化开关编译。
   - 用户项目（`projects/sponza`）保持纯净（UI-framework-free），不引入第三方 UI 框架，通过引擎公开的 `FrameHook` 挂载测试调参 UI，只消费引擎暴露的设置与 API。
2. **双构建形态支持**：
   - `player`（默认）：静态玩家形态，链接引擎，构建桌面测试二进制或 Android JNI 库。
   - `preview-module`：编辑器预览动态模块形态，仅依赖跨 ABI 纯数据层，不链接引擎。
3. **资产方案（方案 A）**：
   - 采用轻量级经典 Crytek Sponza 模型（来自 RTXGI 官方 Assets），秒级加载，避免高负荷触发 GPU device lost。

## 运行方式

### 1. 资源构建

项目自带预打包的 `projects/sponza/assets/scenes.pak`，如需重新打包可运行：

```powershell
cargo run -q -p prism-build-pipeline -- build-scenes
```

### 2. 启动应用

在项目目录下启动：

```powershell
cargo run --manifest-path projects/sponza/Cargo.toml
```

可通过环境变量切换 feature 组合，例如仅使用基础路径追踪（关闭 RTXGI）：

```powershell
cargo run --manifest-path projects/sponza/Cargo.toml --no-default-features --features player
```

## 测试调参 UI (参考 RTXGI ImGui)

调参面板参考 NVIDIA RTXGI 官方示例（`Samples/Pathtracer/UI.cpp`）组织，包含以下功能分区：

### 1. 交互快捷键

| 按键 | 功能 |
| :--- | :--- |
| `F1` / `Tab` | 显示 / 隐藏完整调参面板（隐藏时显示紧凑状态栏） |
| `Up` / `Down` 或 `W` / `S` | 上下选择要调节的参数行 |
| `Left` / `Right` 或 `A` / `D` | 减少 / 增加选中参数的值 |
| `Space` / `Enter` | 切换布尔开关或执行动作 |
| `1` / `2` / `3` | 快速切换 GI 模式：`1` = Reference (纯 PT), `2` = SHaRC (RTXGI), `3` = NRC |
| `T` | 循环切换色调映射算子（Reinhard → ACES → Passthrough） |
| `R` | 手动触发路径追踪累积缓存重置 |
| `V` | 切换垂直同步 (VSync) |

### 2. 核心调参参数

- **Path Tracing**:
  - `GI Mode`: 全局光照算法选择（`Reference` 纯蒙特卡洛积分 / `SHaRC` 空间哈希辐射缓存 / `NRC` 神经网络缓存）。
  - `Max Bounces`: 路径最大弹射次数（1..24，默认 4）。
  - `Ray Max Dist`: 射线最远追踪距离（100..5000）。
  - `Tonemapper`: 色调映射模式（ACES / Reinhard / Passthrough）。
- **RTXGI SHaRC**:
  - `Enable Clear`: 每帧清零哈希网格与累积缓冲。
  - `Enable Update`: 允许主光追更新世界空间辐射缓存。
  - `Enable Resolve`: 允许 1D 计算管线合并时域样本并执行缓存淘汰。
  - `Anti-Firefly`: 开启抗噪高光抑制过滤。
  - `Scene Scale`: 控制体素空间大小（5.0..100.0，默认 50.0）。
  - `Roughness Thresh`: 粗糙度阈值（低于该阈值的镜面/光滑表面跳过缓存查询直接追踪，避免反射模糊）。
  - `Downscale Factor`: 稀疏更新降采样比例（默认 5，即约 4% 像素参与更新）。
  - `Accum Frame Num`: 时域累积平滑窗口帧数（1..120，默认 20）。
  - `Stale Frame Num`: 缓存槽位最大陈旧淘汰帧数（1..120，默认 60）。
- **DLSS**:
  - `DLSS Mode`: 深度学习超分辨率模式（Off / Quality / Balanced / Performance / Ultra Performance / DLAA）。
