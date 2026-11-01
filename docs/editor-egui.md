# egui 编辑器（`editor/`）

PrismaRev 的编辑器（egui + ash），当前也是唯一编辑器。

设计目标只有两条：

1. **经典桌面编辑器布局**：菜单 + 工具栏 + 状态栏、split-tree
   docking、层级 / 检查器 / 资产 / 文档 / 日志 / 历史、预制体、撤销、项目读写。
2. **Scene / Game 视图零拷贝**：引擎渲染的图像直接被 UI 采样，不经过 CPU。

## 1. 为什么 egui 能做到零拷贝

egui 不是渲染器：它只产出 UI 的 mesh + 纹理 id，Vulkan 设备、命令缓冲区、
swapchain 都在宿主手里。`egui-ash` 的 `AppCreator::create` 要求宿主自己创建
instance / device 并返回 `AshRenderState`，因此可以做到引擎的模型：

```text
同一个 VkDevice / VkQueue
├─ prism-render GraphRenderer（Shadow → Scene → GTAO → Post → 离屏 image）
├─ egui-ash（UI 顶点 / 字体纹理 / 采样视口 image）
└─ swapchain present
```

`ImageRegistry::register_user_texture(image_view, sampler)` 把引擎的离屏图像注册
成 `egui::TextureId`，`painter.image()` 在**同一命令缓冲区**里采样它。GPU→GPU，
没有读回。

## 2. 设备共享的接线

`prism-render` 新增两个能力（都在 `crates/prism-render/src/context.rs`）：

- `create_host_instance(entry, extensions)`：宿主（UI 框架）自己持有 instance；
- `VulkanContext::from_host(entry, instance, physical, family, extra_device_extensions)`：
  用**与窗口模式完全相同的特性集**（bindless、synchronization2、时间线信号量、
  设备地址、条件 RT）在宿主 instance 上创建设备，但**不拥有**它
  （`ContextOwnership::External`，析构时不销毁，避免 UI 侧句柄悬空）。

`pick_host_physical_device` / `pick_present_queue_family` 保证选中的物理设备与
队列族同时支持图形和呈现（egui-ash 每个窗口自己建 surface + swapchain）。

离屏图像侧（`crates/prism-render/src/offscreen.rs` + `graph_renderer.rs`）：

- `OffscreenTarget` 从 `pub(crate)` 变成 `pub`，暴露 `image_view()` / `extent()` /
  `format()`，图像 usage 增加 `SAMPLED`；
- `GraphRenderer::offscreen_new_with_context(Arc<VulkanContext>, w, h, env)`：
  在共享设备上建离屏渲染器；
- `GraphRenderer::offscreen_present_for_sampling()`：帧结束（PostPass 的 final
  layout 是 `PRESENT_SRC_KHR`）后插入 `PRESENT_SRC_KHR → SHADER_READ_ONLY_OPTIMAL`
  的 barrier 并等待完成，随后图像可以交给 egui 采样；
- `offscreen_image_view()`：注册纹理用的 `VkImageView`。

下一次帧的 PostPass 用 `initial_layout = UNDEFINED`（load op clear），所以从
`SHADER_READ_ONLY_OPTIMAL` 回到颜色附件是合法的，不需要额外的状态机。

## 3. 帧节奏

编辑器**不建渲染线程**（与 `prism-app` 的 hosted 模式不同）：`Engine` +
`GraphRenderer` 直接跑在 egui-ash 的事件循环线程上，`App::ui()` 里同步出帧。

- 资产上传也同步：`GpuAssetResolver::prepare_requests` → `GraphRenderer::
  apply_asset_requests` → `apply_responses` 写回 ECS（原本是两个线程各一半）；
- 视口只在「文档变化 / 相机变化 / 选中变化 / resize / Play / 首帧」时重渲染，
  静止时零 GPU 工作。判断 resize 必须用 `GraphRenderer::offscreen_extent()`
  （`extent()` 是 swapchain 的，离屏时恒为 `0x0`，拿它比较会让视口**每帧**
  重建离屏目标并重渲染，约 15 ms/帧白烧 GPU）；
