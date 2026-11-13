// game-only Android 宿主（无 launcher / 无 Tauri）：见 docs/android-arm64.md
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

rootProject.name = "template-game"
include(":app")
