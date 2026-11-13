# 版本号规范

适用于 `prisma-rev` 仓库的产品版本（引擎、编辑器、启动壳、构建工具）。
用户游戏项目（`projects/*`）的版本、格式版本和 ABI 版本是**另外三件事**，见第 5 节。

## 1. SemVer 规则

产品版本统一为 `MAJOR.MINOR.PATCH`。当前处于 `0.y.z`（当前版本 `0.1`）：

> **1.0 前零兼容逻辑**：在 1.0 版本正式发布前，**不考虑任何兼容性逻辑，旧版本兼容被明确视为冗余的垃圾代码**。
> 严禁编写任何向前/向后兼容层、废弃垫片、兼容门面或旧版本数据回退分支。所有改动直接按最新最优设计替换/删除旧实现。

| 变更 | 版本位 | 例子 |
|---|---|---|
| 破坏公共游戏 API、项目模块 ABI、项目文件兼容性 | `MINOR`（1.0 前） | `0.1.0 → 0.2.0` |
| 新增向后兼容能力、内部重构、性能改进 | `PATCH` | `0.2.0 → 0.2.1` |
| 纯修复 | `PATCH` | `0.2.1 → 0.2.2` |
| 1.0 之后的破坏性变更 | `MAJOR` | `1.4.0 → 2.0.0` |

- 预发布：`0.2.0-rc.1`，只用于发布候选；`bump` 会丢掉预发布后缀，正式发布用
  `python scripts/version.py set 0.2.0`。
- 构建元数据（`+sha`）不进入仓库版本号；nightly 文档站点用单独的标签。
- `editor` 与引擎同版本发布；Hub 通过 `editor/Cargo.toml` 识别编辑器版本，两者必须一致。
- 1.0.0 的门槛是**公共游戏 API 稳定**（`prism-game-api` / `prism-project-api` 不再频繁
  破坏兼容），不是功能数量。

## 2. 单一版本源

唯一源：根 `Cargo.toml` 的 `[workspace.package].version`。

根 workspace 的所有 crate 使用 `version.workspace = true`。独立 workspace / 前端无法
继承 Cargo 的 workspace 版本，由 `scripts/version.py` 同步：

| 文件 | 字段 | 角色 |
|---|---|---|
| `Cargo.toml` | `[workspace.package] version` | **源** |
| `editor/Cargo.toml` | `[workspace.package] version` | 同步 |
| `launcher/package.json` | `version` | 同步 |
| `launcher/src-tauri/Cargo.toml` | `[package] version` | 同步 |
| `launcher/src-tauri/tauri.conf.json` | `version` | 同步 |
| `crates/xtask/Cargo.toml` | `[package] version` | 同步 |
| `Cargo.lock` / `launcher/src-tauri/Cargo.lock` | 包版本条目 | 由 `cargo` 刷新，脚本不手改 |

不参与同步：

- `projects/*/Cargo.toml`：用户游戏自己的版本；
- `tools/discord-bot/package.json`：独立工具；
- `prisma-hub`、`prisma-web-tutorial`：独立仓库；
- `launcher/src-tauri/gen/**`：生成物（`tauri.properties` 等未跟踪）；
- `crates/prism-editor-mcp/Cargo.toml`：已改为 `version.workspace = true`，不需要加进同步表。

## 3. 命令

```bash
python scripts/version.py show                  # 打印当前版本
python scripts/version.py check                 # 校验所有同步点
python scripts/version.py check --expect 0.2.0  # tag / 发布流程用
python scripts/version.py bump patch|minor|major
python scripts/version.py set 0.2.0
python scripts/version.py tag                   # 打印应打的 tag：v0.2.0
```

- `--dry-run`：只打印会改哪些文件，不写入。
- `--markdown`：输出一行 `| 版本一致性 | ✅/❌ | ... |`，供 `pr_ready.py` / CI 使用。
- `--quiet`：只输出摘要。
- `bump` / `set` 会更新第 2 节的所有同步点；改完运行 `cargo check`
  （或 `cargo metadata`）刷新 `Cargo.lock`，并提交 lock 的变化。
- **不要手改**同步点里的版本号；`check` 会把漂移变成红灯。

## 4. 发布流程

