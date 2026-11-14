# PrismaRev — Agent Instructions

From-scratch Rust game engine; **Vulkan** rendering, **Android** + desktop, **data-oriented ECS**.
Documentation map: [`docs/README.md`](docs/README.md).
Read `docs/DESIGN.md` before changing rendering/architecture, and the
**Coordinate & matrix conventions** section below before touching any matrix/coordinate math —
deviating from those conventions is a bug. (`README.md` is the human-facing overview.)

## AI 工作流（先读）

完整规范见 [`docs/ai-workflow.md`](docs/ai-workflow.md)。核心铁律：

- **合进 `master` 必须一个任务 = 一个分支 = 一个 PR**；默认 diff < ~500 行。**`dev` 是开发集成分支，允许直接提交、直接 push**，但提交仍需原子并自检，`dev` 合进 `master` 走 PR。
- **合并前必须跑 `python scripts/pr_ready.py`**，Markdown 块贴进 PR；红灯严禁合并。
- **AI 产出必须显式标注**：PR / issue 首行声明工具与身份，每条评论以 `🤖 Agent:` 开头，commit 正文写清 what/why，严禁冒充人类。
- **对外文本严禁内部简写**：PR、issue、commit 均面向公共审查，只用通用表述；中英文与数字间留空格，代码用行内标识符（详见 [`docs/ai-workflow.md`](docs/ai-workflow.md) §4.5）。
- **Issue 规则与查重**：提 issue 前按 [`docs/agent-issues.md`](docs/agent-issues.md) 查重（**只比对 open issue**，不比对 closed；纯架构取舍严禁作为“重复造轮子”报 issue）。接手 issue 必须先评论认领再调查（`🤖 Agent: claiming` + `agent/working`）。
- **代码注释只用英文**：严禁冗余复述代码，规范见 [`docs/code-style.md`](docs/code-style.md)；注释 PR 跑 `python scripts/check_comment_only.py --base origin/master`。
- **1.0 前零兼容负担（当前版本 0.1）**：在 1.0 版本发布前，**严禁编写任何兼容性逻辑**（向前/向后兼容、兼容层、兼容门面、废弃垫片或旧版本回退分支）；**旧版本兼容被明确视为冗余的垃圾代码**。涉及 API、数据结构、存档/交换格式、配置或管线重构时，直接按最新最优设计替换/删除旧实现，彻底清理废弃代码，绝不保留过渡兼容包袱。
- **文档分级与命名规范**：遵循 [`docs/README.md`](docs/README.md) L0–L6 划分；技术与流程文档全小写连字符 `kebab-case.md` 并带领域前缀，严禁驼峰或下划线命名；新文档必须在 `docs/README.md` 登记。
- **不猜**：改公开 API、存档格式（RSCN/RTTIBin/.pak）、坐标约定、workspace 分层或加依赖，先改文档或先问人。
- **版本号只改根 `Cargo.toml`**，通过 `python scripts/version.py` 同步，详见 [`docs/versioning.md`](docs/versioning.md)。
- **推送前同步主干本地解冲突**：`git fetch origin && git rebase origin/master`，冲突在本分支解完重跑 `pr_ready.py` 后再 `--force-with-lease` 推送。
- **subagent 限制**：subagent 仅用于只读侦察（同时最多 3 个，分批互不重叠），严禁多 agent 写文件；所有写入与提交留在主会话。

## 调查纪律（先定位，再动手）

排障指南与典型复盘案例详见 [`docs/lessons-learned.md`](docs/lessons-learned.md)。

1. **先读调用点，不要先读历史**：
   - bug 表现（按键不响应、相机未跟随锁定等）在代码中几乎总对应一个 `if` 条件或一次状态写入；`grep` 命中符号后立刻读该函数体即可定位并修改。
   - **严禁**用 `git blame`、翻第三方 crate 源码或写外部探针去验证代码中已写死的事实。
   - 一个 bug 只读它经过的那条链路；`grep` 命中的文件读完就停，不要全库横向扫荡。
2. **一个布尔值只能有一个真值**：
   - 类似「谁接管输入/锁定」的控制开关，真值只能在平台层唯一存在，其它消费方每帧同步读取（如 `InputManager::sync_pointer_lock`），严禁各方私自快照与缓存。
