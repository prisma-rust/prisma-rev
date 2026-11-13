package com.example.tauriandroidapp

import android.app.Activity
import android.content.Context
import android.content.Intent
import com.prisma.MainActivity
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.util.DisplayMetrics
import android.view.WindowManager
import android.view.WindowInsets
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin
import app.tauri.plugin.JSObject

@InvokeArg
class ColorArgs {
  lateinit var color: String
}

@InvokeArg
class VibrateArgs {
  var duration: Long = 200
}

@InvokeArg
class LaunchArgs {
  /** LaunchConfig JSON（场景/日志级别等）；null = 游戏默认。 */
  var config: String? = null
}

/**
 * 自定义 Tauri Mobile Plugin:调用 Android 原生 API
 *
 * 演示四种能力:
 *  - toggle_immersive:      切换全屏沉浸模式(隐藏/显示系统状态栏和导航栏)
 *  - set_status_bar_color:  修改状态栏颜色
 *  - vibrate:               震动(硬件访问)
 *  - get_device_info:       获取设备硬件信息
 */
@TauriPlugin
class NativePlugin(private val activity: Activity) : Plugin(activity) {

  private var immersive = false

  /**
   * 切换沉浸式全屏模式。
   * 使用 WindowInsetsControllerCompat 控制 system bars 的显隐,
   * BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE 让用户从屏幕边缘滑动可临时呼出。
   */
  @Command
  fun toggle_immersive(invoke: Invoke) {
    try {
      val window = activity.window
      val controller = WindowCompat.getInsetsController(window, window.decorView)

      immersive = !immersive
      if (immersive) {
        // 隐藏状态栏和导航栏,沉浸模式
        controller.hide(WindowInsetsCompat.Type.systemBars())
        controller.systemBarsBehavior =
          WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        // 让内容延伸到系统栏区域
        WindowCompat.setDecorFitsSystemWindows(window, false)
      } else {
        // 恢复显示系统栏
        controller.show(WindowInsetsCompat.Type.systemBars())
        WindowCompat.setDecorFitsSystemWindows(window, true)
      }

      val result = JSObject()
      result.put("immersive", immersive)
      invoke.resolve(result)
    } catch (ex: Exception) {
      invoke.reject(ex.message)
    }
  }

  /**
   * 设置状态栏颜色。
   * 解析 hex 颜色字符串(如 "#FF5722"),修改 statusBarColor 和导航栏颜色,
   * 同时调整状态栏图标颜色(深色/浅色)以保证可读性。
   */
  @Command
  fun set_status_bar_color(invoke: Invoke) {
    try {
      val args = invoke.parseArgs(ColorArgs::class.java)
      val color = android.graphics.Color.parseColor(args.color)

      val window = activity.window
      // 清除 FLAG_TRANSLUCENT_STATUS(否则 setColor 无效)
      window.clearFlags(WindowManager.LayoutParams.FLAG_TRANSLUCENT_STATUS)
      // 让内容延伸到状态栏下方
      window.addFlags(WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS)
      window.statusBarColor = color
      window.navigationBarColor = color

      // 根据背景亮度自动切换状态栏图标颜色(深色背景用浅色图标)
      val controller = WindowCompat.getInsetsController(window, window.decorView)
      val isLight = isColorLight(color)
      controller.isAppearanceLightStatusBars = isLight
      controller.isAppearanceLightNavigationBars = isLight

      invoke.resolve()
    } catch (ex: Exception) {
      invoke.reject(ex.message)
    }
  }

  /**
   * 震动(硬件访问演示)。
   * Android 8.0+ 用 VibrationEffect + VibratorManager,
   * 低版本回退到 deprecated 的 Vibrator.vibrate(ms)。
   */
  @Command
  fun vibrate(invoke: Invoke) {
    try {
      val args = invoke.parseArgs(VibrateArgs::class.java)
      val duration = args.duration

      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        val vibratorManager =
          activity.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager
        val effect = VibrationEffect.createOneShot(duration, VibrationEffect.DEFAULT_AMPLITUDE)
        vibratorManager.defaultVibrator.vibrate(effect)
      } else {
        @Suppress("DEPRECATION")
        val vibrator = activity.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
          val effect = VibrationEffect.createOneShot(duration, VibrationEffect.DEFAULT_AMPLITUDE)
          vibrator.vibrate(effect)
        } else {
          @Suppress("DEPRECATION")
          vibrator.vibrate(duration)
        }
      }

      invoke.resolve()
    } catch (ex: Exception) {
      invoke.reject(ex.message)
    }
  }

  /**
   * 获取设备硬件信息。
   * 聚合 Build 常量、Android 版本、屏幕尺寸、CPU 架构等,
   * 以 JSON 返回给前端。
   */
  @Command
  fun get_device_info(invoke: Invoke) {
    try {
      val result = JSObject()
      // 设备型号
      result.put("manufacturer", Build.MANUFACTURER)
      result.put("model", Build.MODEL)
      result.put("brand", Build.BRAND)
      result.put("device", Build.DEVICE)
      // Android 版本
      result.put("androidVersion", Build.VERSION.RELEASE)
      result.put("sdkVersion", Build.VERSION.SDK_INT)
      // CPU 架构
      val abis = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
        Build.SUPPORTED_ABIS.joinToString(", ")
      } else {
        @Suppress("DEPRECATION")
        Build.CPU_ABI
      }
      result.put("cpuAbi", abis)
      // 屏幕尺寸
      val metrics = DisplayMetrics()
      activity.windowManager.defaultDisplay.getMetrics(metrics)
      result.put("screenWidth", metrics.widthPixels)
      result.put("screenHeight", metrics.heightPixels)
      result.put("screenDensity", metrics.densityDpi)
      // 应用包名
      result.put("packageName", activity.packageName)

      invoke.resolve(result)
    } catch (ex: Exception) {
      invoke.reject(ex.message)
    }
  }

  /**
   * 启动游戏:从启动器(Tauri webview)跳转到 Vulkan 游戏 Activity。
   * 通过显式 Intent 启动 com.prisma.MainActivity(GameActivity),
   * 由 GameActivity 加载 libtemplate_core.so 运行渲染器。
   * 可选 `config`（LaunchConfig JSON）先落盘到 app files 目录,
   * 游戏 Rust 侧从同一位置读取（android-activity internal_data_path）。
   */
  @Command
  fun launch_game(invoke: Invoke) {
    try {
      val args = invoke.parseArgs(LaunchArgs::class.java)
      args.config?.let { cfg ->
        val f = java.io.File(activity.filesDir, "launch_config.json")
        f.writeText(cfg)
      }
      val intent = Intent(activity, MainActivity::class.java)
      activity.startActivity(intent)
      invoke.resolve()
    } catch (ex: Exception) {
      invoke.reject(ex.message)
    }
  }

  /** 判断颜色是否为浅色(用于决定状态栏图标颜色) */
  private fun isColorLight(color: Int): Boolean {
    val darkness = 1.0 - (
      0.299 * android.graphics.Color.red(color) +
        0.587 * android.graphics.Color.green(color) +
        0.114 * android.graphics.Color.blue(color)
      ) / 255.0
    return darkness < 0.5
  }
}
