// Game-only Android 宿主（模板，#598）：只把 `lib__GAME_NAME__.so` + 场景包打成 APK。
//
// 和 launcher/src-tauri/gen/android 的区别：这里**没有** Tauri / WebView / 启动器 UI，
// 也没有 Kotlin 编译器；APK 里只有 GameActivity 壳 + 原生 .so + assets/game/。
//
// 由 `scripts/new-android-game.sh` 生成，改模板请改 `templates/android-game/`。
//
// minSdk 31 是硬性下限：libaaudio.so 从 API 26 才有，而引擎的 Android 音频走 AAudio；
// 同时不要低于该工程实际支持的最低设备。
plugins {
    id("com.android.application") version "8.11.0"
}

android {
    namespace = "__PACKAGE__"
    compileSdk = 36

    defaultConfig {
        applicationId = "__PACKAGE__"
        minSdk = 31
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        getByName("debug") { isMinifyEnabled = false }
        getByName("release") {
            isMinifyEnabled = false
            // 签名材料不入库：见 docs/android-apk.md「发布签名」。
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs {
            // debug .so 带调试符号；不要因为找不到 llvm-strip 就整包失败。
            keepDebugSymbols += "**/*.so"
        }
    }
}

dependencies {
    // 对齐 kotlin-stdlib：appcompat/games-activity 会带进 1.6.21 的
    // kotlin-stdlib-jdk7/jdk8，和 stdlib 1.8.x 有重复类，checkDuplicateClasses 会失败。
    // Kotlin BOM 把 jdk7/jdk8 对齐成空壳模块后并入 stdlib。
    implementation(platform("org.jetbrains.kotlin:kotlin-bom:1.9.25"))
    // GameActivity：窗口 + 输入 + 调用 lib__GAME_NAME__.so 的 android_main。
    implementation("androidx.games:games-activity:4.4.0")
    // games-activity 4.4.0 的 GameActivity 继承 AppCompatActivity。
    implementation("androidx.appcompat:appcompat:1.7.1")
}
