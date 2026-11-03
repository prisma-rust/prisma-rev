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

**松散资产热重载（#495）**：Preferences 的
`Rebuild scene pack when loose assets change`（默认开）每 1s 扫描 `assets/` 下
**未打包**的作者资产（贴图 / 模型 / HDR / 字体 / 材质 / `scenes.json`…），排除
`.pak` / `.pak.meta.json` / `.rscn` / `.spv` 与编辑器自己保存的 `.scene.json` /
`.prefab.json`。检测到变化后等 800ms 安静期（连续保存 / 大文件分块写入不会连打多次），
再触发一次现有的 `Build Scene Pack`；打完包由 `.pak` 轮询链路重开包并
`invalidate_assets`，**Play 中也能直接看到新资源**，不用退出再进。构建期间不排队新任务，
构建结束会重新对齐快照。
检测用的是 `prism_editor_core::project::FileStamp`（**mtime + 长度**），与文档外部改动
检测（`Workspace::check_external_changes`，焦点变化触发）共用同一套版本戳，不再各写一份；
粗粒度文件系统上同一秒内的覆盖写也能被长度变化捕获。

**Play 态的视觉提示（#500）**：播放中整窗四周有一圈绿色描边、暂停时变琥珀色；
状态栏的 `PLAYING` / `PAUSED` 带同色底纹。检查器顶部在这期间显示横幅
`Play mode — changes made now are discarded when Play stops.` —— 运行期的编辑仍写进
作者文档，但 `Workspace::stop()` 会用进入 Play 前的快照原样盖回（文档、撤销栈、脏标记、
选中），所以 Stop 之后这些改动不会留下。

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
- **点选循环穿透（#416）**：同一个位置连续点击时，依次选中被挡住的下一层
  （候选集按射线距离从近到远，4px 内算同一次点选；换位置 / 换候选集就回到最近的
  一个）。Shift / Ctrl 的加选 / 减选不循环，永远取最近的一个；
- **方向 gizmo（#416）**：Scene 视图右上角 6 个世界轴手柄（背向相机的更淡），
  点一下把相机对齐到该轴；`OrbitCamera::align_to_axis` 直接解出 yaw / pitch，
  正交视图同样可用。
- **多选 gizmo 与枢轴模式（#416）**：Move / Rotate / Scale 拖动作用于**整个选择集**
  （每次拖动仍是一个撤销步）。工具栏 `Pivot` 开关关闭时是 **Center** 模式：gizmo 落在
  选择集中心，旋转 / 缩放会带动整组位置（绕中心公转 / 按轴心比例推开）；打开后是
  **Pivot** 模式：gizmo 落在活动对象自己的轴心上，每个对象只改自身变换。
- **View… 菜单（#416）**：Scene 视图工具栏里可调编辑器相机 FOV（20–110°）与
  `near` / `far`，并分别开关**相机图标 / 光源图标**。FOV 与裁剪面同时写进引擎相机和
  拾取射线（`CameraFrame::fov_y_degrees`），gizmo、点选与画面不会错位；
  `2D` 开关仍是正交 XY 视图。
- **视口浮层工具条（#421）**：Scene 视图左上角（8px 处）有一条半透明工具条，
  不用把视线移到顶部工具栏：`Grid`（`G`）、`Sky`（`S`）、`2D`（正交 XY）、
  `Snap` 开关与平移吸附步长（`Δ`，世界单位，0.01–100）。左侧箭头折叠成一个小方块；
  折叠状态只存在 `UiState`，不进文档。它是这个 issue 建议的**最小实例**，
  不是 Unity `OverlayCanvas` 那套可拖拽停靠系统。
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

**Console 的四个工具开关（#421）**：`Collapse` 把相同的 `(级别, 来源, 文本)` 合并成
一行并显示 `×N`（保持首次出现的时间顺序）；`Clear on Play` 在 Play 开始时清空；
`Error Pause` 在 Play 中第一次出现 Error / Panic 时自动暂停（并写一行说明，避免
看起来像卡死）；日志行里带 `path:line[:col]` 时**双击**用默认编辑器跳到该位置
（与 Problems 面板同一套 `open_source_location`）。

**Play 自动最大化（#421）**：`Window → General → Maximize Game View on Play` 打开后，
按 Play 自动最大化 Game 视图，Stop 恢复到进入前的布局（进入前没有最大化任何面板时
也回到「都不最大化」，不会把用户自己最大化的面板吞掉）。

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

**可重绑快捷键（#421）**：`Help → Shortcuts` 面板里**点一下键位**再按新组合键即可改绑
（Esc / Cancel 取消，行尾 `Reset` 恢复单条，顶部 `Reset all` 清空全部）。捕获期间
`handle_shortcuts` 整体让路，按键不会顺带触发命令；与其它命令冲突时拒绝改动并在面板
里提示占用的命令。覆盖表存在 `Preferences::shortcut_overrides`（随编辑器状态
`target/editor-egui/state.json` 持久化，键是 `Command::id()`），菜单提示与实际分发
读的是同一张 `effective_bindings` 表，覆盖后旧的别名键位不再生效。

命名布局存在 `target/editor-egui/layouts.json`（`{name: {version,dock,floating}}`），
可以整份导出/导入成 JSON 文件（与 Web 的 `prisma-editor.dock.v4` 同形）。

`Create → Prefab Variant…`：把选中实例用 `extract_subtree_preserving_links` 存成
「继承 base + 覆盖」的预制体文件，再把场景里的这个实例改指向新变体（行为）。