- 两个引擎（Scene / Game）都在启动时建好：首个 Game view 要跑一次 IBL 预过滤
  （约 1.5 s）+ 管线创建，留到第一次切 tab 才建就表现为「切 tab 卡半天」；
- Play 时 Game view 每帧 `tick()` + 重渲染；Scene view 仍是编辑态。

## 3.1 字体与图标

- 正文字体优先用系统 UI 无衬线（Segoe UI / Liberation Sans / Noto Sans），
  找不到再用 egui 内置字体；CJK 字体作为回退追加在最末尾。
- 图标用 **Phosphor Icons**（`egui-phosphor 0.11`，MIT OR Apache-2.0，对应 egui
  0.33）：`egui_phosphor::regular::{PLAY, PAUSE, EYE, FOLDER, FLOPPY_DISK, ...}`
  就是普通文本字形，因此传输控件 / 层级眼睛 / 资产文件夹 / 关闭按钮都不再依赖
  Unicode 符号（`▶ ⏸ ✕ ⧉` 这类在多数 UI 字体里缺字形，会显示成方块）。

## 3.2 用户项目代码（Game View Play）

Game View 的 Play 执行**用户项目自己的 gameplay 代码**，不是只跑引擎内置 system：

- 编辑器把项目 manifest 的 `preview-module` feature 编译成 cdylib
  （`cargo build --no-default-features --features preview-module --target-dir
  <项目>/target/preview-module`），由 `prism-runtime-host` 用同构建 ABI 加载。
  项目模块**不链接引擎**：`prism-game-api` 只开 `plugin` 后端。
- 源码改动落定 **350 ms** 后自动重编；Play 期间不自动换模块（避免打断试玩），
  `Ctrl+R` / File → Compile Project Module 强制重编。编译失败保留上一次成功的
  模块继续跑，诊断进 Console 与 Problems 面板。
- **重载是整模块替换，不是热重载**：旧 session 释放、模块 `create` 重跑，项目侧
  静态状态归零。没有脚本层可以迁移状态，所以文案统一用 “reload”，不要用
  “hot reload / domain reload”。
- 工具栏右侧显示模块状态（`ready` / `changes not compiled` / `compiling…` /
  `compile failed`）和 `Compile (Ctrl+R)` 按钮。源码时间戳晚于上一次编译尝试时是
  `pending`，Auto Refresh 关掉也看得出来。
- **Problems 面板**（Window → General → Problems）列出
  `cargo --message-format=json` 的 error / warning，带
  `file:line:col`，点一行用默认编辑器打开（先试 `code -g`，没有 `code` 就交给
  系统默认处理程序）。编译成功时 warning 也保留；失败时第一条 error 同时进 Console。
  面板有两段：**Project module**（Game View 的 `preview-module`）和
  **Player build**（`File → Build Player` / `Build And Run` 的 `cargo build`，
  它加 `--message-format`）。
- Play 前如果模块已加载但源码有未编译改动（Auto Refresh 关、或上一次 build 被 Play
  挡住），会先补编一次，避免试玩跑旧代码。
- Play 每帧先 `ProjectSession::tick(WorldCommands)`，再跑引擎 system：项目写入的
  组件和 `draw_quad` 提交的 UI 四边形在同一帧被 hierarchy / `ui::render` 看到。
- 项目模块通过 `AssetIo` 读**项目自己的** `assets/`（不是仓库根的引擎资产）。
- 输入：Play 时 Game view 把键鼠转发给引擎 `InputManager`，`InputSnapshot.keys`
  是**按住**语义（fishing 的 `input.key(SPACE)` 需要持续收线）。
- 没有 `preview-module` feature 的项目（任何还没改造的用户工程）只跑引擎，并在
  Console 说明原因；`PRISMREV_PROJECT_MODULE=/path/libgame.so` 可直接加载
  预构建模块（测试 / 非 Cargo 项目）。
