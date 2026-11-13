# PrismaRev 文档体系与目录规范

本文档是 `docs/` 目录的**总览地图**与**文档工程规范**，规定了全仓库文档的内容尺度分级、目录结构规范、文件命名格式及知识维护原则。

---

## 🧭 一、核心维护原则

1. **长期有效原则**：`docs/` 只沉淀具有长期架构指导价值、设计契约和工程规范的文档。会话临时计划、实施清单与临时备忘按 [`ai-workflow.md`](ai-workflow.md) §9 严格存放在 `.zcode/plans/`（已 gitignored）。一旦任务交付，有价值的结论合并进对应权威文档，计划文件随之清除。
2. **1.0 前零兼容逻辑**：当前版本处于 `0.1`，在 1.0 正式发布前，**严禁编写任何兼容性逻辑，旧版本兼容被明确视为冗余的垃圾代码**。文档中绝不保留已废弃过渡特性的维护说明或兼容回退建议，所有架构描述紧随最新最优实现。
3. **单一事实源（Source of Truth）**：
   - 全局架构与渲染意图以 [`DESIGN.md`](DESIGN.md) 为准；
   - 运行时反射与二进制协议以 [`REFLECT.md`](REFLECT.md) 为准；
   - 代码地图、矩阵坐标与硬约束以 [`../AGENTS.md`](../AGENTS.md) 为准；
   - 协同流程与工程门禁以 [`ai-workflow.md`](ai-workflow.md) 为准。
   代码与文档在**同一个 PR** 中同步更新，拒绝文档漂移。

---

## 📐 二、内容尺度分级规范（Content Granularity Standards）

仓库文档按内容尺度与生命周期划分为 7 个层级，各层级职责边界明确，禁止越级堆叠或内容尺度失衡：

```text
L0: 仓库门户与硬约束层 (README / AGENTS / CONTRIBUTING)
      │
L1: 核心架构与协议基石层 (DESIGN / REFLECT)
      │
L2: 专项子系统设计契约层 (anim-* / editor-* / module / raytracing)
      │
L3: 协同流程与工程治理层 (ai-workflow / code-style / versioning / ...)
      │
L4: 平台接入与构建运维层 (android-* / termux / slang)
      │
L5: 诊断方法与排障知识层 (lessons-learned / rendering-investigation)
      │
L6: 调研灵感与展示资料层 (shadertoy / showcase)
```

| 层级 | 职责与内容尺度 | 准入内容 | 严禁与淘汰内容 |
|---|---|---|---|
| **L0 仓库门户** | 项目门面、核心特性、快速开始、技术硬规则与代码地图 | 项目定位、架构特色、快速运行指令、不可触碰的红线 | 局部函数实现细节、临时调试日志、过时架构残留 |
| **L1 核心架构** | 引擎全局系统拓扑、Render Graph 渲染管线、反射与二进制布局基石 | 架构原则、Pass 拓扑、内存模型、RSCN/RTTIBin 规范 | 单个 bug 修复流水账、过渡期临时兼容方案 |
| **L2 子系统契约** | 各功能子系统的模块级规格、数学模型、状态转移与 ABI/API 契约 | 动画图/IK/重定向数学、编辑器数据层、项目模块 C ABI | 与最新实现冲突的历史设计稿、未落地的泛化幻想 |
| **L3 流程治理** | AI / 人类协作流程、代码与注释风格、版本演进机制、门禁规则 | 五条硬规则、Commit/PR 模板、自检门禁、DoD | 随意更改的个人口头约定、未自动化的口号式规范 |
| **L4 平台运维** | 平台编译、交叉编译工具链、打包发布实操指南 | Android NDK、Termux 构建命令、Slang 编译工作流 | 失去复现意义的旧环境补丁、过期版本系统配置 |
| **L5 排障知识** | 深度调试排障决策树、Vulkan 同步与生命周期根因分析 | 二分诊断方法论、典型 VUID 根因剖析、不可重犯的教训 | 缺乏根因分析的表面报错报错备忘、零散调试片段 |
| **L6 展示灵感** | 视听展示脚本、渲染算法与着色器灵感记录 | 展示视频视觉与音频参数、Shadertoy 移植算法思路 | 包含废弃架构描述的过期文案 |

---

## 🗂️ 三、目录结构与文件命名规范

### 1. 文件命名统一标准