## 4.3 面板能力

- **层级**：搜索、引擎式拖拽（上/下 25% 插入、中间挂成子级）、双击定位（框住选中项）、
  就地重命名（`F2`，macOS 上 `Return`）、
  显隐、复制 / 删除 / 改父级、Create 下拉；
- **检查器**：单选 + 多选（公共组件批量改字段，一次撤销；多选时最后点击 /
  右键的对象是 Active，批量 Transform 显示它的值，#507）；预制体区块（Revert /
  Sync All / Save as Prefab…）；组件卡头的复制 / 移除；Add Component 分类菜单；
  **列表字段可编辑（#418）**：`Vec<T>` 每项一行，带 `▲` / `▼` / `✕`，底部
  `+ Add` 按元素 schema 生成默认值（`Vec3` → `[0,0,0]`，嵌套 struct → 各字段默认值）；
  定长数组不提供增删；元素内部仍然用它们真正的控件，路径是 `字段[i]`（`LineRenderer.points`
  这类字段因此不必再手改 JSON）。`Option<T>` 用勾选框控制存在与否，嵌套 struct 可折叠展开。
  **组件预设（#418）**：组件卡头的 `…` → `Presets` 可以把当前组件值存成命名预设
  （同名覆盖），之后对任意实体套用（整份组件 = 一个撤销步）。预设随编辑器状态
  （`target/editor-egui/state.json`）持久化，按完整反射名分组；`Reset` 仍恢复 schema
  默认值。组件右键菜单还有 Copy / Paste Values / Move Up / Down / Remove，字段右键可
  Copy / Paste Value 与 Copy Property Path；检查器右上角的锁按**实体 id** 钉住内容。
  **组件卡头的 `…` 菜单**：Copy Component / Paste Values / Move Up·Down / Reset（写回
  schema 的 `defaultJson`，一次撤销步）/ Remove；**字段右键**：Copy Value / Paste Value
  （只有同形状的值可粘贴，不做类型强转）/ Copy Property Path。面板顶部的锁图标是
  **Inspector Lock**：按实体 id 钉住当前对象，去别处点选不换内容，对象被删后自动解锁。
  没有 schema 的组件（用户项目模块里的自有类型等）显示**可编辑的原始 JSON**：
  Apply JSON / Reload，解析失败时按钮禁用并就地显示错误 —— 不用去手改场景文件。
  Transform 行的数值框右键有 **Copy / Paste / Reset**（整行；Position 与 Scale 的剪贴板
  互不串用，Reset 回到 0 / 单位四元数 / 1）。
- **Project（资产）**：后退 / 前进 / 面包屑、目录浏览、类型过滤、搜索、重命名 /
  复制 / 删除 / 复制路径 / 实例化预制体；**目录行**和文件一样可以选中（单击选中、
  双击进入），右键有 Open / Rename / New Subfolder / Copy Path / Show in Explorer /
  Delete（只能删空目录），`F2` 就地改名；**资产可拖拽**：拖到目录 = 移动并改写引用，
  拖到层级实体 = 实例化预制体，拖到检查器的路径字段 = 直接写入路径（`AssetDrag` 载荷）。
- **Project 面板的视图 / 搜索语法 / Ping（#419）**：搜索框右侧有三个开关 —— 列表 /
  网格视图、单列 / 两列（左目录树 + 右内容）。搜索支持 Unity 风格的 `t:` 前缀：
  `t:texture`（英文 slug / `Texture` / 中文标签 `纹理` 都认）、`dir:sprites`，
  多个词之间是 AND，多个 `t:` 之间是 OR。选中资产后检查器显示资产详情（类型 / 体积 /
  GUID / 引用数 / 预览），底部的 **Ping In Scene** 和右键菜单的 `Ping In Scene` 会
  选中第一个引用该资产的实体并在 Scene 视图里框住它 —— 资产 ↔ 场景双向定位。
- **资产身份（#466 第一步）**：打开或刷新项目时，编辑器给 `assets/` 下每个还缺
  sidecar 的资产生成 `<path>.rmeta`（`{ "guid": "<32 hex>", "importer": "..." }`）；
  已有且有效的 GUID 永不重写，损坏的按缺失处理。扫描会跳过 `.rmeta`（它不是可导入
  资产），资产详情里的 GUID 优先显示 sidecar 的值，没有才回落到旧的路径哈希；
  重命名 / 移动文件时 sidecar 跟着走，删除时一起删。`.scene.json` / `.prefab.json`
  里现在会同时写路径和 `*_guid`：打开文档时 GUID 优先解析成路径（解析不到则回退
  路径），保存时按 sidecar 补写 / 刷新 GUID；cooker 与开发期 JSON loader 也走同
  一套规则，旧文档不需要迁移。重命名时，带 GUID 的引用**不再改写文档**（读取时
  按 sidecar 解析到新路径）；只有旧的无 GUID 路径引用仍走兼容改写。引用分析 /
  删除确认也按 GUID 匹配，stale path 不会漏掉引用。
- **纹理导入设置（#514）**：资产检查器选中纹理时多出 `Import Settings`
  （`Max Size` 0 = 不限、`Mips` 开关）。`Apply` 写旁挂文件
  `<asset>.import.json`（如 `assets/sprites/boat.png.import.json`），`Revert` 重新读盘，
  `Reimport` 直接重打场景包（`BuildKind::ScenePack`，不要求先切到 Build 面板）。
  cooker 在烘纹理时读同一份 JSON：`maxSize` 按**面积平均**缩小（保持宽高比，只缩不放），
  `generateMips` 控制是否生成 mip 链；缺省字段跟随 profile。