3. **提交前自查**：
   - [ ] 改动是否只落在报出问题的那条链路上？未顺手翻历史或重构无关文件。
   - [ ] 结论是否直接来自读代码？（代码已写明则无需探针/日志）。
   - [ ] 状态类 bug 是否确认了系统内布尔真值唯一性？

## 代码编辑纪律（Agent Code-Editing Discipline）

- **严禁人工猜测/数括号修复闭合符号错位**：若编辑后出现未闭合符号或括号不匹配，切勿自行拼凑或逐行数括号，直接向用户寻求协助。
- **大文件修改优先使用 Python 脚本**：文件过大超出编辑工具行数时，编写临时 Python 脚本执行确定性替换，严禁在 shell 中尝试不可靠的多行字符串替换。

## Termux / aarch64 ARM 验证例外（用户已明确授权）

- 当运行在 **Termux / aarch64 ARM 设备**上时，用户明确授权可用静态检查（`cargo fmt`、`cargo clippy`、静态分析）替代完整 `scripts/pr_ready.py` 与 `cargo test`。
- 若因环境限制未完整验证，必须在回复/PR 中明确列出未验证项。离开该环境后恢复完整门禁。流程与编译指南详见 [`docs/build-guide-termux.md`](docs/build-guide-termux.md)。

## Layout