为确保跨操作系统（Windows / Linux / macOS）文件系统兼容性以及脚本/编译期宏的稳健性，所有文档命名遵循以下铁律：

1. **技术专题与流程文档一律全小写连字符命名**：`kebab-case.md`。
2. **前缀聚类语义结构**：格式统一为 `[domain]-[topic][-detail].md`。相同子系统的文档必须共享领域前缀：
   - 动画子系统：`anim-*.md`
   - 编辑器子系统：`editor-*.md`
   - 平台与环境：`android-*.md`
3. **核心基石文档保留全大写**：
   - `README.md`：目录入口索引与全景地图。
   - `DESIGN.md`：引擎架构设计意图最高权威源头。
   - `REFLECT.md`：反射体系与二进制格式最高权威源头。
4. **命名反模式**：严禁使用驼峰式（`AnimGraph.md`）、下划线（`ai_workflow.md`）或无领域前缀的零碎命名。

### 2. 目录组织拓扑

文档在物理上采用**前缀聚类扁平化目录**，在逻辑上按 6 大领域组织。扁平化前缀聚类兼顾了文件浏览器自然排序与跨模块相对引用安全（如 Rust `include_str!` 编译期宏直接绑定 `docs/editor-mcp.md`）：

```text
docs/
  ├── README.md                           # 本文档（文档体系地图与规范）
  ├── DESIGN.md                           # [L1 核心架构] 引擎与渲染总架构设计源头
  ├── REFLECT.md                          # [L1 核心架构] 反射系统与自描述二进制规范
  │
  ├── anim-graph.md                       # [L2 动画子系统] 动画图架构与 .rag 规范
  ├── anim-ik.md                          # [L2 动画子系统] 解析式两骨 IK 与地面贴合
  ├── anim-motion-matching.md             # [L2 动画子系统] 动作匹配纯数学层与特征数据库
  ├── anim-retarget.md                    # [L2 动画子系统] 骨骼姿态重定向纯数学层
  ├── editor-egui.md                      # [L2 编辑器子系统] 原生 egui 编辑器架构与零拷贝采样
  ├── editor-mcp.md                       # [L2 编辑器子系统] MCP 自动化契约与工具注册表
  ├── project-module-architecture.md      # [L2 模块宿主] 游戏工程 C ABI 与动态预览模块架构
  ├── mobile-raytracing-gi-design.md      # [L2 渲染进阶] 移动端 RayQuery 与实时 GI 管线蓝图
  │
  ├── ai-workflow.md                      # [L3 协作流程] AI 协同工作流与五条硬规则
  ├── code-style.md                       # [L3 编码规范] 英文代码注释规范与冗余清理指南
  ├── versioning.md                       # [L3 版本治理] SemVer 规则、1.0 前零兼容逻辑与发版
  ├── integrations.md                     # [L3 工程治理] CI/CD、Discord 机器人与 Agent 认领契约
  ├── agent-issues.md                     # [L3 规范门禁] Agent 提 Issue 准则与查重机制
  │
  ├── android-apk.md                      # [L4 平台运维] Android 纯游戏独立 APK 构建与打包
  ├── android-arm64.md                    # [L4 平台运维] aarch64 宿主机构建 Android 工具链替换
  ├── build-guide-termux.md               # [L4 平台运维] Termux / Android 原生终端编译指南
  ├── slang-bindless-migration.md         # [L4 平台运维] Slang 着色器工具链与 Bindless 资源维护
  │
  ├── lessons-learned.md                  # [L5 经验沉淀] Vulkan 渲染循环与工程踩坑根因库
  ├── rendering-pipeline-investigation.md # [L5 诊断方法] 渲染异常排查决策树与二分诊断方法论
  │
  ├── shadertoy-inspirations.md           # [L6 调研灵感] 常见图形效果与 Shadertoy 算法笔记
  └── showcase-video.md                   # [L6 展示资料] 引擎自动化展示视频技术脚本与参数
```

---

## 🗺️ 四、全景文档速查索引