- **Problems**：项目模块与玩家构建的 error / warning 列表（`file:line:col`），
  点一行打开源码；工具栏的模块状态与第一段同源，`Ctrl+R` / `Compile` 重新编译。
- **Docking**：拖动标签页（行为）：拖到另一个 leaf 的左/右/上/下 25% = 分屏，
  中间 = 合并成标签，拖出面板区 = 浮动窗口，拖到本 leaf 边缘 = 原地分屏。
- **预制体**：实例区块有 **Open Prefab** / Revert / Sync All / **Apply to Prefab**
  （当前子树覆盖预制体文件，再同步同源实例）/ Save as Prefab…（无链接时）。
  **Open Prefab** 在工具栏留下面包屑 `← level.scene.json / crate.prefab.json`；
  点 `←` 回到打开它的场景并选中原实例。预制体文档**干净**时会重新实例化该实例
  （同步预制体里的改动，实体 id 会变，选中仍落在实例上），**脏**时只切回场景并选中、
  不把未保存的内容同步出去。
- **视口拖放**：预制体拖进 Scene / Game 视口 → 实例化并放到光标下的地面点
  （射线 ∩ `y = 0`）；`.scene.json` 拖进视口 = 打开该场景文档。
- **磁盘改动保护**：窗口重新获得焦点时比对每个已打开文档的 mtime（`git checkout`
  之后能立刻发现），改动过的文档在 Documents 面板标 ⚠；此时保存会被挡下并弹
  「Overwrite / Reload from Disk / Cancel」，不会静默覆盖别人的版本。
  资产的增删也在同一次焦点变化时重新扫描。
- **渲染图（Render Graph）**：Scene view 出首帧（以及每 60 帧）把当前图导出成
  `<工程>/target/prism-render-graph.json`，面板直接读它——不需要任何环境变量；
  游戏进程要同样的报告时设 `PRISMREV_RENDER_GRAPH_REPORT=<path>`。
  面板还会显示上一帧总 GPU 时间与按成本排序的 per-pass 表（#518；设备不报时间戳时
  整段隐藏，不用 CPU 墙钟冒充），以及**纹理 / 网格 / 材质的驻留量**与估算字节
  （#518 Phase 2，数据来自渲染器管理器，旧报告没有该字段时同样隐藏）。
  同一段里还有 **Draw calls** 列表（#518 Phase 2 的数据层）：逐条列出 forward 网格
  （网格序号 · 材质槽）、按混合状态合并的 sprite 批次、粒子 billboard 批次，带实例数、
  三角形数与 **State** 列（渲染队列 / cull / z-write / 混合，sprite 显示实际混合状态）；
  被剔除 / 只投影子的网格用弱化斜体标出。只读展示，不含 pipeline / descriptor 捕获与回放。
- **分辨率倍率**：渲染图面板里的 `Viewport res`（0.5×–1.5×）只缩放离屏目标，
  不改变 UI 的 points 布局，给高 DPI 屏 / 弱 GPU 一个折中。
- **工程自带字体（UI 文本）**：把 `.ttf` / `.otf` / `.ttc` 放进 `assets/fonts/`，
  在 `Text` 组件的 `font` 字段（检查器里有资产选择器）填相对路径即可；空 = 系统默认
  字体。字体是普通资产：`AssetType::Font`、原样进 `.pak`、重命名时引用会被改写，
  运行时由 `GpuAssetResolver::prepare_ui_fonts` 读出字节注册进 UI 图集
  （多字体共用一张 R8 图集，仍然只有一次 draw call，#583）。
- **UI 按钮（#528）**：给 `Node + Style` 元素再加 `prism_engine::ui::Button`
  （Add Component 里就是 `Button`）就得到可点按钮：`normal` / `hover` / `pressed` /
  `disabled` 四色由输入系统自动切换，`interactable = false` 只显示禁用色、不响应点击。
  点击语义与 Unity `Clickable` 一致：在元素内按下并**在同一元素内松开**才置
  `clicked`；点击后该按钮获得键盘焦点，`Enter` / `Space` 同样提交。`Interaction`
  现在是可序列化组件，场景 / 预制体可以直接保存。
- **UI 背景图（#591）**：`Node + Style` 加 `"image": "textures/icon.png"` 就在矩形里
  画贴图（整张铺满、最近邻采样），`background` 改为 tint；不写 tint 时按不透明白色
  显示图片原色。图片走场景共享的 bindless 纹理表，不需要新管线；`Button` 的交互色
  同样会 tint 图标，所以同一个图标能跟着按钮变亮/变暗。贴图路径和精灵一样由
  `GpuAssetResolver` 解析进 `.pak`（`UiImageRef`），资产重命名会改写引用。
- **UI 布局组与文本换行（#527）**：`Style.layout` 让容器按方向排列子元素
  （`direction = None` 仍是原来的锚点定位）：
  - `spacing` 相邻间距、`padding` 内容区内边距（左/右/上/下）；
  - `child_align` 控制子元素在**交叉轴**上的对齐（`Start` / `Center` / `End` /
    `Stretch`），主轴按排列顺序推进；
  - `fit_content` 让容器在没写 `width` / `height` 的轴上收缩到内容尺寸；
  - 布局组的子元素由游标定位，自己写的锚点/pivot 被忽略；非布局组的节点行为不变。
  检查器里 `Style` 上方有专门的 **Layout Group** 编辑块（方向下拉、间距、四边
  padding、交叉轴对齐、fit content），写入走 `set_field` 的点路径，所以撤销 /
  标脏 / 重载语义与其它字段一致。
  `Text.wrap = true` 时文本按矩形宽度**按空格断词**换行（单个词过长按字符断），
  `\n` 始终换行并保留空行；超出行底边的行不再绘制。