- `projects/prisma` 与 `projects/fishing` 的 `preview-module` 必须显式打开
  `prism-scene` 的三组组件（3D / 2D / UI）：模块是独立 workspace，拿不到引擎侧的
  feature 统一，少一组就会在 `ProjectModule::load` 的 schema fingerprint 校验处被
  拒绝。（实测踩过：`default-features = false` 的 `prism-scene` 只注册核心组件，
  指纹 0x758a… vs 宿主 0x03b2…，加载直接失败。）
- **符号文件跟着 dll 走**：构建产物复制成 `staged/<stem>-<nanos>/<原文件名>`，
  `.pdb` 放在同一目录。唯一性只能放在**目录**上 —— dbghelp 是按 PE 里记录的
  `fishing.pdb` 去模块目录找符号，把 dll 改名成 `fishing-<nanos>.dll` 会让调用栈里
  工程帧全部退化成导出名（`prism_project_get_api`）。
- **模块日志与 panic 要桥接**：模块是独立 dylib，自带一份 `log` 和 `std`，宿主的
  logger / panic hook 都看不见它。工程在 `create` 里调用
  `prism_game_api::install_host_logger(&host)` + `install_host_panic_hook()`，
  之后 `log::info!` 与 `panicked at …` + `Backtrace` 都经 `HostApi::log` 进 Console
  （target `project`，来源 `Game`）。panic 本身由 `prism_project_api::guard()` 在
  ABI 边界兜住（不能跨 `extern "C"` unwind）。
- **tick 失败会 latch**：模块返回错误（含 panic）后停止调用，直到重新编译 / 重新
  加载。否则一个每帧 panic 的工程会 60 次/秒刷 stderr 并拖慢 GPU。
- **Game view 的渲染默认值来自工程**：`EngineHost::new` 之后立刻用工程
  `assets/settings.ini [render]` 覆盖（`apply_project_render_defaults`，与玩家启动
  同一条 `AppConfig.render → RenderSettings`），渲染设置面板只作用于 Scene view ——
  否则「编辑器里看到的」和「打包出来的」不是一张图。
- **2D 像素画用 `tonemap_mode = 2`**：Reinhard / ACES 会改写美术调色板；passthrough
  只做 `saturate`，配合 sRGB 交换链能 1:1 还原源图。`projects/fishing` 已默认开启。
  UI 颜色按 sRGB 作者值写入，在顶点展开时解码到线性，最终只由交换链编码一次（#592）。

## 4. 交互

- 轨道：左键/右键拖拽 → `OrbitCamera::orbit`；滚轮 → `zoom`；
- 点选：屏幕坐标 → 射线 → 与实体真实局部包围盒求交（网格用 cooked AABB，精灵用
  `frame_size / pixels_per_unit` + pivot；无数据才退回单位立方体），索引即
  `.scene.json` 的 `entities[i]`（#600）；
- 平移 gizmo：`pick_gizmo_axis` 命中轴后，按「屏幕位移 · 轴屏幕方向 / 每世界
  单位像素」换算世界位移，交给 `Workspace::translate_entity_world`；
- 选中高亮复用引擎的 `Gizmo` 组件（世界三色轴箭头），不引入新的渲染概念。
- 工具：`W` / `E` / `R` 切换移动 / 旋转 / 缩放 gizmo，`X` 切世界/局部，`G` 开关网格；
  `S` 开关 Scene view 背景（场景天空盒 / 纯色，与引擎的
  `SceneViewState.showSkybox` 同语义，默认开）；
  旋转走 `pick_rotation_axis` + `screen_angle`，缩放走 `set_scale_axis`。
- 空白拖拽 = 框选（`box_select`），`Alt`+拖 / 右键拖 = 轨道，双击 = 聚焦选中项。

## 4.1 日志

`logging.rs` 把 `env_logger` 包一层 tee：同一条 `log` 记录既进终端（`RUST_LOG`
语义不变），也进 `prism-editor-core` 的 `LogBuffer`，所以 Console 面板能看到
视口里 `prism_render` / `prism_engine` 的警告（与引擎日志
是同一思路）。`Trace` 不转发，避免淹没窗口。

## 4.2 菜单（结构）

菜单按引擎的分组组织，二 / 三级菜单里都是真实命令：

