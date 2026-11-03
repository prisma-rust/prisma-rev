# editor

PrismaRev 的 **egui 编辑器**（egui + ash Vulkan）。独立 workspace，不挂根 workspace。

- 沿用经典桌面编辑器布局：菜单栏 / 主工具栏 /
  状态栏 + split-tree docking（可拖 splitter、tab 切换、浮动窗口）。
- 菜单 / 快捷键按项目约定（含 `Ctrl+1..5` 面板、命名布局、Prefab Variant）。
- 交互：菜单二/三级、层级拖拽改父级、检查器多选批量、W/E/R 三种 gizmo、
  框选、标签页拖拽分屏/浮动、资产拖拽（目录移动 / 层级实例化 / 字段写入）。
- 数据层完全复用 `editor/crates/prism-editor-core`（UI 框架无关：
  文档模型、schema、撤销栈、dock 树、日志缓冲、资产树、i18n、项目读写）。
- **Scene / Game 视口零拷贝**：`prism-render` 渲染到自己的离屏 `VkImage`，
  编辑器和引擎共用同一个 `VkDevice` / 队列，egui 直接把该图像的 `VkImageView`
  注册成纹理采样。没有 `vkCmdCopyImageToBuffer`，也没有 CPU 像素回读。

## 运行

```bash
cd editor
cargo run -p prism-editor-egui
```

前置条件：

- 桌面 Vulkan 设备（Windows / Linux；Android 需要额外的 winit 后端口径，暂未接）；
- 已编译的着色器：`bash assets/shaders/compile.sh`（`.spv` 不入库）；
- 项目根（含 `assets/`）可从 cwd 或上级目录找到；找不到时用 `editor/public/demo.json`
  的只读 fixture 起界面。

可选环境变量：

| 变量 | 作用 |
|---|---|
| `PRISMREV_EDITOR_DEMO` | 指定 fixture JSON 路径 |
| `RUST_LOG` | 日志级别（默认 `warn`） |

## 截图 / 被 MCP 驱动

编辑器启动时会在 `127.0.0.1:0` 上开一个本地控制通道，并把端口写进
`<项目根>/target/editor-egui/control.port`。`crates/prism-editor-mcp` 用它驱动
预览：`editor_launch` / `editor_status` / `editor_capture` / `editor_quit`
（详见 `docs/editor-mcp.md`）。菜单「Window → Capture Window」是同一套实现的本地入口。

- `window`：整窗 PNG（UI + 视口）。Windows 走 `PrintWindow`，DWM 合成后取样，
  窗口部分在屏幕外也能抓全；其它平台不支持。
- `scene` / `game`：只抓该视口的引擎离屏图（`PRESENT_SRC_KHR` 读回），
  不需要窗口，无显示器 / CI 可用。

界面本身也能被 agent 读和操作（`editor_ui`）：`snapshot` 返回一棵 UI 节点树
（dock 面板 / 激活 tab / 打开的文档 / 层级实体 / 资产条目 / 检查器组件字段 /
`Command` 列表），节点 id 形如 `panel:hierarchy`、`entity:3`、`doc:0`、
`asset:assets/x.prefab.json`、`field:3:Transform:position[1]`、`command:Save`；
`click` / `set` 按 id 走和界面相同的代码路径（`ui_tree.rs`）。

## 结构

```
crates/prism-editor-egui/src/
├── main.rs          egui-ash 入口（AppCreator + RunOption）
├── gpu.rs           共享 VkDevice：instance / 物理设备 / 设备 / 分配器 / 命令池
├── build.rs         构建 / cook 子进程（`prism-build-pipeline`），输出进 Console
├── control.rs       本地控制通道（TCP + 一行 JSON：ping/status/capture/quit）
├── screenshot.rs    内置截图：整窗（PrintWindow）/ 视口（离屏读回）
├── app.rs 外壳：引擎式菜单（二/三级）、工具栏、状态栏、docking、Play
├── engine_host.rs   进程内引擎宿主：Engine + GraphRenderer + 资产上传（同步）
├── viewport.rs      Scene/Game 视口：离屏图像 → egui 纹理、轨道/拾取/gizmo
├── panels.rs        层级、检查器、资产、文档、日志、历史、渲染图 / 设置类面板
├── logging.rs       `log` tee：引擎 / 渲染警告同时进 Console 面板
├── widgets.rs       schema 驱动的字段控件
└── theme.rs         数据层主题 token → egui visuals + CJK 字体回退
```

## 零拷贝链路

```text
prism-render GraphRenderer（离屏 OffscreenTarget，R8G8B8A8_UNORM）
  └─ 帧结束：PRESENT_SRC_KHR → SHADER_READ_ONLY_OPTIMAL（同队列 barrier）
        └─ ImageRegistry::register_user_texture(image_view, sampler) → TextureId
              └─ egui painter.image(TextureId, rect, uv)  （同一个 VkDevice / 队列）
```

- 只有一条队列、一个线程（egui-ash 的事件循环线程），不需要跨线程 semaphore；
- 静止的编辑视口**零 GPU 工作**：只有文档变化 / 相机变化 / 选中变化 / resize /
  Play 时才重渲染，其余帧复用上一帧纹理；
- resize 时 `GraphRenderer::resize_offscreen` 重建图像（先 `device_wait_idle`），
  编辑器注销旧纹理 id、注册新图像视图。

## Scene vs Game

| | Scene View | Game View |
|---|---|---|
| 相机 | 编辑器轨道相机（`depth = 1000` 的 ECS 相机实体） | 文档里的相机 |
| 模拟 | 永远编辑态（只传播层级） | Play 时跑 `fixed_update` / `update` / `late_update` |
| 引擎实例 | 自己的 `Engine` + `GraphRenderer` | 自己的 `Engine` + `GraphRenderer`（首次显示时创建） |

两个视口各持有一个引擎实例，所以各自的
相机不会互相干扰；文档变化时两份世界一起重新加载。

## 已验证 / 未验证

- ✅ `cargo check`（Termux aarch64，用占位 `.spv` 编译）；
- ✅ `cargo check --target x86_64-pc-windows-gnu`；
- ❌ **未在真实 Vulkan 设备上运行过**：当前开发环境是 Termux（无显示 / 无
  Vulkan 设备），运行时行为（首帧呈现、resize、字体加载、docking 拖拽手感）需要
  在桌面上复核。
- ❌ Android：egui-ash 支持 `android-game-activity`，但本 crate 还没有接入
  `android_main`（Android 上还没有接入 egui 编辑器）。