- **UI Canvas 缩放（#463）**：给 UI 根节点加 `prism_engine::ui::Canvas`
  （`reference_resolution` / `scale_mode` / `match_width_or_height`）后，整棵子树
  按「设计分辨率 → 实际屏幕」等比缩放：布局在虚拟分辨率里算，写回
  `ComputedLayout.rect` 时乘回系数，字号与圆角也一起放大。三种模式对应
  Unity `CanvasScaler`：`ConstantPixelSize`（默认，1 屏幕像素 = 1 设计像素）、
  `ScaleWithScreenSize`（宽高比例在对数空间按 `match` 插值）、
  `MatchWidthOrHeight`（只用宽或高）。嵌套 Canvas 只继承最近的一层。
  这样 1920×1080 的设计稿在 720p 与 4K 上构图一致，Game 视图选目标分辨率后
  看到的就是真机比例。
- **Game 视图分辨率 / 宽高比**：视口左上角下拉。默认读取工程
  `assets/settings.ini [window]` 的 `width`/`height`（例如 1600×900），
  按该分辨率离屏渲染并等比 letterbox，所以编辑器构图和打包产物一致。
  还可以选固定分辨率（1920×1080 等）或「只锁比例」的 `16:9 (fit)` /
  `4:3 (fit)` 等；`Match Window` 保留旧的跟面板缩放行为。控制通道用
  `resolution:1600x900` / `resolution:fit:16x9` / `resolution:match`。
- **内部分辨率 / 整数缩放（#613）**：工程 `assets/settings.ini [render]` 的
  `internal_resolution = 640x360` + `internal_scale_mode = integer` 会让场景按 640×360
  渲染、再整数倍放大到窗口（居中黑边），相机也按内部宽高比投影 —— 于是美术像素是
  等大的方块。Game 视图与打包产物走同一份配置，`off`（默认）保持按窗口分辨率渲染。
- **2D 网格**：Scene 视图切到 2D / 正交相机时，参考网格画在 `z = 0` 的 XY 平面
  （X 红、Y 绿），而不是 3D 的 XZ 地面；位置和线宽沿用原来的 1m / 10m 分级（#602）。
- **瓦片地图（`prism_engine::scene::Tilemap`）**：一个实体管一整张网格。`tiles` 行优先、
  `0` = 空格、`n > 0` = tileset 第 `n - 1` 块；`columns = 0` 排成一行；第 0 行在上方。
  引擎把非空格子展开成 `SpriteDrawItem`，与精灵共用实例缓冲 / bindless 图集（几百格仍是一次
  draw call），并按主相机视锥逐格裁剪（#529）。检查器的 Transform 下方有网格编辑器：
  `Columns` / `Rows` 改规模，`Palette` 决定左键循环范围，左键递增、右键单格清零。
- **2D 光照（`prism_engine::scene::Light2D`，#611）**：精灵默认不受光照；给场景加一个
  `Light2D` 实体（`kind` = `Point` / `Ambient` / `Directional`），再把需要被照亮的
  `SpriteRenderer.receive_lights` 勾上即可。强度是美术单位（`1.0` = 原色），点光按
  世界 XY 距离衰减（`radius = 0` 不衰减），氛围光适合夜景最低亮度。3D 的
  `PointLight` / `SpotLight` / `DirectionalLight` 不影响 2D，场景里只有 3D 光时会打印
  一次提示。`Light2D` 走反射注册，Add Component 里直接可见。
- **精灵裁剪（`SpriteRenderer.clip_enabled` / `clip_rect`）**：勾上后按**世界空间**
  矩形 `[x0, y0, x1, y1]` 逐像素裁剪（片元 `discard`，不打断 2D 的单 draw call）。
  角点顺序随便写，引擎自己排序；关闭 / 非有限 / 零面积视为不裁剪，所以刚勾上开关
  时精灵不会消失。多个精灵填同一个矩形 = Unity `SpriteMask` 的矩形模式，
  湖面倒影「只显示水线以下」就是 `clip_rect = [-1e4, -1e4, 1e4, 水线 y]`（#582）。
- **多选拖拽**：拖多选中的任一行且落在行的中间区 = `reparent_selection` 整组改父级。
- **资产选择器**：检查器的路径字段右侧有 `⋯` 按钮，按字段名过滤扩展名
  （`mesh_path` → 模型、`sprite_path` → 贴图、`hdr_path`/`env_map` → HDR…），
  选中的相对路径直接写入字段。

### 4.3.1 骨骼动画（#560 P0）

cook 阶段把 glTF 的 `skins` / `JOINTS_0` / `WEIGHTS_0` / `animations` 烘成三份 side asset
（`{mesh}.skinning/skeleton.rskel`、`mesh.rskin`、`clips/{n}_{name}.ranim`），并给场景实体
自动补上 `SkinnedMeshRenderer` + `SkeletonRoot` + `Animator`（作者手写的组件优先，
只有骨架没有权重时保持普通 `MeshRenderer`）。三件套：

