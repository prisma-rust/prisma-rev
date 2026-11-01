# PrismaRev 从拉取到编译（Termux / Android 实测流程）

适用于**没有 slangc** 的主机（Termux/Android）。桌面机流程更简单：装 slangc 后
`bash assets/shaders/compile.sh` 再 `cargo build` 即可，跳过第 3 节的 CI 步骤。

## 1. 环境准备

```bash
pkg install rust git gh        # Termux；rust 需 ≥ 1.85（is_multiple_of 等新 API）
gh auth login                  # GitHub CLI 认证（HTTPS token 即可）
```

git 网络坑（Termux IPv6 到 github.com 会超时，IPv4 正常）：

```bash
git config --global http.version HTTP/1.1
```

可选（Android .so 目标，见第 5 节）：

```bash
rustup target add aarch64-linux-android
pkg install cargo-ndk          # 或 cargo install cargo-ndk
```

## 2. 拉取

```bash
git clone https://github.com/Excurs1ons/PrismaRev.git
cd PrismaRev
```

注意：`assets/shaders/*.spv` 是 gitignored 的（不入库），克隆下来**没有** `.spv`，
但 `prism-render` 用 `include_bytes!` 引用它们——直接 `cargo build` 会失败，
必须先走第 3 节生成/获取 `.spv`。

## 3. Shader 流程（无 slangc 主机）

### 3a. 首次：从 CI 取预编译产物

```bash
# 触发一次 CI（推送任意提交，或手动跑 workflow）
git push   # 或 gh workflow run ci.yml

# 等 CI 的 "slang compile" job 完成（约 2-3 分钟）
gh run list --limit 1           # 拿到 run id
gh run watch <run_id>           # 等绿
gh run download <run_id> -n spirv -D ~/spirv

# 部署到项目
cp ~/spirv/*.spv assets/shaders/
cp ~/spirv/reflection/*.json assets/shaders/reflection/
```

> `/tmp` 在 Termux 不可写，下载目录用 `~/` 下路径。
> `spirv` artifact 每次 CI 都会上传（含 .spv + reflection JSON）。

### 3b. 生成 Rust 绑定（bindgen）

```bash
cd crates/xtask
cargo run --bin shader-bindgen -- \
  ../../assets/shaders/reflection ../../crates/prism-render/src/shader_bindings
cd ../..
```

规则：**push constant 结构体一律用生成的 `shader_bindings::模块::Struct`**，
禁止手写。改过 `.slang` 后必须重跑此命令并提交 binding 差异
（CI 的 binding drift job 会校验：不重跑就失败）。

### 3c. 改了 shader 后的更新流程

```bash
git add -A && git commit -m "..." && git push   # 触发 CI 编译新 shader
gh run download <新run_id> -n spirv -D ~/spirv
cp ~/spirv/*.spv assets/shaders/
cp ~/spirv/reflection/*.json assets/shaders/reflection/
cd crates/xtask && cargo run --bin shader-bindgen -- ../../assets/shaders/reflection ../../crates/prism-render/src/shader_bindings && cd ../..
cargo check --workspace
```

## 4. 编译 / 检查 / 测试

```bash
cargo check -p prism-render            # 快速验证渲染层
cargo check --workspace                # 全部 crate
cargo clippy --workspace --all-targets # 必须 0 警告（#![deny(warnings)]）
cargo test --workspace                 # ~400 测试（含 CRT 状态机单测，无 GPU 可跑）
```

注意：

- 根 workspace **没有可运行的 bin**。桌面入口在 `projects/prisma/`（`prisma` 二进制，
  独立 workspace：`cd projects/prisma && cargo run`）；编辑器是 `editor/`（Tauri 2 + Vue 3，
  需要 `pnpm tauri dev`）；`launcher/` 是 Android 启动壳（独立 workspace），
  桌面 Hub 在独立仓库 [`Excurs1ons/prisma-hub`](https://github.com/Excurs1ons/prisma-hub)。
- `projects/prisma/`、`projects/prisma-sponza/`、`launcher/`、`editor/`、
  `crates/xtask` 不在根 workspace 的默认覆盖内——
  根 workspace 构建不包含它们，需各自 `cd` 进去单独 build/clippy。
- 首次全量构建较久（依赖多，磁盘约 2GB+）；`target/` 被外部清理后需重新构建。
- 本地 Cargo 镜像（可选，加速）：`~/.cargo/config.toml` 用 rsproxy：
  `sparse+https://rsproxy.cn/index/`。

## 5. Android APK（可选）

两条路，按需选：

- **game-only（无 launcher）**：`projects/<p>/android/` + `scripts/build-android-game.sh <project>`
  （宿主用 `scripts/new-android-game.sh <project>` 从 `templates/android-game/` 生成，#598）。
  打出的 APK 里只有 `GameActivity` + `libprisma.so` + `assets/game/`，不含 Tauri / WebView。
  流程见 [`docs/android-apk.md`](android-apk.md)。
- **launcher + 游戏（双 Activity）**：Tauri 的 `launcher/src-tauri/gen/android/`，
  `scripts/build-android.ps1`（Windows）。Termux 上也可以直接把新 `.so` 替换进已有 APK
  并用 `apksigner` 重签。

> Termux / aarch64 主机额外要换 NDK 与 build-tools 里的 x86_64 原生二进制，
> 见 [`docs/android-arm64.md`](android-arm64.md)（**x86_64 主机不需要**）。

只编 `.so`（不打包）时仍可用 cargo-ndk：

```bash
# -P 必须 >= 26：libaaudio.so 从 API 26 才有，cargo-ndk 默认 21 会报 -laaudio not found。
# 同时不要高于项目的 minSdk（当前为 31），否则会在旧设备上链接到不存在的符号。
cargo ndk -P 26 -t arm64-v8a -o <输出目录> build
```

## 常见坑速查

| 症状 | 原因 | 解决 |
|---|---|---|
| git pull/push 卡死超时 | Termux IPv6 连不上 github.com | `git config --global http.version HTTP/1.1` |
| 编译报 include_bytes 找不到 .spv | .spv 不入库，本地没有 | 走第 3 节 CI 下载 |
| CI binding drift job 红 | 改了 shader 没重跑 bindgen | 3b 重生成并提交 |
| `mem::zeroed` 编译/运行报 invalid_value | ash::Device 含函数指针 | 测试构造用 `MaybeUninit` + `#[allow(invalid_value)]` |
| clippy 报 manual is_multiple_of | Rust 新版 API | 用 `.is_multiple_of(n)` |
| 修改 shader 后"什么都没变" | 跑的是旧 .spv | 重新编译/下载 shader 产物 |
