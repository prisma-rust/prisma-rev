# PrismaRev Sponza Path Tracing

这是与 `projects/prisma/` 同属用户项目目录的独立 workspace，演示引擎已有的实时路径追踪管线。
项目代码只依赖 `prism-app` 和 `prism-engine`，不直接访问 Vulkan、winit 或 ECS 内部实现。

## 运行

Sponza 资源需要先导入并构建到运行目录的资源包（根目录 `assets/scenes.json` 的 `sponza` 条目）：

```powershell
cargo run --manifest-path projects/sponza/Cargo.toml
```

引擎默认加载 manifest 的**第一个**条目，而根 `assets/scenes.json` 里排第一的是 `intro`（开场动画），
不是 Sponza。要直接进 Sponza，用启动配置指定场景名（`scenes[].name`）：

```powershell
$env:PRISMREV_LAUNCH_CONFIG = '{"scene":"sponza"}'
cargo run --manifest-path projects/sponza/Cargo.toml
```

不设置时回退到第一个条目；`scene` 写错名字不会回退，引擎只记一条警告并保持空场景。

路径追踪由 `build_app()` 的 `RenderSettings::render_mode = PathTrace` 自动启用；需要支持
`VK_KHR_ray_query`、加速结构和相应 bindless 特性的 Vulkan 设备。能力不足时由引擎能力层降级或报告初始化错误。