- **`Animator`**：检查器里是「▶/⏸、⏮ 重播、时间轴滑杆、Loop、speed」；滑杆下方一条 6px
  刻度带标出所有轨道的关键帧时刻（`Clip::key_times` 合并去重），拖的时候能看出哪里会变。
  改时间 / 勾 Loop 直接写回组件字段（一次撤销步），引擎的 `animator::pose` 系统下一帧按
  新时间重算骨骼调色板。滑杆下方还有一条 Notify 色带（青色小三角）：剪辑里的关键帧事件
  （`{mesh}.skinning/notifies.json` 里的 `Footstep.L` 这类）按时间画在轴上，悬停显示名字，
  一眼能看出这段动画在哪几秒会触发事件。**`blend_duration`**（秒，默认 0.2）是运行时换剪辑的交叉淡入时长：
  **淡出源是上一帧实际显示姿态的快照**（不是「旧剪辑 + 时间」），所以换剪辑、出入混合空间、
  甚至旧剪辑被删 / 热重载，都能正常淡出而不是硬跳；`0` 表示硬切。权重到 1 就丢掉快照。
- **叠加层（additive）**：`Animator.additive_clip_path` + `additive_weight`（默认 1）。叠加
  剪辑的局部变换相对 **reference（bind）姿态**求差，再按权重打到最终姿态上（旋转 `slerp`
  增量、平移加差值、缩放乘比值；参考缩放为 0 时按 1 处理）。所以呼吸 / 瞄准偏移 / 受击
  抖动这类只改一部分骨骼的剪辑不需要再做一份基础剪辑，未覆盖的关节保持不动。它在交叉淡入
  **之后**应用 —— 叠加层不参与两个基础剪辑之间的过渡。路径写错或权重为 0 / NaN 时安静地
  跳过，不会把姿态打坏。
- **Root motion 记录（#560 P0）**：动画系统每帧把根关节（joint 0）相对上一帧的**局部**平移
  与绕 Y 偏航写进 `AnimatorPlayhead.root_motion`（`RootMotionDelta`，米 / 弧度，最短路径）；
  首帧、换剪辑 / 换播放内容、被 LOD 冻结的帧都是零。默认只记录；开启
  `root_motion_consume` 后把根**平移**按实体朝向旋到世界空间加到 `LocalTransform.position`，
  并把姿态根平移清零，避免双倍位移（根旋转仍只记录，等 CharacterMovement 统一处理）。
- **动画 LOD（`lod_enabled` / `lod_near` / `lod_far`，#560 P6）**：默认关闭。开启后实体离
  **启用中的相机**超过 `lod_far` 米（默认 50，非有限或 `<= 0` 也按 50）时，动画系统这一帧
  完全不推进 —— 时间、状态机转移、图求值器与 Montage 全部冻结在当前姿态，回到范围内接着走，
  冻结期间不补时间。`lod_near..lod_far`（默认 25..50）之间隔帧求值：更新帧推进
  `2 * dt`，所以播放速度不变；`lod_near` 非有限 / `<= 0` / 大于 `lod_far` 时没有降频档。
  相机来自场景里第一个 `enabled` 的 `Camera`（读 `LocalToWorld`，没有就读 `LocalTransform`），
  没有相机时 LOD 不生效（编辑器预览不会因为相机没摆好而停住）。
  开启 `lod_root_only` 后，超过 `lod_far` 的**单剪辑**实体不再整体冻结：时间照常推进、
  只采样根关节，其余骨骼保持上一帧姿态（第一次进入该档先完整求值一帧，避免 T-pose 突变），
  根运动消费仍然生效。`lod_bones` 非空时是同一档的骨骼子集版本：根关节 + 名单里的骨骼
  每帧采样，其余保持上一帧；名字按骨架解析，找不到的忽略，根关节总在更新。状态机 /
  混合空间 / 动画图实体因为需要完整求值器推进，仍整体冻结。
- **`SkeletonRoot`**：检查器里画骨骼树（`CollapsingHeader` 按父链缩进，成环有护栏）。
- **混合参数平滑（`blend_smoothing`，#560 P1）**：默认 `0`，直接使用游戏侧写入的
  `blend_parameter` / `blend_parameter_y`；大于 0 时按指数 `1 - exp(-dt / tau)` 趋近目标，
  `tau` 就是该字段（秒）。首帧直接落在目标上，参数非有限或关闭平滑时回到直通。
  只影响混合空间采样，不改组件里的参数值（游戏侧仍然看到自己写的数）。
- **一维混合空间**：`Animator` 的 `blend_clip_paths` / `blend_positions` / `blend_parameter`
  是一条参数轴（速度 0 → 走 → 跑 这种）。填了 ≥2 条剪辑、且 `blend_positions` 与之一一对应
  时，姿态由 `blend_parameter` 在轴上插值得到，**优先于** `clip_path`；参数超出范围夹到两端。
  剪辑时长不同时按**归一化相位**对齐（步幅对齐），走 ↔ 跑不会出现两只脚互相追。少一个样本
  （路径写错 / 资产还没加载）就退回单剪辑，不会让整个角色不动。三个字段都是 `Vec` / 数字，
  检查器里直接可编辑；`blend_clip_paths` 已进资产重命名清单。参数轴在检查器里画成一条
  16px 条带：灰线是每个样本的位置、金色圆点是**当前参数**，点一下或拖动就直接改
  `blend_parameter`（一次撤销步），拖的同时场景里的角色跟着变 —— 这是唯一能看出「现在落在
  哪两个样本之间」的地方。
