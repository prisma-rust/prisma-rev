# 打 game-only Android APK（无 launcher / 无 Tauri）

把任意用户工程打成只有一个 `GameActivity` 壳 + `lib<工程>.so` + 场景包的最小 APK：
没有 WebView、没有启动器 UI、不需要 Kotlin 编译器。适用于任何主机（x86_64 最常见）。

脚本第一个参数是**工程名**（`projects/` 下的目录名），默认 `prisma`：

```bash
bash scripts/build-android-game.sh                # projects/prisma
bash scripts/build-android-game.sh fishing        # projects/fishing
bash scripts/build-android-game.sh fishing release
```

新工程第一次打包前先跑 `scripts/new-android-game.sh` 生成宿主（见 §7）。

> **主机是 aarch64（Asahi / 树莓派 / ARM 服务器 / Termux）？**
> 先按本文装好 JDK/SDK/NDK，再补 [android-arm64.md](android-arm64.md) 里的工具链替换步骤
> ——那部分是少数情况，大多数人不需要。

## 1. 前置

| 依赖 | 说明 |
|---|---|
| JDK 21 | AGP 8.11 要求 17+ |
| Android SDK | `platforms;android-36` + `build-tools;36.0.0` |
| NDK | r29（`ANDROID_NDK_HOME`），官方 x86_64 主机版即可 |
| `.spv` | 不入库，先在有 slangc 的机器上编，或从 CI 的 `spirv` artifact 下载（[build-guide-termux.md §3](build-guide-termux.md)） |

```bash
# x86_64 Linux 示例
sudo apt-get install -y openjdk-21-jdk-headless cmake ninja-build unzip

export ANDROID_HOME=$HOME/android-sdk
mkdir -p "$ANDROID_HOME/cmdline-tools"
curl -L -o /tmp/cmdline.zip \
  https://dl.google.com/android/repository/commandlinetools-linux-14742923_latest.zip
unzip -q /tmp/cmdline.zip -d /tmp/cmdtmp
mv /tmp/cmdtmp/cmdline-tools "$ANDROID_HOME/cmdline-tools/latest"

yes | "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" --licenses
"$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" \
  "platforms;android-36" "build-tools;36.0.0"

curl -L -o /tmp/ndk.zip https://dl.google.com/android/repository/android-ndk-r29-linux.zip
unzip -q /tmp/ndk.zip -d "$ANDROID_HOME/ndk-tmp"        # 约 2.4 GB
mkdir -p "$ANDROID_HOME/ndk"
mv "$ANDROID_HOME/ndk-tmp/android-ndk-r29" "$ANDROID_HOME/ndk/29.0.14206865"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/29.0.14206865"
```

## 2. 一条命令

```bash
export ANDROID_HOME=$HOME/android-sdk
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/29.0.14206865

bash scripts/build-android-game.sh                     # prisma，debug（可直接装）
bash scripts/build-android-game.sh fishing             # fishing，debug
bash scripts/build-android-game.sh fishing release     # 未签名 release
bash scripts/build-android-game.sh fishing debug --skip-cargo   # .so 已就绪，只重打包
```

脚本流程（全部针对 `<project>` 自己）：`cargo build --lib --target aarch64-linux-android`
→ `build-scenes --manifest <project>/assets/scenes.json --output <project>/assets/scenes.pak`
→ 复制 `.so` 与游戏资产到 `<project>/android/app/src/main/` → `gradlew clean assemble<变体>`。

- 库名（`.so` 与 `android.app.lib_name`）从 `<project>/assets/settings.ini` 的
  `[build] lib_name`、否则 `Cargo.toml` 的 `[lib] name` / `[package] name` 读出来
  （`-` → `_`），脚本不再按目录名猜；manifest 里的声明与它不一致时构建期直接失败。
- 工程没有 `android/` 时脚本会提示先用 `scripts/new-android-game.sh <project>` 生成。

- API level 取 `max(minSdk, 26)`：`libaaudio.so` 从 API 26 才有，低于它会报 `-laaudio not found`。
- 打包前**一定 clean**：换过 `.so` 后增量打包会在 zip 里留下空洞（实测 21 MB 产物变 217 MB）。
- 复制的是 `assets/` 下的**全部** `.pak`（运行时按扩展名挂载），脚本不再写死
  `scenes.pak`；一个都没有时直接失败。`build-scenes` 也会在场景引用的资源缺失时
  非零退出——先补齐资产（例如 `python scripts/gen_placeholder_sprites.py`）再打包。

## 3. 产物

```
<project>/android/app/build/outputs/apk/<变体>/*.apk
  package com.prisma（prisma）/ com.prisma.fishing（fishing）, minSdk 31, targetSdk 36, arm64-v8a only
  lib/arm64-v8a/lib<project>.so  ~12 MB（release）/ ~200 MB（debug，带调试符号）
  classes*.dex                   ~6 MB（GameActivity + appcompat）
  assets/game/                   <project>/assets/*.pak + *.pak.meta.json + settings.ini + HDR
```

安装：`adb install -r app-debug.apk`，图标即启动游戏。

## 4. 发布签名

`release` 产物未签名（`app-release-unsigned.apk`），签名材料**不入库**：

