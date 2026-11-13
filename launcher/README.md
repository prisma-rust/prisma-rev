# PrismaRev Android 启动壳

APK 里的「开始游戏」页 + 打包配置。这个 Tauri 工程只做一件事：拉起游戏。

- **Android**：`launch_game` 通过 Kotlin plugin 启动 `com.prisma.MainActivity`
  （`GameActivity`，`android.app.lib_name = template_core` → `libtemplate_core.so`）。
- **桌面**：`launch_game` 拉起本地游戏二进制，方便不打包也能验证启动页与
  `PRISMREV_LAUNCH_CONFIG` 参数传递。

APK 里有两个 Activity（两个进程）：

| Activity | 作用 |
|---|---|
| `com.example.tauriandroidapp.MainActivity` | 这个 Tauri 启动壳（本目录） |
| `com.prisma.MainActivity` | `GameActivity`，加载 `libtemplate_core.so` |

## 打包

```powershell
scripts/build-android.ps1     # Windows
bash launcher/build_release.sh  # Git Bash / WSL / Linux
```

脚本会编译游戏 cdylib、构建场景包，并把产物放进
`launcher/src-tauri/gen/android/app/src/main/assets/game/`，最后交给 Gradle 组装 APK。
`gen/android/` **带本地定制**（minSdk 31、arm64-v8a、签名配置、第二个 Activity），
不要用 `tauri android init` 重新生成。

## 开发

```sh
cd launcher
pnpm install
pnpm tauri dev      # 桌面：显示启动页，点击后拉起本地游戏二进制
```

前端检查：

```sh
cd launcher
pnpm build
```

后端检查 / 测试（独立 workspace，不挂根 PrismaRev workspace）：

```sh
cd launcher/src-tauri
cargo test
```

游戏二进制定位不查 `PATH`：`PRISMREV_GAME_BIN` → 启动壳同目录的 `template[.exe]` →
`projects/template/target/{debug,release}/template[.exe]`。