- `crates/prism-ecs` — ECS 核心（Entity / Component / World / Query）。
- `crates/prism-reflect` — 运行时反射（`Reflect` / `TypeDesc` / `TypeRegistry`）+ 自描述二进制 RTTIBin。UI-free，不依赖 engine / render，详见 [`docs/REFLECT.md`](docs/REFLECT.md)。
- `crates/prism-reflect-derive` — `#[derive(Reflect)]` 过程宏。
- `crates/prism-gfx` — 跨场景/渲染的纯数据渲染状态枚举（`CullMode` / `BlendMode` / 材质与相机枚举）。无 Vulkan 与 ECS 依赖。
- `crates/prism-scene` — 场景 schema 层：纯数据组件、`ComponentRegistry`、`.scene.json` 文档与 RSCN v4 编解码。
- `crates/prism-ini` — `assets/settings.ini` 方言的唯一解析与写回实现，详见 [`docs/DESIGN.md`](docs/DESIGN.md) §10.14。
- `crates/prism-render` — Vulkan 后端：swapchain、Render Graph pass、`GraphRenderer` 驱动、IBL、Bindless/PBR、路径追踪。UI-free，详见 [`docs/DESIGN.md`](docs/DESIGN.md)。
- `crates/prism-engine` — 运行时引擎：渲染系统/设置、资产解析器、音频对接、自研 ECS UI 系统。UI-framework-free。
- `crates/prism-physics2d` — 确定性 2D AABB 几何内核（无 ECS/渲染依赖）。
- `crates/prism-app` — 平台接入与应用循环（winit 事件循环、窗口、`FrameHook` 扩展点、`run_on_android`）。UI-framework-free。
- `crates/prism-platform` — 平台抽象（窗口抽象、Vulkan surface、输入路由、线程调度提示）。
- `crates/prism-project-api` — 游戏工程与宿主编辑器之间的纯 C ABI（虚表与 POD 契约），详见 [`docs/project-module-architecture.md`](docs/project-module-architecture.md)。
- `crates/prism-runtime-host` — 动态游戏模块宿主（加载 cdylib、校验 ABI、转发组件与世界操作）。
- `crates/prism-game-api` — 统一游戏运行时 API（提供同一套 `GameWorld`，含零拷贝 `host` 与走 C ABI 的 `plugin` 两个后端）。
- `crates/prism-editor-api` — 编辑器与 MCP 的数据层（组件注册表投影 schema 与 `.scene.json` 定位/读写/校验）。UI-free、Vulkan-free。
- `crates/prism-editor-mcp` — 面向 Agent 的 MCP 服务器（stdio 暴露工具，驱动场景读写、游戏运行与编辑器自动化），详见 [`docs/editor-mcp.md`](docs/editor-mcp.md)。
- `crates/prism-build-pipeline` — 离线构建管线（资产打包、heightmap CLI；`bake-gi` 处于 `gi-bake` feature 下）。
- `crates/prism-audio` — 音频子系统（Firewheel 驱动）。
- `crates/prism-asset` — 纯运行时资产层（`AssetId` / `Handle`、`.pak` 格式与热重载）。
- `crates/prism-anim` — 动画核心数学与数据层（骨骼、剪辑采样与 CPU 蒙皮参考实现）。
- `crates/prism-anim-ik` — 动画两骨 IK 与地面贴合纯数学层，详见 [`docs/anim-ik.md`](docs/anim-ik.md)。
- `crates/prism-anim-retarget` — 姿态重定向纯数学层，详见 [`docs/anim-retarget.md`](docs/anim-retarget.md)。
- `crates/prism-motion-matching` — 动作匹配纯数据/数学层（轨迹与姿态特征库、NN 搜索），详见 [`docs/anim-motion-matching.md`](docs/anim-motion-matching.md)。
- `crates/prism-cooker` — 离线资产烘焙层（importers、cookers、资产数据库、CookProfile 与 `prism-cook` CLI）。
- `launcher/` — Android 启动壳（Tauri 2 + Vue 3，独立 workspace）。桌面工程管理 Hub 已拆至独立仓库 [`Excurs1ons/prisma-hub`](https://github.com/Excurs1ons/prisma-hub)。
- `editor/` — 原生 egui 编辑器（唯一编辑器，独立 workspace）：split-tree 停靠、Scene/Game 视图零拷贝采样、内置本地控制通道（`target/editor-egui/control.port` 供 MCP 驱动），详见 [`docs/editor-egui.md`](docs/editor-egui.md)。
- `projects/template/` — 官方模板/样例工程（桌面 bin + Android cdylib `libtemplate_core.so`，独立 workspace）；其 `android/` 子目录为 game-only 独立 Gradle 宿主，详见 [`docs/android-apk.md`](docs/android-apk.md)。
- `projects/sponza/` — Sponza 样例工程（独立 workspace）。
- `crates/xtask` — 桌面/CI 工具（着色器编译校验、反射生成 Rust 绑定与着色器预算检查）。
- `assets/shaders/` — Slang 着色器源码（`slang/`）、预编译提交的 `.spv` 及反射元数据（`reflection/`），详见 [`docs/slang-bindless-migration.md`](docs/slang-bindless-migration.md)。

## Editor / user-project separation (do not regress)

- **依赖树单向隔离**：用户项目依赖链必须保持 **UI-free 与 editor-free**：
  `template → prism-app → prism-engine → {prism-scene, prism-ecs, prism-render, prism-asset, prism-physics2d}`。
  严禁将 UI 框架（egui/egui-ash 等）或构建期库引入引擎底层与用户工程，用 `cargo tree | grep -i egui` 验证必须为空。
- **可选能力默认关闭**：`prism-app/physics`（Rapier 刚体与运动学角色移动）、`prism-app/audio`（Firewheel 空间声像）、`prism-asset/async-io`。由 `scripts/check_deps.py` 守门禁预算。
- **视口离屏流水线零拷贝**：`prism-render` 渲染至离屏 `VkImage`，egui 通过共享 `VkDevice` 注册 `VkImageView` 直接采样，不经过 CPU 回读。
- **项目代码在 Game View 中作为动态模块运行**：
  - 模块作为 `cdylib` 动态载入，独立 target 目录（`projects/<p>/target/preview-module`）；玩家端保持静态一体化构建。
  - C ABI（`crates/prism-project-api`）只传递操作类（`LOGGING`, `WORLD_COMMANDS`, `COMPONENTS`, `DRAW`, `ASSET`），组件作为 RTTIBin 字节穿越，严禁传递 `Engine` / `World` / trait 对象。
  - 详细规格、PDB 隔离与模块重载流程见 [`docs/project-module-architecture.md`](docs/project-module-architecture.md)。
- **换场复用同一个 `Engine`**：`Engine::load_scene_json_file` 走 `Single` 模式，`Persistent` 实体跨场存活，GPU 资源交回渲染器回收（#482, #495）。
- **中立 UI 覆盖与报告 DTO**：`prism-app` 通过类型擦除的 `OverlayMessage` 跨线程下发 UI 数据，严禁依赖具体 UI 类型；渲染图报告 DTO（`render_graph_report.rs`）保持纯数据。

## Build / check / test

- **提交前自检**：`python scripts/pr_ready.py`（`--quick` 快速回路；`--all` 全 workspace）。
- **构建与运行**：`scripts/run.ps1`（Windows），跨平台入口 `bash scripts/dev.sh [project] [--release]`（Windows: `scripts/dev.ps1`）。
- **运行前置依赖**：运行时必须从 `assets/scenes.pak` 读场景，启动前通过 `cargo run -p prism-build-pipeline -- build-scenes` 打包场景。
- **编辑器运行**：`cd editor && cargo run -p prism-editor-egui`。数据层测试：`cargo test --manifest-path editor/Cargo.toml`。
- **独立 Workspace 构建**：`projects/template/`、`projects/sponza/`、`launcher/` 与 `editor/` 为独立 workspace，必须在各自目录下运行 `cargo check` / `cargo test`。
- **发布档构建**：`bash scripts/build-dist.sh <project> [target]`（Windows: `scripts/build-dist.ps1`），支持 `--size`（tiny 极限体积）与 `--pack`（UPX 壳，仅用于带签名发布包），详见 [`docs/DESIGN.md`](docs/DESIGN.md) §12.5。

## Editor MCP server (agent 改场景 / 跑游戏 / 抓图)

- `crates/prism-editor-mcp` 将 `prism-editor-api` 暴露为 stdio MCP 工具：文档读写、`schema_list`、`field_set`（反射校验）、组件增删、`game_run/game_logs`、`screenshot`。
- 不链接渲染器与 Vulkan；修改作者文档 `.scene.json` 后需重新构建 scene pack。
- 编辑器 UI 由 `editor_ui`（`snapshot` / `click` / `set`）经本地控制通道驱动。完整工具契约见 [`docs/editor-mcp.md`](docs/editor-mcp.md)。

## 运行时自动化与诊断辅助

- **内置截屏（`PRISMREV_SCREENSHOT`）**：
  - `PRISMREV_SCREENSHOT=<path>` 在第 N 帧（默认 90，由 `_FRAME` 控制）将输出写入 PNG；支持 `_SIZE=1920x1080` 或 `_SCALE=2`（最近邻缩放）。
  - 用于独立游戏进程与自动化流水线；编辑器视口截图走控制通道 `editor_capture`，不要给编辑器设环境变量。
- **帧时间基准报告（`PRISMREV_BENCH_REPORT`）**：
  - `PRISMREV_BENCH_REPORT=<path>` 在采样满 N 帧（默认 600，预热 120 帧）后输出 JSON 统计分布（分位数、low 统计与直方图）。
- **开发期资产热重载（`PRISMREV_HOT_RELOAD`）**：
  - `PRISMREV_HOT_RELOAD=1` 开启 `.pak` 监听与重载，旧 GPU 资源回收（#495）。编辑器松散资产监听详见 [`docs/editor-egui.md`](docs/editor-egui.md)。

## Shaders 与绑定系统

完整工具链与 Bindless 规范见 [`docs/slang-bindless-migration.md`](docs/slang-bindless-migration.md)。

- **源码与编译**：Slang 源码在 `assets/shaders/slang/*.slang`，入口点为 `vertexMain` / `fragmentMain`；通过 `bash assets/shaders/compile.sh` 编译。
- **Bytecode 已入库**：`.spv` 与反射 JSON（`reflection/*.json`）已提交入库，slangc 版本钉死在 `2026.21`。修改着色器必须重新编译并提交 `.spv` 与反射。
- **强类型 Push Constants**：必须且只能来自 `shader_bindings`（`xtask shader_bindgen` 生成），严禁手写结构体；std140 对齐洞由 `_pad_*` 显式占位，derive `bytemuck::Pod` / `Zeroable`。
- **资源预算**：`assets/shaders/budgets.json` 记录上限，由 `xtask shader-budget` 监控。

## Coordinate & matrix conventions (do not mix up)

- **右手系**：相机朝向 **−Z**；+X 向右，+Y 向上，+Z 朝向观察者。
- **矩阵排布**：列主序 `mat4` = `[[f32; 4]; 4]`，按 `[col][row]` 索引；`clip = projection * view * model`。
- **透视投影**：使用 Vulkan y-flip `p[1][1] = -inv_tan(fovy / 2)`；深度范围 **[0, 1]**。
- **NDC 空间**：y 轴 −1 为顶部，+1 为底部。帧缓冲原点在左上角，y 向下。
- **天空盒视图旋转**：`GraphFrame::inv_view_rot` 为 `view` 左上角 3×3 的**转置**（`m[c][r] = view[r][c]`），用于 view→world 旋转，非前向矩阵。

## Render-graph architecture rules

- **节点声明**：Pass 实现 `RenderPassNode`（`setup` 声明资源输入输出，`execute` 录制指令）。
- **资源生命周期**：Framebuffers / depth 必须在 swapchain 重建前释放（`forward_pass.drop_target`），防 VUID 报错。
- **双面阴影**：`ShadowCastingMode::TwoSided` 仅在阴影 pass 关闭背面剔除（双管线区分）。
- **Frame Packet 跨线程复用**：主线程取 `take_spare_packet`，`extract_frame_packet_into` 重填，渲染线程末尾 `recycle_packet`，严禁每帧 collect 新集合。
- **描述符集索引**：set 0 = frame UBO/materials/lights，set 1 = bindless textures，set 2 = IBL，set 3 = shadow map。

## Reflection / RTTIBin rules

完整规范与二进制协议见 [`docs/REFLECT.md`](docs/REFLECT.md)。

- **中立与命名**：`prism-reflect` 为纯数据层；`TypeId` 为 `fnv1a64(canonical_type_name)`，持久化严禁使用 `std::any::TypeId`。
- **格式分工**：
  - **RSCN v4**：唯一分发场景格式（`magic + version + (type_id, schema_hash) + 紧凑实体载荷`）。
  - **RTTIBin**：自描述工具/交换格式，供编辑器、MCP 与工程动态模块穿越使用。

## Scene / asset layering rules

架构意图与资产流转见 [`docs/DESIGN.md`](docs/DESIGN.md)。

- **三职责分层**：`prism-scene`（schema/数据/RSCN编解码），`prism-cooker`（离线工具），`prism-asset`（运行时读取）。
- **Transform 唯一权威源**：作者写 `.scene.json` 的 `transform`，烘焙为 `LocalTransform` + `LocalToWorld`。`LocalTransform` 仅注册为 reflect type，不作为可挂载组件。
- **矩阵传播时机**：在模拟步末尾由 `hierarchy_system` 执行，渲染提取阶段只读。
- **实体稳定 ID**：`.scene.json` v3 实体持稳定 `id`，父级引用该 `id` 而非数组索引；运行时注入 `SceneEntityId` 标记。
- **UI 布局派生输出**：`ComputedLayout` 每帧计算覆写，作者仅写 `Node` + `Style`。

## Asset root resolution (runtime, do not regress)

- **路径解析单点实现**：统一走 `prism_engine::asset_paths`，严禁新增 cwd 相对路径。
- **命中顺序**：`PRISMREV_ASSET_ROOT` → cwd 及其各级父目录 → 可执行文件目录及其各级父目录；必须包含实际存在的 `assets/` 目录。

## 项目配置文件（settings.ini / scenes.json，do not regress）

规范见 [`docs/DESIGN.md`](docs/DESIGN.md) §10.14。

- **格式唯一**：仅使用 `assets/settings.ini` 与 `assets/scenes.json`，1.0 前零兼容逻辑，不包含任何旧格式回退分支。
- **方言解析器**：唯一实现为 `crates/prism-ini`，各方仅 re-export。
- **清单接口**：统一走 `prism_editor_api::scene_manifest` 与引擎 `SceneManifest::parse`。

## Platform constraints & Window backends

- **平台约束**：Android 仅分发预编译 `.spv`；保持 Vulkan 验证层 0 报错。
- **窗口后端互斥**：`winit-backend`（默认）与 `win32-native`（默认关，无 winit 依赖）。`prism-platform` 为唯一中立抽象层。
- **Win32 后端决策（#1033）**：保留并冻结，用于保障极限发布体积，不扩展新功能。

## Android packaging chain (game + launcher in one APK)

完整流程见 [`docs/android-apk.md`](docs/android-apk.md) 与 [`docs/android-arm64.md`](docs/android-arm64.md)。

- **双 Activity 架构**：Tauri launcher 界面（`:main`）与独立游戏进程 `GameActivity`（`:game`，加载 `libtemplate_core.so`）。
- **打包入口**：`scripts/build-android.ps1 -Project <name>`（要求 NDK minSdk ≥ 26）。发布签名材料不入库。
- **生成宿主**：新工程通过 `scripts/new-android-game.sh <project>` 生成独立 Gradle 宿主。
