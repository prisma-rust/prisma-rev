# 代码注释与文档注释规范

适用于 `prisma-rev` 仓库里的**代码注释**：`.rs` 的 `//` / `///` / `//!` / `/* */`，
以及 `Cargo.toml`、CI YAML、shell / Python / PowerShell、`.slang` 里的注释。

**目标只有两条：注释用英文；删掉不提供信息的注释。**
（本文档本身、`docs/**/*.md`、`README.md`、`AGENTS.md` 继续用中文，不在此规范内。）

## 1. 语言边界

| 内容 | 语言 |
|---|---|
| Rust 代码注释、rustdoc | 英文 |
| `Cargo.toml` / CI / 构建脚本 / shader 注释 | 英文 |
| `docs/**/*.md`、`README.md`、`AGENTS.md`、`CONTRIBUTING.md` | 中文 |
| UI 文案、日志、错误提示、测试断言的用户可见文本 | 保持原文（通常是中文） |
| 标识符、类型名、命令、路径、Vulkan 枚举、格式名 | 不翻译 |

技术术语沿用仓库既有写法：`swapchain`、`bindless`、`render graph`、`bind group`、
`descriptor set`、`push constant`、`RSCN`、`RTTIBin` 等，不要另造中文译名。

### 为什么强调这一条

历史上有一个提交（`803c0068`）用脚本把英文注释**逐词替换**成中文，留下了大量病句：

```rust
/// Raw 索引 into the 实体 allocator. 稳定 for the entity's 生命周期
/// Monotonically increasing version; bumped each 时间 the 槽 is recycled.
```

这种文本比英文原文更难读，也更难维护。恢复时优先查该提交之前的 git 历史，
再按当前代码重写；**不要再用替换脚本生成最终注释**。

## 2. 保留什么

只保留“读代码看不出来”的信息：

- 模块级 `//!`：这个模块负责什么、边界在哪、和谁共享约定。
- 公开 item 的 `///`：语义、单位、不变量、何时 panic / 返回错误、调用方必须满足的条件。
- 非显然的算法、数学、坐标约定（尤其 `AGENTS.md` 的矩阵与坐标系约定）。
- Vulkan 规范约束、VUID、驱动 workaround、布局转换、同步与生命周期不变量。
- 二进制格式布局（magic / version / 字段顺序）；注意 1.0 前不维护旧版本二进制兼容，直接升级格式并移除旧逻辑。
- 线程模型、所有权、锁顺序、channel 语义。
- 性能取舍、feature 取舍的原因（例如“为什么这个依赖必须是 optional”）。
- 真正不显然的 `unsafe` 前置条件，用 `// SAFETY:` 说明。

## 3. 删除什么

- 复述代码的注释（`// increment i`、`// return the value`）。
- 装饰性分隔线（`// ----------`、`// ==========`、`// Entity` 这类纯标题）。
- 被注释掉的旧代码。
- 1.0 版本发布前（当前 0.1）为了兼容旧版本而保留的死代码、兼容门面、旧分支回退与垫片（旧版本兼容被明确视为冗余垃圾代码）。
- 过期状态 / 里程碑 / “目前”“暂时”“以后再说”而没有 issue 链接的说明。
- 同一事实在模块文档、类型文档、字段文档里重复多遍的内容。
- 没有 issue 或明确原因的 `TODO` / `FIXME` / `XXX` / `HACK`。
- 只对作者有意义的缩写、代号、内部梗。

长文件需要“章节感”时，优先拆模块或拆函数，而不是加分隔线。

## 4. rustdoc 写法

公开 item 使用 `///`，第一行是摘要，空一行再写细节：

```rust
/// Returns the entity's generation.
///
/// The generation distinguishes a live handle from a stale one after the
/// slot is recycled. It is not a global version counter.
pub fn generation(self) -> u32 {
```

按需使用标准段落：

```rust
/// Decodes an RSCN scene from `bytes`.
///
/// # Errors
///
/// Returns [`SceneError::UnsupportedVersion`] when the header version is newer
/// than this build supports.
///
/// # Panics
///
/// Panics when `bytes` is shorter than the fixed header.
```

- 第一行用完整句子，句末加句号。
- 标识符、路径、命令用反引号；Rust item 用 rustdoc 链接（`` [`World`] ``）。
- 不写日期、作者、变更历史；这些属于 git / CHANGELOG。
- 单行不超过 100 字符，与 rustfmt 行宽一致。
- 文档里的代码块必须能编译（doctest）或明确标成 `text`。