- **二维混合空间**：`blend_2d_clip_paths` / `blend_2d_positions`（`[x, y]` 与剪辑一一对应）/
  `blend_parameter` / `blend_parameter_y` 组成参数平面（x 通常是速度、y 是方向）。至少 3 个
  样本且不共线时按 Delaunay 三角化 + 重心插值求姿态，**优先于一维混合空间与 `clip_path`**；
  参数落在样本凸包外时负权重夹到 0 再归一化（投到最近的边界），不会把角色弹回某个样本。
  样本不足 / 共线 / 坐标非有限时安静退回一维或单剪辑。检查器里画一块 120px 的参数平面：
  灰点是样本、金色圆点是当前参数，点 / 拖直接改 `blend_parameter` + `blend_parameter_y`
  （一次撤销步）。`blend_2d_clip_paths` 已进资产重命名清单。
- **视口叠加**：工具栏 `Bones`（`Outline` 右边）控制，默认开。选中蒙皮实体时按
  `BonePalette` 还原关节世界位置（`world = palette × inverse_bind⁻¹`），画父 → 子 1.6px
  连线 + 关节圆点（根关节更大）。拖时间轴时叠加层跟着动，因为它读的就是动画系统当帧的
  结果而不是作者数据。`show_skeleton` 与 `show_selection_outline` 一样存进 `state.json`
  的 `scene` 段，旧存档默认开。

**权重热图**：渲染模式下拉框（工具栏 `Shaded` 旁边）选 `Skin Weights`，等价于
`debug_flags = 1 << 15`（游戏里是 `Shift+6`）。顶点阶段按**主导骨骼**算色相（金色角，
相邻骨骼不会撞色）、按该骨骼权重算明度，片元阶段原样输出不做光照，所以一眼能看出
「这块网格到底挂在哪根骨骼上」以及权重是不是均匀过渡；权重全 0 的顶点（坏导入、静态
网格）是中灰，不会被误读成某根骨骼的颜色。这一位不是 PBR 分量，不参与 15 位 isolate 逻辑。

时间轴刻度**可以点**：点中某根刻度（命中半径 4px，取最近的一根）就把 `time` 跳到那个
关键帧，鼠标悬停时刻度加粗并显示 `Jump to this keyframe`；点空白处不跳，避免拖时间轴时
被吸到某个关键帧。

**关键帧事件（Notify，#560 P1）**：`.ranim` v3 在剪辑末尾追加事件表（`time` + 名字）和同步标记表，
引擎每帧把「这一帧跨过的时间点」写进 `AnimEvents` 资源（整体重建，不是累加）：

```rust
for event in world.get_resource::<AnimEvents>().map(|e| e.drain()).unwrap_or_default() {
    match event.name.as_str() {
        "Footstep.L" => play_footstep(Footstep::Left),
        _ => {}
    }
}
```

- 区间是 `(上一帧本地时间, 这一帧本地时间]`，循环回绕时把 `[0, 当前]` 也算上，所以
  「0.9 → 1.0 → 0.1」能同时触发 0.95 和 0.05 两个事件；
- 起点开区间：事件在恰好等于上一帧时间的那一帧已经触发过，停在上面不会反复播；
- **刚创建 / 刚换剪辑 / 暂停 / 倒放都不触发**（倒放需要反向扫区间，暂时宁可不播）；
- **事件从哪来**：glTF 没有事件通道，所以作者写一个侧车文件
  `assets/<mesh>.skinning/notifies.json`（与源文件同级，键是 glTF 里的动画原名）：

  ```json
  { "clips": { "walk": [
      { "time": 0.25, "name": "Footstep.L" },
      { "time": 0.75, "name": "Footstep.R" }
  ] } }
  ```

  cook 时按剪辑名合并进 `.ranim`（编码按时间排序、丢掉负时间/空名字）；文件**存在但
  解析失败会让 build 失败**，名字对不上任何动画会 warn —— 打错的事件表不该静默消失。
- 编辑器可以编辑事件表：在资产面板选中 `assets/<mesh>.skinning/notifies.json` 后打开
  Window → General → Animation Graph，按剪辑列出每条事件（时间 DragValue + 名字文本框，
  失焦写回），`×` 删除，`+ Notify` 草稿表单可以给已有或新的剪辑追加一条。每个剪辑上方还有
  一条时间轴：金色圆点是事件，悬停显示「名字 @ 秒数」，左右拖动直接改时间（量化到 0.01s，
  夹在 0..=窗口长度，窗口长度按最晚事件自动取 ≥2s），和 DragValue 共用同一个写回路径。
  在空白处点一下会在该时间打开新增草稿（剪辑名与时间已填好，只差事件名），点已有标记不会新建。
  保存后由松散资产热重载重新 cook，`.ranim` 里的 v3 事件表随之更新。

**同步标记（Sync marker）**：走路 / 跑步循环的脚落地相位不同，混合时可以按标记对齐。
作者写侧车 `assets/<mesh>.skinning/sync_markers.json`，键是 glTF 动画名，值是归一化相位：

```json
{ "clips": { "walk": [0.0, 0.5], "run": [0.05, 0.55] } }
```

cook 时归一化到 `[0, 1)`、排序去重，写进 `.ranim` v3；运行时
`Clip::sync_marker_set()` 返回 `SyncMarkerSet`，可以构造 `SyncGroup` 做相位映射。
文件不存在 = 没有标记；存在但解析失败会让 build 失败；名字对不上任何动画会 warn。

