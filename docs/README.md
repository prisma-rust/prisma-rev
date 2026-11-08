# PrismaRev 文档地图

`docs/` 只放**长期有效**的文档。会话计划、临时分析、一次性实施清单不入库：
按 [`ai-workflow.md`](ai-workflow.md) §9 放在 `.zcode/plans/`（gitignored）。
计划完成后，结论合并进对应权威文档，计划文件直接删除（git 历史可查）。
因此本目录不再保留 `plans/` 或已实现的设计稿。

## 先读哪一篇

| 想做什么 | 读这个 |
|---|---|
| 了解引擎定位、架构原则、渲染/资源管线 | [`DESIGN.md`](DESIGN.md) |
| 查代码地图、硬约束、坐标约定、工作流红线 | [`../AGENTS.md`](../AGENTS.md) |
| 提 PR / 跑自检 / 分支与提交规范 | [`ai-workflow.md`](ai-workflow.md) |
| 写代码注释 / 统一注释风格 | [`code-style.md`](code-style.md) |
| 给人看的贡献简版 | [`../CONTRIBUTING.md`](../CONTRIBUTING.md) |
| 查踩过的坑和根因 | [`lessons-learned.md`](lessons-learned.md) |
| 改反射、RTTIBin、RSCN | [`REFLECT.md`](REFLECT.md) |
| 改动画图 / `.rag` 格式 | [`anim-graph.md`](anim-graph.md) |
| 用或改 egui 编辑器 | [`editor-egui.md`](editor-egui.md) |
| 通过 MCP 改场景 / 跑游戏 / 抓图 | [`editor-mcp.md`](editor-mcp.md) |
| 打 Android APK | [`android-apk.md`](android-apk.md) |
| 宿主机是 aarch64 时打 Android | [`android-arm64.md`](android-arm64.md) |
| 在 Termux 上从零编译 | [`build-guide-termux.md`](build-guide-termux.md) |
| 改编辑器 / 项目模块边界 | [`project-module-architecture.md`](project-module-architecture.md) |
| 改移动端光追 / GI | [`mobile-raytracing-gi-design.md`](mobile-raytracing-gi-design.md) |
| 改 Slang / bindless | [`slang-bindless-migration.md`](slang-bindless-migration.md) |
| 接 GitHub / Discord / agent 自动化 | [`integrations.md`](integrations.md) |
| 给 agent 提 issue | [`agent-issues.md`](agent-issues.md) |
| 看渲染效果灵感笔记 | [`shadertoy-inspirations.md`](shadertoy-inspirations.md) |
| 看示例游戏（Cozy Fishing）设计 | [`../projects/fishing/DESIGN.md`](../projects/fishing/DESIGN.md) |

## 文档约定

- **`DESIGN.md` 是意图的 source of truth**；`AGENTS.md` 是代码地图与硬约束。
  两者冲突时，技术约束以 `AGENTS.md` 为准，流程以 `ai-workflow.md` 为准。
- 代码与文档在**同一个 PR** 更新；行为或约定变了就同步改这里列出的对应文档。
- 新特性如果需要长期保留设计理由，直接写进 `docs/`（必要时新建分文档并在上表登记）；
  不要再建 `docs/plans/` 长期档案。
- 已完成 / 已废弃的计划不要留在仓库里：先把仍有价值的结论写进上表文档，再删除计划。