| 类别 / 领域 | 推荐入口 | 核心关注点 | 关联代码 / 模块 |
|---|---|---|---|
| **架构总纲** | [`DESIGN.md`](DESIGN.md) | 全局分层、Render Graph 拓扑、TBDR 优化、零兼容包袱 | `prism-render`, `prism-engine` |
| **反射与数据** | [`REFLECT.md`](REFLECT.md) | `#[derive(Reflect)]`、RTTIBin、RSCN 场景序列化 | `prism-reflect`, `prism-scene` |
| **动画系统** | [`anim-graph.md`](anim-graph.md) | 状态机、剪辑混合、层级混合节点 | `prism-engine::anim` |
| | [`anim-ik.md`](anim-ik.md) | 分析式两骨 IK、Foot Placement 接线 | `crates/prism-anim-ik` |
| | [`anim-retarget.md`](anim-retarget.md) | 跨骨架旋转映射、骨长比例缩放 | `crates/prism-anim-retarget` |
| | [`anim-motion-matching.md`](anim-motion-matching.md) | 轨迹采样、特征最近邻搜索、脚部锁定 | `crates/prism-motion-matching` |
| **编辑器生态** | [`editor-egui.md`](editor-egui.md) | 零拷贝离屏采样、Split-tree 布局、控制通道 | `editor/`, `prism-editor-core` |
| | [`editor-mcp.md`](editor-mcp.md) | MCP 工具集、场景审查、离屏截屏、自动化驱动 | `crates/prism-editor-mcp` |
| | [`project-module-architecture.md`](project-module-architecture.md) | 预览模块 cdylib、C ABI 契约、HostApi 表 | `prism-project-api`, `prism-runtime-host` |
| **渲染专题** | [`mobile-raytracing-gi-design.md`](mobile-raytracing-gi-design.md) | RayQuery inline、SHARC 辐射度缓存 | `prism-render` |
| | [`slang-bindless-migration.md`](slang-bindless-migration.md) | Slang 编译、绑定代码生成、Bindless 纹理表 | `assets/shaders/`, `xtask` |
| **协作规范** | [`ai-workflow.md`](ai-workflow.md) | 五条硬规则、原子提交、自检门禁、PR 模板 | 全仓库 |
| | [`code-style.md`](code-style.md) | 纯英文注释、删除代码复述、删除旧版本兼容垫片 | 全仓库 `.rs` 源码 |
| | [`versioning.md`](versioning.md) | 0.1 演进策略、单一版本源、格式版本升级 | `Cargo.toml`, `scripts/version.py` |
| | [`integrations.md`](integrations.md) | CI/CD 标签门禁、Issue 自动化认领 | `.github/workflows/` |
| | [`agent-issues.md`](agent-issues.md) | 查重准则、复现模板、禁止提虚假兼容回归 | GitHub Issues |
| **平台实战** | [`android-apk.md`](android-apk.md) | Gradle 宿主、GameActivity、产物瘦身 | `projects/template/android/` |
| | [`android-arm64.md`](android-arm64.md) | aarch64 宿主独立工具链与 NDK 替换 | `scripts/build-android-game.sh` |
| | [`build-guide-termux.md`](build-guide-termux.md) | 移动端终端原生编译、静态检查验证例外 | Termux 终端环境 |
| **排障排查** | [`lessons-learned.md`](lessons-learned.md) | Vulkan 同步、Framebuffer 顺序、信号量复用 | `docs/lessons-learned.md` |
| | [`rendering-pipeline-investigation.md`](rendering-pipeline-investigation.md) | 画面异常二分法、黑屏排查、环境解耦 | `crates/prism-render/tests/` |
| **展示灵感** | [`shadertoy-inspirations.md`](shadertoy-inspirations.md) | 噪声、体积云、着色器灵感公式 | `assets/shaders/` |
| | [`showcase-video.md`](showcase-video.md) | 自动化合成视频脚本、动效与音频规范 | `scripts/showcase_video_simple.py` |

---

## 🛠️ 五、新增与修改文档检查清单（Checklist）

在编写或更新任何文档时，必须执行以下自查：

- [ ] **尺度对齐**：内容是否符合所在层级的尺度定位？没有把临时 session 计划、局部流水账写入长效文档。
- [ ] **命名合规**：新文档名称是否遵循 `kebab-case.md` 规范并包含正确的领域前缀？
- [ ] **零兼容垃圾**：是否彻底清除了向前/向后兼容的冗余描述？涉及格式或 API 变更时直接阐述最新设计。
- [ ] **地图登记**：新增文档是否已登记在本文件的「目录组织拓扑」与「全景速查索引」中？
- [ ] **交叉验证**：文中引用的文件路径与行号是否在当前分支上真实存在且有效？
