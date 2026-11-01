# 打 game-only Android APK（无 launcher / 无 Tauri）

把 `projects/prisma` 打成只有一个 `GameActivity` 壳 + `libprisma.so` + 场景包的最小 APK：
没有 WebView、没有启动器 UI、不需要 Kotlin 编译器。适用于任何主机（x86_64 最常见）。

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

bash scripts/build-android-game.sh debug       # 日常调试（debug 证书签名，可直接装）
bash scripts/build-android-game.sh release     # 未签名 release
bash scripts/build-android-game.sh debug --skip-cargo   # .so 已就绪，只重打包
```

脚本流程：`cargo build --lib --target aarch64-linux-android` → `build-scenes` →
复制 `.so` 与游戏资产 → `gradlew clean assemble<变体>`。

- API level 取 `max(minSdk, 26)`：`libaaudio.so` 从 API 26 才有，低于它会报 `-laaudio not found`。
- 打包前**一定 clean**：换过 `.so` 后增量打包会在 zip 里留下空洞（实测 21 MB 产物变 217 MB）。
- 复制的是 `assets/` 下的**全部** `.pak`（运行时按扩展名挂载），脚本不再写死
  `scenes.pak`；一个都没有时直接失败。`build-scenes` 也会在场景引用的资源缺失时
  非零退出——先补齐资产（例如 `python scripts/gen_placeholder_sprites.py`）再打包。

## 3. 产物

```
projects/prisma/android/app/build/outputs/apk/<变体>/*.apk
  package com.prisma, minSdk 31, targetSdk 36, arm64-v8a only
  lib/arm64-v8a/libprisma.so    ~12 MB（release）/ ~200 MB（debug，带调试符号）
  classes*.dex                  ~6 MB（GameActivity + appcompat）
  assets/game/                  assets/*.pak + *.pak.meta.json + settings.ini + HDR
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
projects/prisma/android/          game-only Android 宿主（独立 Gradle 工程，不属任何 cargo workspace）
  app/src/main/AndroidManifest.xml     GameActivity + android.app.lib_name=prisma
  app/src/main/java/com/prisma/MainActivity.java   3 行，extends GameActivity
  app/src/main/res/values/themes.xml   AppCompat 主题（GameActivity 继承 AppCompatActivity）
  app/src/main/jniLibs/                生成物：libprisma.so（gitignore）
  app/src/main/assets/game/            生成物：场景包与设置（gitignore）
scripts/build-android-game.sh     一条命令打包
```

引擎侧零改动：`prism_app::run_on_android` 把 APK 的 `assets/game/` 解到
`<filesDir>/assets`、切 cwd，再调 `projects/prisma/src/lib.rs` 的 `android_main`。

**和 `launcher/` 的关系**：`launcher/src-tauri/gen/android` 打的是「Tauri 启动器 + 游戏」
双 Activity APK（`scripts/build-android.ps1`），与本路径互不影响；需要启动器 UI 时用那条，
只想要游戏本体时用这条。

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
