# Slang 工具链 + Bindless 迁移指南

本文档记录 PrismaRev 引入 **Slang shader 工具链** 与 **Bindless（descriptor-indexing）纹理表** 的过程，以及现在维护它们要用到的命令和文件位置。

> 背景：最初改动在 Termux/aarch64 上开发，那里 `cargo check`/`cargo test -p prism-render` 全绿，但官方 `slangc` 是 glibc 二进制、无法在 Termux 原生运行。因此 shader 编译（`.slang -> .spv` + 反射 JSON）在桌面/CI 完成；`.spv` 与生成的 Rust 绑定 checked-in 后手机端照常构建。

---

## 一、Slang 工具链

### 文件布局
```
assets/shaders/slang/*.slang              # 每个模块一个文件（common.slang 提供共享类型）
assets/shaders/compile.sh                 # slangc 编译脚本 (.spv + reflection JSON)
assets/shaders/reflection/*.json          # slangc 产出的反射 JSON（checked-in，驱动 codegen）
assets/shaders/*.spv                      # slangc 产物（checked-in，include_bytes! 进二进制）
crates/xtask/                             # 反射 JSON -> Rust 绑定 codegen（独立 workspace）
crates/prism-render/src/shader_bindings/   # @generated：每个 shader 模块一个 .rs + mod.rs
```

迁移完成后原来的 GLSL 源（`*.vert` / `*.frag`）已删除，`assets/shaders/` 下只剩它们的 `.spv`。

### 绑定约定（`[[vk::binding(binding, set)]]`）

各 set 的分配以 `.slang` 源码为准（Rust 侧的 `shader_bindings` 由同一份反射生成，不会漂）：

| set | 资源 |
|-----|------|
| 0 | frame UBO / materials / lights（skybox 复用这里的 IBL `envCube`） |
| 1 | bindless 纹理表（合并 sampler + SRV 数组） |
| 2 | IBL（env / irradiance / prefiltered） |
| 3 | shadow map |

Push constant 结构**不手写**：全部来自 `shader_bindings::<module>::*`（`#[repr(C)]`，字段顺序与 Slang 反射一致）。由于 std140 尾部隐式 padding 不一定出现在反射里，`VkPushConstantRange` 的 `size` 有需要时显式写死（例如 144），不要依赖 `size_of`。

> ⚠️ combined image sampler：Slang 用 `Sampler2D`/`SamplerCube`（不是分离的 `Texture2D + SamplerState`），以匹配 Rust 侧的 `COMBINED_IMAGE_SAMPLER` 描述符和原 GLSL。

### 桌面/CI 收尾步骤
```bash
# 1) 装 slang 2026.21+（对应你的平台）：
#    https://github.com/shader-slang/slang/releases
#    也可以解压到 tools/slang-dl/，compile.sh 会自动用它。
#    Termux/aarch64 无法用官方 glibc 版；请在桌面/CI 做这一步。

# 2) 编译 shader -> .spv + 反射 JSON
#    需要 slangc 2026.21 或更新（已提交的反射就是这个版本生成的）；
#    更旧的版本会被脚本拒绝，因为它会静默丢掉 "version" / "combined" / "sizes" 等字段。
#    （SLANGC 可覆盖；否则依次找 tools/slang-dl/bin/slangc 和 PATH）
bash assets/shaders/compile.sh
#   产出 assets/shaders/*.spv；反射 JSON 先写到临时目录，再和已提交的逐字节比对。
#   不一致时脚本失败并列出行（默认不覆盖仓库里的文件）。确认 schema 变更是有意的，再用
#   bash assets/shaders/compile.sh --write-reflection
#   采用新文件。

# 3) 从真实反射重新生成 Rust 绑定（输出目录会被整份重写：每模块一个 .rs + mod.rs）
cd crates/xtask
cargo run --bin shader-bindgen -- ../../assets/shaders/reflection ../../crates/prism-render/src/shader_bindings
cd ../..

# 4) 校验
cargo test -p prism-render
```

