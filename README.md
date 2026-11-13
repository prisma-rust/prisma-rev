# PrismaRev

> 从零打造的现代纯 Rust 游戏引擎。**Vulkan** 渲染，**Android 优先** + 桌面跨平台，**数据导向 ECS** 架构。

---

## 🌟 核心理念与技术特色

- **Android 优先与移动端友好**：深度适配移动端 GPU 特性（TBDR、Transient Attachment、显存带宽极致优化），Android 产物零额外运行时开销。
- **现代化 Vulkan 渲染体系**：
  - 模块化 **Render Graph** 驱动，自动屏障推导与 Pass 调度；
  - **Bindless 全局描述符索引**，告别传统状态切换与绑定槽位瓶颈；
  - **Slang 统一着色器工具链**，编译期强类型反射驱动 Rust 侧绑定代码生成；
  - 硬件级 **RayQuery 软阴影/环境光遮蔽**、路径追踪支持，集成 SHARC 世界空间辐射度缓存。
- **纯粹的数据导向设计（Data-Oriented ECS）**：
  - 高性能 Archetype ECS，结构体阵列（SoA）内存布局；
  - 自描述二进制格式（**RSCN v4** / **RTTIBin**），运行时反射系统支持零成本反序列化。
- **UI-Framework-Free 纯净运行时**：
  - 玩家端（Player）与核心引擎严格隔离 UI 框架，坚守极简依赖预算；
  - 构建端工具（Cooker / Asset Pipeline）与运行时彻底解耦。
- **原生 Rust 专用编辑器（`editor/`）**：
  - 基于 `egui` + `ash` 的原生桌面编辑器，支持 Split-Tree 灵活 Docking；
  - **Scene / Game 视图零拷贝采样**：离屏 `VkImage` 经由共享设备直接送入 egui 视口采样，无 CPU 回读开销；
  - 支持游戏工程动态预览模块（Preview Module）热切换与本地 TCP / MCP 自动化控制通道。
- **1.0 前零兼容负担（当前版本 0.1）**：
  - 在 1.0 版本正式发布前，**不考虑任何兼容性逻辑，旧版本兼容被明确视为冗余的垃圾代码**；
  - 架构与 API 重构坚决采用最新最优设计，不保留过渡垫片与废弃回退分支。

---

## 📁 核心架构布局

```text
crates/
  ├── prism-ecs               # 数据导向 ECS 核心（Entity/Component/World/Query）
  ├── prism-reflect           # 运行时反射与自描述二进制（RTTIBin）
  ├── prism-gfx               # 跨场景与渲染共享的纯数据渲染状态枚举
  ├── prism-scene             # 场景 Schema、组件注册表、RSCN 编解码
  ├── prism-ini               # 统一 settings.ini 方言解析与写回
  ├── prism-render            # Vulkan 渲染后端（Render Graph、Passes、Bindless、IBL、PBR）
  ├── prism-engine            # 引擎运行时（渲染系统、资源装载、音频接线、纯几何物理）
  ├── prism-physics2d         # 确定性 2D AABB 几何核心（Move-and-Slide，无外部引擎依赖）
  ├── prism-app               # 跨平台宿主（winit 事件循环、Android 宿主辅助、物理系统）
  ├── prism-platform          # 平台与窗口系统抽象（Vulkan Surface、输入路由）
  ├── prism-project-api       # 游戏工程 C ABI 契约（纯 POD 跨边界通信）
  ├── prism-runtime-host      # 动态模块宿主加载与 ABI 验证驱动
  ├── prism-game-api          # 游戏逻辑 API 抽象（单套代码适配 monolithic 与 dynamic 两种模式）
  ├── prism-anim-*            # 动画数学层（anim-ik 两骨 IK、anim-retarget 姿态重定向、motion-matching 动作匹配）
  ├── prism-cooker            # 离线资产烘焙（Importer、Pak 构造器、CookProfile）
  └── prism-editor-mcp        # 面向 AI 与工具的 MCP 自动化服务器

editor/                       # 独立 Workspace：纯 Rust 原生 egui 场景编辑器
launcher/                     # 独立 Workspace：Android 启动壳（Tauri 2 + Vue 3）
projects/template/            # 独立 Workspace：用户工程模板（桌面可执行程序 + Android cdylib）
assets/shaders/               # Slang 着色器源码与签入的 .spv / 反射元数据
```

---

## 🚀 快速开始

### 依赖环境

- **Rust**: Stable 工具链（见 `rust-toolchain.toml`）
- **Vulkan SDK**: 1.3+，包含着色器编译工具
- **Python**: 3.11+（用于自动化脚本与 `pr_ready.py` 自检门禁）
- **Node.js**: 20+ / pnpm（仅用于 `launcher/` Android 启动壳）

### 常用运行命令

```bash
# 1. 桌面端：构建 Shader、Workspace 并打包运行默认场景
scripts/run.ps1

# 2. 运行模板游戏工程（projects/template）
scripts/dev.sh

# 3. 启动 原生 egui 场景编辑器（独立 Workspace）
cd editor && cargo run -p prism-editor-egui

# 4. 运行全套单元测试与集成测试
cargo test
cargo test --manifest-path editor/Cargo.toml
```

---

## 📚 文档导航

完整文档地图请参阅 **[`docs/README.md`](docs/README.md)**。

| 领域 | 核心文档 | 说明 |
|---|---|---|
| **架构与设计总纲** | [`docs/DESIGN.md`](docs/DESIGN.md) | 引擎全局架构、Render Graph 拓扑与资产管线权威源头 |
| **反射与序列化规范** | [`docs/REFLECT.md`](docs/REFLECT.md) | 反射系统、RTTIBin 与 RSCN v4 布局约定 |
| **协作规范与工作流** | [`docs/ai-workflow.md`](docs/ai-workflow.md) | AI / 人类协同工作流、五条硬规则与提交规范 |
| **代码与注释风格** | [`docs/code-style.md`](docs/code-style.md) | 统一英文代码注释、删除无信息废话与冗余代码 |
| **版本控制与发布** | [`docs/versioning.md`](docs/versioning.md) | SemVer 规范、1.0 前零兼容逻辑与发版流程 |
| **技术约束与地图** | [`AGENTS.md`](AGENTS.md) | 严谨的代码地图、矩阵坐标系硬约束与协作红线 |
| **实战排障经验** | [`docs/lessons-learned.md`](docs/lessons-learned.md) | 踩坑与排障根因库（Vulkan 同步、生命周期等） |
| **渲染二分诊断** | [`docs/rendering-pipeline-investigation.md`](docs/rendering-pipeline-investigation.md) | 复杂画面异常排查的标准决策树与离线切片方法论 |

---

## 🤝 贡献与规范

在提交代码前，请务必阅读：
1. **[`CONTRIBUTING.md`](CONTRIBUTING.md)**：精炼的贡献者指引与不可逾越的红线。
2. **[`docs/ai-workflow.md`](docs/ai-workflow.md)**：提交前必须通过 `python scripts/pr_ready.py` 自检。
3. **零兼容代码红线**：1.0 版本前不考虑任何兼容性逻辑，直接使用最新最优方案重构并删除过时代码。

---

## 📄 开源许可证

PrismaRev 采用双重开源许可协议：
- MIT OR Apache-2.0