1. 确认 `master` 上 CI 全绿。
2. 更新 `CHANGELOG.md`（版本段 + 日期 + 条目）。
3. 运行 `python scripts/version.py bump <major|minor|patch>`（或 `set X.Y.Z`）。
4. 运行 `cargo check` 刷新 `Cargo.lock`；确认没有意外依赖变化。
5. 提交：`chore(release): 0.2.0`（版本号 + CHANGELOG 可以同提交）。
6. 打 annotated tag：`git tag -a v0.2.0 -m "PrismaRev 0.2.0"`；
   `python scripts/version.py tag` 打印正确的 tag。
7. 推送分支和 tag；创建 GitHub Release，release notes 从 `CHANGELOG.md` 取。
8. 文档站点工作流按 tag 自动归档到 `/v0.2.0/`（站点上线后；见第 6 节）。

Tag 规则：

- 格式固定 `vMAJOR.MINOR.PATCH`，预发布 `v0.2.0-rc.1`。
- tag 指向通过 CI 的 commit；tag 上的 `Cargo.toml` 版本必须与 tag 一致
  （`version.py check --expect`）。
- 已发布的稳定 tag 不移动、不重打。

## 5. 格式版本与 ABI 版本

这些数字**不跟随产品版本**：

| 版本 | 位置 | 何时 +1 |
|---|---|---|
| `ABI_VERSION` | `crates/prism-project-api` | 项目模块 C ABI / `HostApi` 表变化 |
| RSCN `version` | `prism-scene` | 场景二进制布局 / 语义变化 |
| RTTIBin `version` | `prism-reflect` | 自描述二进制格式变化 |
| `.pak` 版本 | `prism-asset` | 包容器布局变化 |
| RTEX / RMES / RSKL / RSKN / RANI / RASM / RAGP | `prism-cooker` / `prism-asset` | 对应 cooked 资产格式变化 |

规则：

- 产品版本升级**不自动**改格式 / ABI 版本；格式升级也不强制产品 `MAJOR` 变化。
- **1.0 前零兼容逻辑**：在 1.0 之前，不为旧版本格式或 ABI 保留任何回退解析代码、垫片或兼容门面；旧数据/旧格式解析直接删除替换，旧版本兼容代码明确视为冗余的垃圾代码。
- 破坏格式兼容性时：格式版本 +1、更新对应 `docs/*.md`；
  0.y 阶段通常同时升产品 `MINOR`，让用户知道需要重新烘焙/导出资产。
- 旧数据读不了时必须明确报错失败，不编写静默降级或回退逻辑。

## 6. 文档站点的版本目录

站点上线后，目录由版本自动决定，不手工维护：

- tag `vX.Y.Z` → `/vX.Y.Z/`；
- `master` → `/nightly/`；
- `/latest/` → 最新稳定 tag；
- `versions.json` 从各目录的 `meta.json` 扫描生成。

产品版本只决定目录名和页面上的版本标签；API 内容由该 tag 的 `cargo doc` 产物决定。
托管方式（GitHub Pages / Cloudflare / 自建）尚未定，构建脚本的输出与托管无关。

## 7. 自动检查

- `scripts/pr_ready.py` 始终运行 `version.py check`（快速、只读）。
- CI 的 `lint` job 运行 `version.py check`，任何同步点漂移都会红灯。
- 发布 / 文档工作流用 `version.py check --expect "$VERSION"` 校验 tag 与清单一致。
- `CHANGELOG.md` 与 `Cargo.toml` 的版本段不一致时，发布流程应停在本地的第 2 步。

## 8. 常见问题

**改了版本但 `Cargo.lock` 没变？**

路径依赖的版本变化通常需要 `cargo check` 才会写回 lock；在根目录和
`launcher/src-tauri/` 各跑一次对应命令。提交前用 `git status` 确认 lock 的变化符合预期。

**独立 workspace 的版本为什么会漂移？**

Cargo 不支持跨 workspace 继承版本，只能同步。`version.py check` 是唯一的防线；
新增独立 workspace 时，把它加进 `TARGETS`，不要只写在文档里。

**预发布怎么发？**

`set 0.2.0-rc.1` → 打 `v0.2.0-rc.1` → 测试通过后 `set 0.2.0` → 打 `v0.2.0`。
`bump` 不用于“从 rc 转正”，因为它会直接加版本位。