> `assets/shaders/reflection/*.json` 是**真实 slangc 产出**并 checked-in：没有 slangc 的机器（Termux / Android 构建机）也能跑 codegen，桌面/CI 重新编一遍由 drift guard 比对（见文末 CI 一节），手改反射 JSON 会在那里挂掉。
>
> 生成它们的是 slangc `2026.21`（CI 的 `SLANG_VERSION` 与 `compile.sh` 的 `SLANGC_MIN_VERSION` 都钉在它上面；自己的机器版本落后时按脚本提示自行安装，不要下调检查）。`compile.sh` 会检查 `slangc -version`，并在反射与仓库不一致时失败——所以本地那次「只在 /tmp 生成 `.spv`」的绕法不再需要，也不该再手工改 `-reflection-json` 的目标。
>
> 同理，`assets/shaders/*.spv` 也 checked-in（确定性产物，约 460 KB）：**新 worktree 或新克隆不需要 slangc 就能构建**。改了 `.slang` 要重新跑 `bash assets/shaders/compile.sh` 并把新的 `.spv` 一起提交；CI 的 shaders job 重编后比对 `assets/shaders/`，漂移即失败。

### xtask 为什么是独立 crate（不是 build.rs）
- `build.rs` 每次 `cargo build` 都跑 → 如果它自己调 slangc 就会强制依赖 slangc → **手机端构建直接崩**。
  （`crates/prism-render/build.rs` 只做检查 + 「有 slangc 才顺手编译」的兜底，缺 slangc 时只发提示。）
- xtask 在 workspace `exclude` 列表里，默认 `cargo check`/`build` **完全不碰它**，生成的 `shader_bindings/` checked-in。
- 运行方式：`cd crates/xtask && cargo run --bin shader-bindgen -- <反射目录> <输出目录>`（不能用 `cargo run -p xtask`，因为它被 exclude 了）。

---

## 二、Bindless（descriptor-indexing）

### 设计
`crates/prism-render/src/bindless.rs` 里的 `BindlessTextureTable`：一个大的描述符集，含一个**运行时大小的 `COMBINED_IMAGE_SAMPLER` 数组**。Shader 用 `u32` handle 索引，材质/纹理切换不再需要重绑描述符集。

Flags：`PARTIALLY_BOUND | UPDATE_AFTER_BIND | VARIABLE_DESCRIPTOR_COUNT`，配合 `runtime_descriptor_array` + `shader_sampled_image_array_non_uniform_indexing`。

### 现状
- `context.rs`：`rt_caps.descriptor_indexing` 为真时开启 5 个 descriptor-indexing 子特性（runtime array / partially bound / update-after-bind / variable count / non-uniform indexing）。
- 纹理表由 `RenderTextureManager` 持有（`bindless()` / `bindless_mut()`），`ForwardPass` 只借用它的 set + layout（set 1）；IBL cubemap 与 BRDF LUT 在启动/场景切换时注册进去换成 handle。
- **PBR 已经走 bindless**：`scene_frag.slang` 从 set 1 的运行时数组采样，push constant 用生成的 `PbrBindlessPushConstants`（96 字节，多一个 `env_handle: u32`），采样走 `NonUniformResourceIndex`。
- 老的独立描述符集（`descriptor.rs`、`ibl.rs` 的 cubemap set、`external_overlay.rs`）**全部保留可用**：它们是给不支持 descriptor-indexing 的设备和其他 pass 用的，不是遗留待删代码。
- 落地顺序是「先加表、再切 pipeline」的增量改造——当时 renderer 体量大，在没有可视化验证的环境里整体重写绑定风险太高。

### 移动端注意
- 并非所有 Android GPU 都支持全套 descriptor-indexing。`context.rs` 只在 `descriptor_indexing` cap 为真时开启；设备缺 `UPDATE_AFTER_BIND` 一类子特性时应回退到现有独立描述符集路径（老路径已保留，天然可 fallback），真机务必验证。
- `UPDATE_AFTER_BIND` 在部分移动驱动上支持较弱。

---

