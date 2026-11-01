# aarch64 主机上的 Android 构建工具链（少数情况）

> **大多数人不需要读本文。** 常规打包流程见 [android-apk.md](android-apk.md)；
> 本文只在**宿主机是 aarch64** 时需要：Asahi / 树莓派 / ARM 服务器 / aarch64 容器。

Google 的 Android SDK / NDK 原生二进制只发 x86_64。aarch64 主机上照 [android-apk.md](android-apk.md)
装完 SDK 和 NDK 之后，还需要本文的三步替换，才能真正编出 APK。

## 1. 官方为什么不行

| 组件 | 官方发布形态 | aarch64 主机上 |
|---|---|---|
| NDK `toolchains/llvm/prebuilt/*` | 只有 `linux-x86_64` / `darwin-x86_64` / `windows-x86_64` | `Exec format error` |
| `aapt2` / `aidl` / `zipalign` / `split-select` | 只有 x86_64 ELF | `Exec format error` |
| Gradle / AGP / D8 / R8 / apksigner | JVM | ✅ 原生可跑 |
| Rust `aarch64-linux-android` | rustup target | ✅ 原生可跑 |

关键点：NDK 里**与主机架构无关**的部分是 `sysroot/`（bionic 头文件 + aarch64 stub 库 +
`crtbegin_so.o`/`crtend_so.o`）和 `lib/clang/<ver>/lib/linux/` 下的目标 runtime
（`libclang_rt.builtins-aarch64-android.a`、`aarch64/libunwind.a`）；换掉 clang 驱动即可。

## 2. 宿主依赖

```bash
# Ubuntu / Debian
sudo apt-get install -y clang lld llvm-21 cmake ninja-build unzip curl
# Termux
pkg install clang lld llvm cmake ninja unzip curl
```

`scripts/make-ndk-host-native.sh` 需要 `clang` 与同版本的 `llvm-strip`（`llvm-<major>` 包）。

## 3. NDK：换成宿主 clang 驱动

```bash
bash scripts/make-ndk-host-native.sh "$ANDROID_NDK_HOME"
```

脚本原地改写 NDK（想恢复就重新解压 NDK zip）：

1. `prebuilt/<host>/bin/*clang*` → shim。shim 从 `argv[0]`（`aarch64-linux-android31-clang`）
   解析 API level，然后调用**宿主 clang** 并补齐 `--target` / `--sysroot` /
   `--rtlib=compiler-rt` 以及 NDK 的 `builtins`、`libunwind` 搜索路径。
2. `llvm-{ar,ranlib,strip,objcopy,nm,readelf,...}` → 宿主 LLVM（同目标架构，能处理 aarch64 ELF），
   这样 AGP 的 strip 任务与 cmake 也能跑。

### 为什么 shim 不用 NDK 自带的 clang 头文件

NDK 的 `lib/clang/<ver>/include`（`arm_neon.h` 等）必须和同一版本 clang 绑定。用宿主 clang 配
NDK 头文件会直接炸：

```
arm_neon.h:46628: error: incompatible constant for this __builtin_neon function
```

所以 shim 只从 NDK 借 **sysroot + 目标 runtime**，头文件用宿主 clang 自己的 resource dir。

## 4. aapt2 等四个 build-tools

Google 不发 aarch64 版，用社区从 AOSP 交叉编译的版本（要求 glibc ≥ 2.36）：

```bash
curl -fsSL -o /tmp/arm-tools.sh \
  https://raw.githubusercontent.com/Commit451/android-arm-build-tools/main/install.sh
bash /tmp/arm-tools.sh --sdk "$ANDROID_HOME" --version 36.0.0
"$ANDROID_HOME/build-tools/36.0.0/aapt2" version    # 能打印版本即成功
```

Termux 上对应的静态 aarch64 版本见 `Lzhiyong/sdk-tools`、`hamza72x/android-sdk-linux-arm64`。

## 5. 让 AGP 用这个 aapt2

AGP 8.x 会从 Maven 拉 `aapt2-*-linux`（x86_64）。`scripts/build-android-game.sh [project]`
已自动传 `-Pandroid.aapt2FromMavenOverride=<SDK>/build-tools/<ver>/aapt2`；手跑 `gradlew` 时二选一：

```bash
./gradlew -Pandroid.aapt2FromMavenOverride=$ANDROID_HOME/build-tools/36.0.0/aapt2 :app:assembleDebug

# 或写进 ~/.gradle/gradle.properties（本机路径，不要提交）
echo "android.aapt2FromMavenOverride=$ANDROID_HOME/build-tools/36.0.0/aapt2" >> ~/.gradle/gradle.properties
```

AGP 会打 `option setting 'android.aapt2FromMavenOverride' is experimental` 警告，属正常。

## 6. arm64 专属坑

| 症状 | 原因 | 解决 |
|---|---|---|
| `clang: Exec format error` | NDK prebuilt 是 x86_64 | `make-ndk-host-native.sh` |
| `incompatible constant for this __builtin_neon function` | 用 NDK 头文件配宿主 clang | shim 不传 `-resource-dir`，头文件用宿主自己的 |
| `ld.lld: unable to find library -lunwind` | `libunwind.a` 在 `lib/clang/<v>/lib/linux/aarch64/` | shim 追加该 `-L` |
| `ld.lld: unable to find library -lgcc` | 宿主 clang 默认 `--rtlib=libgcc` | shim 加 `--rtlib=compiler-rt` |
| API level 被 cc-rs 覆盖成 21 | cc 传 `--target=aarch64-linux-android`（无 API 后缀） | shim 把 `--target=...<api>` 放最后 |
| `aapt2 ... Exec format error` / `Daemon startup failed` | Maven 拉的 aapt2 是 x86_64 | `-Pandroid.aapt2FromMavenOverride=<aarch64 aapt2>` |

通用坑（AppCompat 主题、kotlin-stdlib 重复类、换 `.so` 后 clean 等）见
[android-apk.md §6](android-apk.md)。

## 7. 实测记录

Ubuntu 26.04 aarch64（glibc 2.43 / clang 21 / JDK 21 / NDK r29 / SDK 36）
→ `libprisma.so` 11.7 MB → release APK 20 MB；debug APK 装到 Android 16 真机启动，
Vulkan 设备 + swapchain 创建、首帧 `present` 成功。