| 菜单 | 二级 / 三级 |
|---|---|
| File | New Scene / New Scene From Template ▸ / New Prefab / Open / Recent Scenes ▸ / Save / Save As / Save Project / Reload / Build Settings / Build Scene Pack / Cook Active Scene / Build Player / Build And Run / Compile Project Module / Auto Refresh Project Module / Clean Build / Exit |
| Edit | Undo / Redo / Cut / Copy / Paste / Duplicate / Delete / Select All / Deselect All / Invert Selection / Select Children / Select Prefab Root / Next·Previous Selection / Copy·Paste Transform ▸ / Paste Component / Play / Pause / Step / Project Settings / Preferences / Theme |
| Assets | Create ▸ (Scene / Prefab / **Prefab Variant…** / Folder…) / Open / Show in Explorer / Copy Path / Duplicate / Delete / Instantiate In Scene / Find References In Scene / Find References In Project / Refresh |
| GameObject | Create Empty / Create Empty Child / Create Empty Parent / Create Other ▸ (Rendering ▸ Mesh Renderer·Skybox / Light ▸ Directional·Point·Spot / Camera / UI) / Move To View / Align View to Selected / Snap to Ground |
| Component | schema 分类 ▸ 组件列表 |
| Window | General ▸ 面板开关 / Layouts ▸ 预设 + 命名布局（Load/Delete）+ Save Layout… + Save·Load Layout to File… + Reset All Layouts / Reset Layout |
| Help | Shortcuts / Editor Guide / About |

**玩家构建（`Build Player` / `Ctrl+B` = `Build And Run`）**按 `assets/settings.ini` 的
`[build]` 段走：`profile` 选 `debug` / `release`（`cargo build [--release]`），然后
`prism-build-pipeline build-scenes` 刷新 `scenes.pak`，再把可执行文件摆到
`<output_dir>/<exe>`、产物摆到 `<output_dir>/assets/{scenes.pak,settings.ini}`
（引擎只认 `<root>/assets/`，平铺在 `output_dir/` 根下双击会报 “no assets directory”）。
`Build And Run` 运行的是这份**摆好的产物**，不是 `cargo run` 的调试版；
非 0 退出、产物缺失都让状态栏显示 `build failed:`，完整 stderr 留在 Console。

`Build*` / `Cook Active Scene` / `Clean Build` 通过子进程调用 `prism-build-pipeline`
或 `cargo`（编辑器仍然不链接 cooker），输出进 Console。

快捷键（常用习惯）：`Ctrl+1..5` = Scene / Game / Inspector / Hierarchy / Project，
`Ctrl+Shift+C` = Console，`Ctrl+B` = Build And Run，`Ctrl+R` = 重编并加载用户项目模块，
`Ctrl+I` = 反选，
`W/E/R` = 移动 / 旋转 / 缩放，`X` 世界/局部，`G` 网格，`S` 天空盒，`F` 框住全部，`Del` 删除。

命名布局存在 `target/editor-egui/layouts.json`（`{name: {version,dock,floating}}`），
可以整份导出/导入成 JSON 文件（与 Web 的 `prisma-editor.dock.v4` 同形）。

`Create → Prefab Variant…`：把选中实例用 `extract_subtree_preserving_links` 存成
「继承 base + 覆盖」的预制体文件，再把场景里的这个实例改指向新变体（行为）。

## 4.3 面板能力

- **层级**：搜索、引擎式拖拽（上/下 25% 插入、中间挂成子级）、双击定位（框住选中项）、
  就地重命名（`F2`，macOS 上 `Return`）、
  显隐、复制 / 删除 / 改父级、Create 下拉；
- **检查器**：单选 + 多选（公共组件批量改字段，一次撤销）；预制体区块（Revert /
  Sync All / Save as Prefab…）；组件卡头的复制 / 移除；Add Component 分类菜单；