### 状态机

状态机写成 `assets/<something>.statemachine.json`，cook 成 `.rasm`（RASM v1：状态表 +
转移表 + 条件；`from` 用 `u32::MAX` 表示「任意状态」）：

```json
{
  "states": [
    { "name": "Idle", "clip": "char/idle.ranim" },
    { "name": "Run",  "clip": "char/run.ranim", "speed": 1.2, "looped": true }
  ],
  "transitions": [
    { "from": "Idle", "to": "Run", "duration": 0.2, "min_state_fraction": 0.0,
      "conditions": [{ "type": "float_greater", "name": "speed", "threshold": 1.5 }] }
  ]
}
```

- 条件种类：`float_greater` / `float_less`（`threshold`）、`bool_is`（`value`）、`trigger`；
- 缺省：`speed = 1`、`looped = true`、`from = "*"`（任意状态）、`duration = 0`（硬切）、
  `min_state_fraction = 0`；`to` / `from` 写错状态名是**硬错误**（整份资产失败），
  重复状态名、空的 `clip` / `name`、未知条件种类同样报错并指出是哪一条；
- 求值是**按作者顺序第一条命中**（条件全真 + 当前状态已播到 `min_state_fraction`），
  一帧只走一条转移；`min_state_fraction` 在剪辑时长未知时视为已播完，否则单次剪辑会卡在最后一帧；
编辑器提供 **Animation Graph** 面板（Window → General）：选中 `.animgraph.json` 或
`.statemachine.json` 后显示参数、节点 / 连线、输出，以及状态、转移和可读条件。
面板顶部有 `↶ Undo edit` / `↷ Redo edit`，只针对动画资产（图 / 状态机 / Notify）的
JSON 改动，和场景的 Ctrl+Z 是两条独立历史：每步存整份文件快照，连续拖拽（DragValue /
时间轴）在 800ms 内合并成一步；参数改名这种同时改多个文件的操作整组算一步，撤销 / 重做
一起回滚。撤销后写回磁盘、下一次重绘直接用磁盘内容。
图的面板顶部有一行形状检查结果（`shape check: ok` / `Shape issues (n)`）：复刻 cooker 在
cook 前会拒绝的形状问题 —— 节点名 / 类型、剪辑与坐标数量、连线目标与自连、输出数量、
环、参数类型、每样本速度数量 —— 在保存前就列出来。它不读资产内容，剪辑 / 状态机文件
是否存在仍以 cook 为准。

按 **Play** 后（Game 视图在跑）面板多出一块绿色 `Runtime · N entities`：每个动画图实体的
节点按图顺序列出名字 / 类型 / 连线权重 / 本地时间，状态机节点显示 `状态名 · 时间`；下面还有
该实体各 Slot 的 Montage（剪辑、时间、权重、暂停 / 播完 / 排队段数），以及这一帧该实体
触发的 Notify（蓝色 `• 名字 @ 时间`，来自引擎的 `AnimEvents`，每帧重建）。数据来自引擎的
`AnimGraphDebug` 资源，只在 Play 时插入，停止后移除；编辑器不复制状态，面板每帧只读。
`Parameters` 区可以直接编辑：每行是名字文本框 · 类型 + 默认值 DragValue（bool / trigger 归一成
0 / 1），`×` 删除参数（仍被混合空间节点引用的参数会被拒绝并指出是哪个节点），`+ Parameter`
打开草稿表单（名字 + 类型下拉 + 默认值）。改参数名（失焦）会同时改写混合空间节点的
`parameter` / `parameter_x` / `parameter_y`，以及图引用的状态机侧车里的条件名：
节点写 `state_machine: "anim/hero.rasm"` 时自动找 `assets/anim/hero.statemachine.json`
（文件不存在就跳过，可能还没落盘），状态栏会列出同步了哪些文件。重名会被拒绝。
图节点可以在画布上拖动，位置保存到 `<graph>.layout.json`（不改图本身）；节点卡片里可以增删
`inputs` 连线（`+ Link` / `×`），每条连线的 `w` 可以直接拖拽调 0..1 权重，保存后写回 `.animgraph.json`。使用单数 `input` 的链式节点
仍需手改 JSON。每条连线下面还有一行逗号分隔的 `bone mask`（`spine_02, arm_l`）：失焦时才写回，
字符串连线需要掩码时自动升级成 `{ node, weight, bone_mask }`；清空这行等于去掉掩码（整条姿态）。
剪辑节点的剪辑路径是文本框（失焦写回），每个节点还有 `speed` / `w` DragValue 和 `loop` 复选框。
节点名也是文本框：重命名会同步改写其它节点的 `inputs` / `input` 与顶层 `output`，但布局 sidecar
里还是旧名字，节点会回到画布的默认网格位置。
混合空间节点额外显示参数名文本框（二维各一个）和每个样本的 `x` / `y` DragValue（一维只写 `x`）；
二维节点还有一块 180×120 的样本画布：按坐标画出样本点，直接拖动点就改 `positions`（与 DragValue
共用写回路径），坐标范围标在画布下方。所有样本共线时画布下方给橙色提示「退化」——运行时三角化
会失败并退回一维 / 单剪辑，数字上不容易看出来；
节点卡片上的 `⧉ Duplicate` 复制整份节点（剪辑 / 坐标 / inputs / bone mask 原样保留），
名字自动加 `_copy` / `_copy2`；复制出的节点接线仍指向原来的上游，输出节点不能复制。
`clips` 与 `positions` 数量对不上时用橙色提示，cooker 会拒绝这种节点。样本行尾的 `×` 删除样本、
`+ Sample` 打开草稿表单（剪辑 + x/y），一维至少留 2 个、二维至少留 3 个；节点带每样本 `speeds`
时增删会同步维护那张表。`+ Clip Node` 追加一个 `clip_player` 节点（必需字段只有剪辑路径，当帧就能通过
cooker 校验）；`× Remove node` 删除节点，输出节点和仍被引用的节点会被拒绝并说明原因，先断线再删。
其它节点类型（blend space / 状态机 / blend stack）需要剪辑坐标、机器路径或至少一条输入才合法，
仍然手写 JSON。状态机视图支持 `+ Transition`（任意状态 / 指定 from → to）和 `× Remove transition`，
保存回 `.statemachine.json`。展开一条转移后 `from` / `to` 是下拉框（`from` 可选 `any`），
重连会拒绝重名状态、自连和与现有转移完全重复的 `(from, to)`；`↑` / `↓` 调整转移顺序，
顺序就是优先级（运行时取第一条条件命中的转移，列表上也写了这句提示）。状态行末尾的 `⧉` 复制整个状态（剪辑 / speed / looped 原样保留，名字自动 `_copy`），
转移表不变，复制出来的状态需要自己接线。状态行的名字与剪辑是文本框（失焦写回）、`speed` 是 DragValue（0..5）、
`loop` 是复选框；重命名会同步改写转移里的 `from` / `to`，但不会改运行时 `Animator.state`
或 `.scene.json` 里按旧名字写死的字段，改名后要自己检查场景。每条转移的 `blend` / `min` 是 DragValue（混合时长 / 最小状态占比）；
条件行可以直接改 float 阈值或 bool 期望值、`×` 删除，`+ Condition` 打开草稿表单（类型下拉
`float >` / `float <` / `bool ==` / `trigger`、参数名、阈值或布尔值），点 `Add condition` 才写盘。
条件只校验形状（名字非空、类型已知、`(type, name)` 不重复），参数是否真在运行时的
`AnimParameters` 里仍由运行期决定。视口里直接拖关节做 IK（P2 的 IK 另立）还没做。

