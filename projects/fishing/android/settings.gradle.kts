// game-only Android 宿主模板（#598）：由 `scripts/new-android-game.sh` 展开到
// `projects/<p>/android/`，`fishing` 会被替换成工程的库名（Cargo package name，
// `-` 换成 `_`）。这里**没有** Tauri / WebView / 启动器 UI。
pluginManagement {
    repositories {
        gradlePluginPortal()
        google()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "fishing-game"
include(":app")