- **Project（资产）**：后退 / 前进 / 面包屑、目录浏览、类型过滤、搜索、重命名 /
  复制 / 删除 / 复制路径 / 实例化预制体；**目录行**和文件一样可以选中（单击选中、
  双击进入），右键有 Open / Rename / New Subfolder / Copy Path / Show in Explorer /
  Delete（只能删空目录），`F2` 就地改名；**资产可拖拽**：拖到目录 = 移动并改写引用，
  拖到层级实体 = 实例化预制体，拖到检查器的路径字段 = 直接写入路径（`AssetDrag` 载荷）。
- **Problems**：项目模块与玩家构建的 error / warning 列表（`file:line:col`），
  点一行打开源码；工具栏的模块状态与第一段同源，`Ctrl+R` / `Compile` 重新编译。
- **Docking**：拖动标签页（行为）：拖到另一个 leaf 的左/右/上/下 25% = 分屏，
  中间 = 合并成标签，拖出面板区 = 浮动窗口，拖到本 leaf 边缘 = 原地分屏。
- **预制体**：实例区块有 Revert / Sync All / **Apply to Prefab**（当前子树覆盖预制体
  文件，再同步同源实例）/ Save as Prefab…（无链接时）。
- **视口拖放**：预制体拖进 Scene / Game 视口 → 实例化并放到光标下的地面点
  （射线 ∩ `y = 0`）；`.scene.json` 拖进视口 = 打开该场景文档。
- **磁盘改动保护**：窗口重新获得焦点时比对每个已打开文档的 mtime（`git checkout`
  之后能立刻发现），改动过的文档在 Documents 面板标 ⚠；此时保存会被挡下并弹
  「Overwrite / Reload from Disk / Cancel」，不会静默覆盖别人的版本。
  资产的增删也在同一次焦点变化时重新扫描。
- **渲染图（Render Graph）**：Scene view 出首帧（以及每 60 帧）把当前图导出成
  `<工程>/target/prism-render-graph.json`，面板直接读它——不需要任何环境变量；
  游戏进程要同样的报告时设 `PRISMREV_RENDER_GRAPH_REPORT=<path>`。
- **分辨率倍率**：渲染图面板里的 `Viewport res`（0.5×–1.5×）只缩放离屏目标，
  不改变 UI 的 points 布局，给高 DPI 屏 / 弱 GPU 一个折中。
- **Game 视图分辨率 / 宽高比**：视口左上角下拉。默认读取工程
  `assets/settings.ini [window]` 的 `width`/`height`（例如 1600×900），
  按该分辨率离屏渲染并等比 letterbox，所以编辑器构图和打包产物一致。
  还可以选固定分辨率（1920×1080 等）或「只锁比例」的 `16:9 (fit)` /
  `4:3 (fit)` 等；`Match Window` 保留旧的跟面板缩放行为。控制通道用
  `resolution:1600x900` / `resolution:fit:16x9` / `resolution:match`。
- **2D 网格**：Scene 视图切到 2D / 正交相机时，参考网格画在 `z = 0` 的 XY 平面
  （X 红、Y 绿），而不是 3D 的 XZ 地面；位置和线宽沿用原来的 1m / 10m 分级（#602）。
- **多选拖拽**：拖多选中的任一行且落在行的中间区 = `reparent_selection` 整组改父级。
- **资产选择器**：检查器的路径字段右侧有 `⋯` 按钮，按字段名过滤扩展名
  （`mesh_path` → 模型、`sprite_path` → 贴图、`hdr_path`/`env_map` → HDR…），
  选中的相对路径直接写入字段。

## 4.4 内置截图与控制通道

截图在编辑器进程里做（`editor/crates/prism-editor-egui/src/screenshot.rs`）：

| target | 取法 | 说明 |
|---|---|---|
| `window` | Windows `PrintWindow` + `PW_RENDERFULLCONTENT` 画进 32bpp DIB | 连 UI 一起抓；DWM 合成后取样，窗口部分在屏幕外也能抓全。交换链由 egui-ash 持有，宿主拿不到 `VkImage`，这是唯一路径 |
| `scene` / `game` | `GraphRenderer::copy_offscreen_to_readback` + `readback_pixels` | 引擎离屏图（`PRESENT_SRC_KHR` → 读回 → PNG），不需要窗口，无显示器 / CI 可用 |