运行时（#560 P1）：`Animator.state_machine_path` 指向 `.rasm`、`state` 是当前状态名（引擎每帧
写回，可在检查器里预置起始状态）。状态机生效时：

- **状态决定播什么**：`state.clip_path`、`speed`、`looped` 覆盖组件上的同名字段（状态剪辑为空
  时才回落到 `clip_path`）；
- 转移由 **`prism_anim::AnimParameters`** 资源驱动（项目侧每帧写 `speed` / `grounded` / trigger
  等），引擎在 `animator_system` 里求值；一帧最多走一条转移，换状态时播放时间归零；
- `min_state_fraction` 用**上一帧**的播放时间判定（那一帧确实播到了那里），所以跨过阈值的那
  一帧还拦着、下一帧放行；
- 转移自己的 `duration` 覆盖组件的 `blend_duration`（那条转移说了算），淡出源仍是上一帧姿态；
- 机器路径写错 / 还没加载时安静退回 `clip_path`，不会让实体消失。
- 检查器的 `Animator` 卡片里显示当前状态与机器文件名（只读）。


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
`ui.snapshot` / `ui.click` / `ui.move` / `ui.wheel` / `ui.drag` / `ui.set` /
`quit` / `quit.confirm` / `quit.cancel`。
`quit` 是自动化用的强制退出（不写恢复快照、不弹确认）；`quit.confirm` 走用户点窗口 ×
的路径——有未保存文档时返回 `closing:false` / `pending:"unsaved"` 并弹出三选一，
`quit.cancel` 关掉该对话框。
`logs` 读 Console 的同一份缓冲（`since` 增量、`limit` 条数），自动化因此能拿到引擎日志、
工程模块的 `project:` 日志和 panic 栈。

视口里的指针操作也走控制通道（#643），坐标单位是**视口局部逻辑像素**：

| 命令 | 参数 | 行为 |
|---|---|---|
| `ui.move` | `node`+`offset` 或 `tab`+`x`/`y` | Scene：返回该点下的实体（`hit`）与 gizmo 轴（`gizmo_axis`），不改选择；Game：把光标送进玩法输入 |
| `ui.wheel` | 同上 + `delta`（默认 120） | Scene：缩放到光标（`Camera::zoom_towards`，#511）；Game：滚轮进玩法输入 |
| `ui.drag` | `from` / `to` + `steps`（默认 8） | Scene：起点命中 gizmo 就按真实拖动逻辑变换（吸附、撤销步一致），否则 `mode:"orbit"` 转轨道相机，再否则框选；Game：按下 → 逐步移动 → 松开 |

`from` / `to` 是对象：`{"node":"view:scene:entity:2","offset":[40,0]}`（物体节点锚点是
其 `screen_rect` 中心）或 `{"x":220,"y":610}`。Scene 的框选结果直接写进选择集，回执带
`selected` 节点列表；gizmo 拖动回执带 `changed`，用于断言变换真的发生了。
`ui.click` 的 Game 分支也走同一套 letterbox / 离屏尺寸换算（#463/#585）。

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

`editor/crates/prism-editor-core` 是与 UI 框架无关的数据层：文档模型、schema、撤销栈、dock 树、日志缓冲、资产树、i18n、项目读写。
egui 壳通过路径依赖直接使用它。

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