## 三、代码生成与资源预算规范

### 1. Push Constant 内存布局与对齐填充

- **强类型推导**：所有 push-constant 结构体定义必须由 `xtask shader-bindgen` 读取 `assets/shaders/reflection/*.json` 自动生成，严禁手写 `#[repr(C)]` 镜像。
- **std140 对齐洞显式占位**：代码生成器强制将每个字段定位在反射记录的 `offset` 上。std140 规范产生的内存空洞被显式生成为 `_pad_<offset>: [u8; N]` 字段，从而确保 Rust 的 `size_of` 及各字段 offset 与着色器端严格逐字节匹配。
- **类型安全转换**：生成的结构体自动 derive `bytemuck::Pod` 与 `bytemuck::Zeroable`，上传时使用 `bytemuck::bytes_of(&push)`，严禁裸指针与未安全检查的切片强转。
- **代码生成器实现原则**：`xtask shader_bindgen` 使用标准的 `std::fmt` 字符串格式化而非 `syn` / `quote`，以避免向 xtask 依赖树引入重量级 proc-macro 依赖，极大加快工具链构建速度。

### 2. 着色器资源预算与棘轮监控（budgets.json）

- `assets/shaders/budgets.json` 记录并锁定每个着色器模块的资源上限（采样器、采样纹理数组、UBO、SSBO、加速结构、描述符总数及 push constant 字节数）。
- `crates/xtask/src/shader_budget.rs`（`shader-budget`）汇总实际反射用量并对比预算表。改动着色器若超出预算即触发构建与 PR 失败。
- 预算表具有棘轮属性（Ratchet）：仅在架构决策确认需要扩展资源时，通过 `cargo run --bin shader-budget -- --write-budget` 同步更新，并在 PR 审查中显式体现配额变化。

---

## 验证状态（迁移当时，Termux/aarch64 快照）
- `cargo check -p prism-render` ✅
- `cargo test -p prism-render --lib` ✅ 25 passed
- `xtask` 端到端生成绑定 ✅
- 当时的 `prism-android` crate 在非 Android target 上编不过（android-activity 需要 NDK target），与本次改动无关。该 crate 后来拆成 `prism-app` + 用户项目 `projects/template`，同样的限制仍然成立：`prism-app` 不进 desktop CI job。

## CI（.github/workflows/ci.yml）
4 个 job：
1. **changes** — doc-only 快速通道：diff 只落在 `docs/`、`*.md`、`LICENSE*`、`.gitignore` 时其余 job 跳过（跳过的 job 仍报 Success，保持 required checks 可合并）。
2. **lint** — `cargo fmt --all -- --check`；desktop crates 的 `clippy --all-targets -- -D warnings`（`prism-ecs/render/engine/platform`）；`xtask` 因为被 workspace exclude，单独 fmt + clippy。
3. **desktop** — `cargo build/test --locked -p prism-ecs -p prism-render -p prism-engine -p prism-platform`。`prism-app` 需要 NDK target，因此不在这个 job 里。
4. **shaders** — slangc 编 `.spv` + 反射，`shader-bindgen` 从新反射重生成绑定，**drift guard**（committed 绑定与真实反射不符则 fail），并把 `spirv` / `slang-reflection-real` 作为 artifact 上传。

> 没有 Android job：APK/`.so` 由桌面上的 `scripts/build-android.ps1` 产出（见 `AGENTS.md` 的「Android packaging chain」）。
> `.spv` 是 checked-in 且被 `include_bytes!` 编进 Rust 的，所以 shaders job 的 drift guard 是硬保证——谁改了 `.slang` 忘了重编并提交 `.spv`，CI 直接挂。

## 资产
`assets/fbx/`：`coord.fbm/`、`coord.fbx`、`cornell-box.fbx`、`cube-coord.fbx`。
`sponza.zip`(110MB)/`sponza-1.zip`(99MB) 因 GitHub 大文件限制未拷；需要时在桌面/CI 用 `just fetch-res` 等价流程拉取（见 TruvisRenderer `resources.toml`）。