```bash
$ANDROID_HOME/build-tools/36.0.0/apksigner sign \
  --ks ~/prisma-release.jks --ks-key-alias prisma \
  --out prisma-release.apk app-release-unsigned.apk
```

## 5. 目录与职责

```
templates/android-game/           宿主模板（#598）：scripts/new-android-game.sh 从这里展开
  app/build.gradle.kts                minSdk 31 / targetSdk 36 / arm64-v8a / 签名注释
  app/src/main/AndroidManifest.xml    GameActivity + android.app.lib_name=__GAME_NAME__
  app/src/main/java/MainActivity.java 3 行，extends GameActivity（按包名放目录）
  app/src/main/res/values/themes.xml  AppCompat 主题（GameActivity 继承 AppCompatActivity）
  gradle/wrapper/                     wrapper jar + properties（Gradle 版本只有这一处定义）
projects/<p>/android/             game-only Android 宿主（独立 Gradle 工程，不属任何 cargo workspace）
  app/src/main/jniLibs/                生成物：lib<p>.so（gitignore）
  app/src/main/assets/game/            生成物：场景包与设置（gitignore）
scripts/new-android-game.sh       从模板生成 projects/<p>/android/
scripts/lib/project-info.sh       工程目录 / 库名的唯一解析处（两个脚本共用）
scripts/build-android-game.sh     一条命令打包任意工程
```

引擎侧零改动：`prism_app::run_on_android` 把 APK 的 `assets/game/` 解到
`<filesDir>/assets`、切 cwd，再调 `projects/prisma/src/lib.rs` 的 `android_main`。

**和 `launcher/` 的关系**：`launcher/src-tauri/gen/android` 打的是「Tauri 启动器 + 游戏」
双 Activity APK（`scripts/build-android.ps1`），与本路径互不影响；需要启动器 UI 时用那条，
只想要游戏本体时用这条。launcher 的 manifest 固定 `android.app.lib_name = prisma`，所以
`build-android.ps1 -Project fishing` 会转而调用 game-only 脚本（并转发 `-Mode`）。

## 6. 通用坑

| 症状 | 原因 | 解决 |
|---|---|---|
| `GameActivity` 编译报找不到 `AppCompatActivity` | games-activity 4.4.0 继承 `AppCompatActivity` | 加 `androidx.appcompat` + AppCompat 主题 |
| `找不到 style/Theme.AppCompat.NoActionBar.Fullscreen` | 该主题不存在 | 自定义主题 `parent="Theme.AppCompat.NoActionBar"` + `android:windowFullscreen` |
| `Duplicate class kotlin.collections.jdk8.*` | appcompat 带进 kotlin-stdlib-jdk7/jdk8 1.6.21 | `implementation(platform("org.jetbrains.kotlin:kotlin-bom:1.9.25"))` |
| APK 突然多出 ~200 MB | 换 `.so` 后增量打包留空洞 | 打包前 `clean`（脚本已默认 clean） |
| `-laaudio not found` | API level < 26 | API level 取 `max(minSdk, 26)` |
| 编译报 `include_bytes!` 找不到 `.spv` | `.spv` 不入库 | 桌面机编一次，或从 CI `spirv` artifact 下载 |
| `aapt2: cannot execute binary` / `Exec format error` | 主机不是 x86_64 | 见 [android-arm64.md](android-arm64.md) |

## 7. 新工程接 Android（#598）

```bash
bash scripts/new-android-game.sh fishing
#   --package com.example.fishing   默认 com.prisma.<工程名>
#   --label "Cozy Fishing"          桌面图标名，默认工程名
#   --force                         覆盖已有的 projects/fishing/android

bash scripts/build-android-game.sh fishing          # debug APK
bash scripts/build-android-game.sh fishing release  # 未签名 release
```

要点（模板已经处理好，手工 fork 时最容易漏的就是这几条）：

- **`minSdk` 不低于 26**：`libaaudio.so` 从 API 26 才有，低于它会 `-laaudio not found`。
  模板默认 31，与仓库现有工程一致。
- **`abiFilters` 只留 `arm64-v8a`**：引擎的 Android 支持面就是这一档。
- **`android.app.lib_name` 必须等于库名**（`__GAME_NAME__` → `fishing`）：它决定
  `System.loadLibrary` 找哪个 `.so`；脚本在构建前会校验，不一致直接失败。
- **`packaging.jniLibs.keepDebugSymbols`**：debug `.so` 带符号，缺 `llvm-strip` 时不要整包失败。
- **不要用 `tauri android init` 或其它脚手架覆盖 `projects/<p>/android/`**：模板里的
  定制（minSdk / abiFilters / 签名注释 / 主题）会被模板版覆盖掉。
- 打包脚本按**工程名**解析，不再写死 `projects/prisma`；场景包、`.pak`、settings.ini、
  HDR 都从 `<project>/assets/` 读，`build-scenes` 也写回该工程自己的 `assets/`。
- 美术资产是 gitignore 的工程要先生成（例如 `python scripts/gen_placeholder_sprites.py`），
  否则 `build-scenes` 会因为缺引用而失败——这是有意的（#530）。