触发方式（都不是环境变量）：MCP 的 `editor_capture`（见 `docs/editor-mcp.md`），
或菜单「Window → Capture Window」（默认写到 `target/editor-egui/captures/`）。

控制通道（`src/control.rs`）：启动时在 `127.0.0.1:0` 上监听并把端口写进
`<项目根>/target/editor-egui/control.port`，一行 JSON 一问一答：
`ping` / `status` / `play` / `stop` / `pause` / `logs` / `input` / `capture` /
`ui.snapshot` / `ui.click` / `ui.set` / `quit`。`logs` 读 Console 的同一份缓冲（`since`
增量、`limit` 条数），自动化因此能拿到引擎日志、工程模块的 `project:` 日志和 panic 栈。

模态对话框也走节点树（#618）：`ui.snapshot` 的 `dialogs` 列出当前打开的对话框
（`delete-entities` / `delete-document` / `delete-assets` / `save-conflict` / `close` /
`confirm` / `prompt`）及按钮 id，`pending_dialog` 给出最上面那个；`ui.click` 接受
`dialog:<id>:<button>`（如 `dialog:delete-entities:delete` /
`dialog:close:discard`）。`ui.set` 对 `dialog:prompt` 写入输入值，再用
`dialog:prompt:ok` 提交。命令只是挂起确认框时，回执会带 `pending_dialog`，
不会出现「`ok: true` 但什么都没发生」。

`input` 给 Play 注入键鼠（`key=space` / `mouse=left` / `click=true`，`state=press|release`）。
事件**每帧只消费一个**：`just_pressed` 只在这一帧的 tick 里可见，下一帧就会被
`InputManager::begin_frame` 清掉，一次塞两个等于没按。fishing 的完整一竿就是这么跑的：
`click` → `cast` → `bobber landed` → 等到 `bite!` → 再 `click` → `hook set` →
`landed fish #1`，全流程只用控制通道，不需要窗口焦点。请求经 `mpsc` 排进 UI 线程，所以截图和
界面状态是同一帧的；启动前三帧内的 `capture` 会延后到画面合成后再抓（否则是白图）。

## 5. 数据层

`editor/crates/prism-editor-gpui-core` 是与 UI 框架无关的数据层（历史包名，不依赖
GPUI）：文档模型、schema、撤销栈、dock 树、日志缓冲、资产树、i18n、项目读写。
egui 壳通过路径依赖直接使用它；将来可以改名为 `prism-editor-core`。

## 6. 未完成 / 风险

- **未在真实设备上运行**：开发机是 Termux（aarch64、无显示），只做了
  `cargo check`（本机 + `x86_64-pc-windows-gnu`）。首帧呈现、resize、字体、
  docking 手感都需要桌面复核。
- **尚未实现**：材质 / 贴图拖到场景里自动挂 `MeshRenderer`；层级多选整组插入排序
  （整组只支持改父级）；多窗口（egui viewport）的独立 DPI。
- **内置图元（已打通）**：`crates/prism-render/src/primitives.rs` 生成 box / sphere /
  cylinder / capsule / quad 的顶点与索引；`mesh_path = "__builtin/<name>"` 由
  `prism-engine` 的资产解析器短路，不读 `.pak`、不需要 cook。对应的实体预设
  （`cube / sphere / cylinder / capsule / quad / plane`）在 `prism-editor-api`
  的 `entity_presets()` 里，GameObject → Create Other → 3D Object 直接用。
  尺寸固定：box/sphere/cylinder 1 单位，capsule 总高 2（半径 0.5），
  quad/plane 是 `y = 0` 上的 1×1 XZ 面、法线 +Y。
- **性能**：每次文档变化都会整场重载两个引擎实例（防抖 120ms），大场景下偏重；
  后续可以改成「只重建变化的实体」或引入 delta 上传。
- **两个引擎实例**意味着两份 GPU 资源（管线、IBL、网格）。如果以后只保留一个
  视口可见，可以按可见性懒加载 / 回收 Game view。