## 5. `SAFETY`、`TODO` 与格式

### `SAFETY`

`unsafe` 块只在前置条件不显然时写 `SAFETY`，并紧贴代码：

```rust
// SAFETY: `image` was created from `device` and is still alive; the caller
// holds `&mut self`, so no other submission can destroy it concurrently.
unsafe { device.destroy_image(image, None) };
```

不要给每个 `unsafe` 机械补一句“调用方保证安全”；写清楚**具体**不变量。

### `TODO` / `FIXME`

必须带 issue 或链接：

```rust
// TODO(#123): replace the linear scan once the asset index lands.
```

没有 issue 就删掉；不要把待办留在注释里当隐形需求。

### 分隔线

不要使用：

```rust
// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------
```

直接用模块 / 函数 / 类型组织代码；确实需要导航时，用 `//!` 模块文档里的列表。

## 6. 示例

保留（解释了为什么）：

```rust
// The framebuffers must be destroyed before the swapchain is recreated,
// otherwise validation reports VUID-vkDestroyFramebuffer-... and the device
// can be lost on some drivers.
```

删除（只是复述代码）：

```rust
// Set the width.
self.width = width;
```

删除（过期里程碑）：

```rust
// This is a skeleton for milestone 1; later milestones can slot in RenderSystem.
```

需要保留时改写成英文：

```rust
// Before
/// Raw 索引 into the 实体 allocator. 稳定 for the entity's 生命周期

// After
/// Raw index into the entity allocator. Stable for the entity's lifetime.
```

## 7. 只改注释的 PR

注释清理必须与逻辑改动分开。每个注释 PR 跑：

```bash
python scripts/check_comment_only.py --base origin/master
```

它用同一套 Rust 词法扫描去掉注释和空白，比较剩下的代码 token。任何代码改动都会失败；
纯注释修改（包括删除注释）通过。不要在一个 PR 里顺手改代码、重排 import 或跑全仓 rustfmt。

如果注释修复必须伴随代码改动（例如坏链接指向的 item 已经不存在），把代码改动拆到
另一个 PR，或在 PR 里明确说明并请 reviewer 重点看那一处。

## 8. 工具

```bash
# 全仓检查（迁移期会输出存量，用于统计）
python scripts/check_comments.py --all

# 只检查相对 base 的改动
python scripts/check_comments.py --changed --base origin/master

# 只检查暂存区；输出可直接贴 PR 的 Markdown 行
python scripts/check_comments.py --staged --markdown

# 某个文件 / 目录
python scripts/check_comments.py crates/prism-ecs/src/lib.rs
```

`check_comments.py` 会标记：

- 注释中的 CJK 字符；
- 装饰性分隔线；
- 没有 issue 的 `TODO` / `FIXME` / `XXX` / `HACK`；
- `lib.rs` / `main.rs` / `mod.rs` 缺少 `//!` 模块文档。

字符串里的中文（UI 文案、错误信息）不会被标记；生成文件
（`crates/prism-render/src/shader_bindings/`、`launcher/src-tauri/gen/`、
`tools/slang/`）会被跳过。

## 9. 迁移与门禁

- 存量中文注释按 crate / 模块分批清理，先核心 crate，后渲染、引擎、编辑器。
- 迁移期 CI 可以只对**改动文件**告警；存量清零后，`check_comments.py --all` 变成硬门禁。
- 每个注释 PR 额外跑 `RUSTDOCFLAGS="-D warnings" cargo doc -p <crate> --no-deps`，
  确保 rustdoc 链接和代码块没有坏掉。
- 生成的文档注释不要手改：`shader_bindings/`、Tauri `gen/` 等以生成器为准。

## 10. Review checklist

- [ ] 注释是英文，标识符 / 路径保持原样。
- [ ] 没有复述代码、装饰线、被注释掉的旧代码、过期里程碑。
- [ ] 非显然的不变量、单位、panic / error / safety 条件仍然保留。
- [ ] `TODO` 带 issue；没有 issue 的已删除。
- [ ] 模块文件有 `//!`；公开 item 有必要的 `///`。
- [ ] 注释-only PR 通过 `check_comment_only.py`。
- [ ] `cargo doc -D warnings` 对改动的 crate 通过。
