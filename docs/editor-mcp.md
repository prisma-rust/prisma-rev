# Editor MCP server

`crates/prism-editor-mcp` 把编辑器数据层（`prism-editor-api`）暴露成 MCP 工具，
让 agent 直接改场景、跑游戏、抓图，不用手改 JSON 再重新 cook。

它**不链接** engine / render，也不开 Vulkan：读写走 `prism-editor-api` 的反射
校验，需要画面时用 `screenshot` 起一个子进程。

## 启动

```bash
cargo run -p prism-editor-mcp -- --project projects/fishing
```

项目根也可以用 `PRISMREV_PROJECT` 指定；都没给时从 cwd 向上找 `assets/`。
传输是 stdio，一行一条 JSON-RPC 消息。

在 `.mcp.json` 里挂上去（示例）：

```json
{
  "mcpServers": {
    "prisma-editor": {
      "command": "cargo",
      "args": [
        "run", "--quiet", "-p", "prism-editor-mcp", "--",
        "--project", "projects/fishing"
      ]
    }
  }
}
```

## 工具

| 工具 | 作用 |
|---|---|
| `documents_list` | 列出场景 / 预制体（项目相对路径） |
| `document_read` | 读完整文档（已迁移到当前格式，实体带稳定 `id`） |
| `document_write` | 校验后整体写回 |
| `schema_list` | 所有组件 + 嵌套结构体的字段、枚举变体、只读标记 |
| `entity_get` | 按 `id` 或名字取实体 |
| `field_set` | **反射写入**一个字段（`a.b` / `list[0].x` 都支持） |
| `component_add` / `component_remove` | 挂 / 摘组件（默认值来自 schema） |
| `game_run` / `game_stop` / `game_logs` | 起停游戏、读日志尾部 |
| `screenshot` | 起游戏 → 抓图 → 结束，返回 PNG 路径 |

### `field_set` 为什么安全

写入走 `prism_editor_api::reflect_edit::set_component_field`：
`from_object` 还原组件 → `Reflect::set_field` 写字段（`#[reflect(read_only)]`
被拒、类型经反射校验）→ `object_to_json` 还原作者 JSON。所以非法类型、未知
枚举变体、只读字段都不会落到磁盘上。

### `screenshot`

参数：`path`（必填）、`size`（`1920x1080`）、`scale`（窗口倍数）、`frame`
（默认 120）、`timeout_ms`、`command`、`env`。

它复用引擎的内置截图（`PRISMREV_SCREENSHOT*`，见 `AGENTS.md`），所以抓的是
**屏幕上那一帧**；缩放是最近邻，像素画放大不糊。

`command` 默认 `cargo run --release`，在项目根目录执行。想跳过 cargo 的启动
开销就直接给二进制路径，例如
`"command": "./target/release/fishing"`。

## 边界

- 只做数据 + 进程，不做渲染。想改渲染器行为要回引擎 crate。
- 不实现 resource / prompt / sampling；`tools/list` 之外只有 `ping`。
- 没有并发保护：同一个文档别同时开两个 agent 写（`document_write` 是整文件覆盖）。

## egui 编辑器（`editor/`）

egui 编辑器是**独立进程**（Rust + egui-ash），窗口截图和视口零拷贝图像都只能
在它自己的进程里取，所以它开了一个本地控制通道，MCP 通过它下命令：

```text
editor 启动 → 监听 127.0.0.1:<随机端口> → 写 <项目根>/target/editor-egui/control.port
MCP 工具        → 读端口文件 → TCP 一行 JSON 请求 / 一行 JSON 回执
```

- 只监听回环；端口文件在编辑器退出时删除（`PRISMREV_EDITOR_CONTROL` 可覆盖成端口号）。
- 请求在 UI 线程处理，所以截图和界面状态是**同一帧**的。

| 工具 | 参数 | 作用 |
|---|---|---|
| `editor_launch` | `command?` `wait_ms?` | 起 `editor/target/{release,debug}/prism-editor-egui`（没构建过退回 `cargo run`），等控制通道就绪 |
| `editor_status` | — | 帧数、当前场景、选中实体、Play / 暂停、未保存标记、项目根 |
| `editor_capture` | `target`(`window`/`scene`/`game`) `path?` `size?` | 抓整窗（UI + 视口）或某个视口的引擎离屏图，返回 PNG 路径 |
| `editor_ui` | `action`(`snapshot`/`click`/`move`/`wheel`/`drag`/`set`) `node?` `offset?` `from?` `to?` `value?` | UI 节点树：快照 / 按节点点击 / 视口指针移动、滚轮、拖动 / 按节点设值 |
| `editor_play` | `action`(`play`/`stop`/`pause`) | 控制 Play（Play 会触发工程模块编译 + 加载） |
| `editor_logs` | `since?` `limit?` | 读 Console 缓冲：引擎日志 + 工程模块 `project:` 日志 + panic 调用栈 |
| `editor_input` | `key?` `mouse?` `state?` `click?` | 给 Play 注入输入（每帧一个事件），`click=true` 点一下左键 |
| `editor_quit` | — | 走正常退出路径（会写 `state.json`） |

`editor_status` 里的 `project_module` 汇报 Game View 的用户项目模块：
`available` / `loaded` / `building` / `auto_refresh` / `state`（`ready` / `pending` /
`compiling` / `failed` / `unavailable`）/ `dirty` / `problems` / `manifest` / `error`。
模块的编译和重载由编辑器自己完成（见 `docs/editor-egui.md` §3.2）。

### UI 节点树（`editor_ui`）

egui 没有 DOM，也拿不到可以外部点击的控件句柄，所以节点树来自**数据模型** +
每帧记录的 dock 面板矩形，`click` / `set` 走和界面完全相同的代码路径（`Command` /
`Workspace::select` / `Workspace::set_field`），不注入合成鼠标事件。

`snapshot` 返回：`dock`（每个 tab 一个节点，激活的那个展开子节点，带
`bounds`）、`documents`、`assets`、`commands`，以及激活面板的子节点
（`hierarchy` → 实体、`inspector` → 组件字段、`log` → 最近 50 行）。

| 节点 id | 例子 | `click` | `set` |
|---|---|---|---|
| `command:<Name>` | `command:Save` | ✅ | — |
| `panel:<tab>` | `panel:hierarchy` | ✅（已开则切 tab，未开则打开） | — |
| `doc:<index>` | `doc:0` | ✅ | — |
| `entity:<id>` | `entity:3` | ✅（选中） | — |
| `asset:<path>` | `asset:assets/prefabs/a.prefab.json` | ✅ | — |
| `field:<entity>:<component>:<path>` | `field:3:Transform:position[1]` | — | ✅ |

`field` 的 `path` 就是组件 JSON 的路径：对象用点号（`position.x`），数组既给出整体
（`position` = `[x,y,z]`）也给出下标（`position[1]`）。路径不存在时 `ui.set` 会
**报错并列出可用路径**（`Workspace::set_field` 自己找不到字段是静默返回的）。
写回走 `Workspace::set_field`，所以是一次可撤销的字段修改。

`target=window` 走 Windows `PrintWindow`（`PW_RENDERFULLCONTENT`）：DWM 合成
之后取样，所以 Vulkan 交换链内容和 UI 一起抓到，窗口部分在屏幕外也行；其它平台
只支持 `scene` / `game`（引擎离屏图，不需要窗口）。编辑器菜单
「Window → Capture Window」是同一套实现的本地快捷键。
