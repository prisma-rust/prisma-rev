//! Vulkan 设备上下文：实例、物理设备、逻辑设备、队列。
//!
//! 拥有任何渲染发生前所需的长期 Vulkan 句柄。
//! 交换链和每帧资源位于 [`crate::swapchain`] 和 [`crate::renderer`] 中。

use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_void;

use ash::vk;

use crate::capabilities::{self, RayTracingCaps};

/// 调试构建/当加载器存在时请求的验证层。
const VALIDATION_LAYERS: [&str; 1] = ["VK_LAYER_KHRONOS_validation"];

/// 谁拥有上下文背后的 Vulkan 对象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextOwnership {
    /// [`VulkanContext::new`] 创建，随上下文一起销毁。
    Owned,
    /// 宿主（编辑器 UI 框架）创建并与渲染器共享；上下文析构时**不销毁**。
    External,
}

/// All the long-lived Vulkan 状态 the 渲染器 needs to do anything.
pub struct VulkanContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,

    /// 队列 family 索引 that supports both graphics and presentation.
    pub graphics_queue_family: u32,
    pub graphics_queue: vk::Queue,

    /// Properties of the chosen 物理 设备 kept for 交换链 queries.
    pub physical_device_properties: vk::PhysicalDeviceProperties,
    pub physical_device_memory_properties: vk::PhysicalDeviceMemoryProperties,

    /// Probed ray-tracing capabilities of the chosen 物理 设备
    /// All fields are `false` on non-RT hardware; callers can 分支 freely.
    pub rt_caps: RayTracingCaps,

    /// `fillModeNonSolid` 是否可用（Wireframe 管线需要）。
    pub wireframe_supported: bool,

    /// 加速度 structure 函数 pointers (loaded when the
    /// `VK_KHR_acceleration_structure` 扩展 is 启用 `None` otherwise).
    pub acceleration_structure_fn: Option<ash::khr::acceleration_structure::Device>,

    /// 设备 扩展 names that were actually 启用 (RT extensions are
    /// conditional; the rest are always-on). Stored for later RT modules.
    enabled_extensions: Vec<CString>,

    /// 对象所有权：`Owned` 在析构时销毁设备/实例，`External` 留给宿主。
    ownership: ContextOwnership,

    // Held for 放置 ordering / FFI 生命周期
    _debug_messenger: Option<vk::DebugUtilsMessengerEXT>,
}

impl VulkanContext {
    /// 创建 the 实例 and 设备
    ///
    /// `window_extensions` are the 实例 extensions the 表面 needs
    /// (obtained via [`ash_window::enumerate_required_extensions`]).
    pub fn new(window_extensions: &[&str]) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let entry = unsafe { ash::Entry::load() }.context("failed to load Vulkan loader")?;

        let enable_debug = cfg!(debug_assertions);
        let instance = create_instance(&entry, window_extensions, enable_debug)?;
        let debug_messenger = if enable_debug {
            setup_debug_messenger(&entry, &instance)
        } else {
            None
        };

        let physical_device = pick_physical_device(&instance)?;
        let physical_device_properties =
            unsafe { instance.get_physical_device_properties(physical_device) };
        let physical_device_memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };

        // Probe ray-tracing capabilities *before* 设备 creation so we can
        // conditionally enable extensions and 链 the 右 特性 structs.
        let rt_caps = unsafe { capabilities::probe(&instance, physical_device) };
        log::info!("RT capabilities: {rt_caps}");
        let wireframe_supported = unsafe { instance.get_physical_device_features(physical_device) }
            .fill_mode_non_solid
            != 0;

        let graphics_queue_family = pick_graphics_queue_family(&instance, physical_device)
            .context("no graphics-capable queue family found")?;

        let (device, enabled_extensions) = create_device(
            &instance,
            physical_device,
            graphics_queue_family,
            &rt_caps,
            !window_extensions.is_empty(),
            &[],
        )?;

        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };

        // 加载 RT 扩展 函数 pointers if the 扩展 was 启用
        let acceleration_structure_fn = if rt_caps.acceleration_structure {
            Some(ash::khr::acceleration_structure::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };

        Ok(Self {
            entry,
            instance,
            physical_device,
            device,
            graphics_queue_family,
            graphics_queue,
            physical_device_properties,
            physical_device_memory_properties,
            rt_caps,
            wireframe_supported,
            acceleration_structure_fn,
            enabled_extensions,
            ownership: ContextOwnership::Owned,
            _debug_messenger: debug_messenger,
        })
    }

    /// 宿主（编辑器 / UI 框架）已经建好 instance 与物理设备，但希望渲染器与
    /// UI **共用同一个 `VkDevice`** 时使用。
    ///
    /// 设备仍由本函数按与 [`VulkanContext::new`] 相同的特性集创建（bindless、
    /// 时间线信号量、synchronization2 …… 的前提都在这里），宿主只需要额外
    /// 声明自己的设备扩展（例如 `VK_KHR_swapchain`）。
    ///
    /// 所有权：`instance` 与 `device` 归宿主，上下文析构时**不会**销毁它们；
    /// 宿主必须在渲染器全部销毁之后再销毁设备/实例。
    pub fn from_host(
        entry: ash::Entry,
        instance: ash::Instance,
        physical_device: vk::PhysicalDevice,
        graphics_queue_family: u32,
        extra_device_extensions: &[&str],
    ) -> anyhow::Result<Self> {
        let physical_device_properties =
            unsafe { instance.get_physical_device_properties(physical_device) };
        let physical_device_memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };

        let rt_caps = unsafe { capabilities::probe(&instance, physical_device) };
        log::info!("RT capabilities (host device): {rt_caps}");
        let wireframe_supported = unsafe { instance.get_physical_device_features(physical_device) }
            .fill_mode_non_solid
            != 0;

        let (device, enabled_extensions) = create_device(
            &instance,
            physical_device,
            graphics_queue_family,
            &rt_caps,
            true,
            extra_device_extensions,
        )?;
        let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };

        let acceleration_structure_fn = if rt_caps.acceleration_structure {
            Some(ash::khr::acceleration_structure::Device::new(
                &instance, &device,
            ))
        } else {
            None
        };

        Ok(Self {
            entry,
            instance,
            physical_device,
            device,
            graphics_queue_family,
            graphics_queue,
            physical_device_properties,
            physical_device_memory_properties,
            rt_caps,
            wireframe_supported,
            acceleration_structure_fn,
            enabled_extensions,
            ownership: ContextOwnership::External,
            _debug_messenger: None,
        })
    }

    /// Name an 对象 in the 调试 层 (no-op outside 调试 builds / no 层
    pub fn name_object(&self, ty: vk::ObjectType, handle: u64, name: &str) {
        if self._debug_messenger.is_some() {
            let ext = ash::ext::debug_utils::Device::new(&self.instance, &self.device);
            let name_c = CString::new(name).unwrap();
            let info = vk::DebugUtilsObjectNameInfoEXT {
                s_type: vk::StructureType::DEBUG_UTILS_OBJECT_NAME_INFO_EXT,
                p_next: std::ptr::null(),
                object_type: ty,
                object_handle: handle,
                p_object_name: name_c.as_ptr(),
                _marker: std::marker::PhantomData,
            };
            unsafe {
                let _ = ext.set_debug_utils_object_name(&info);
            }
        }
    }

    /// Names of the 设备 extensions that were 启用 at 设备 creation.
    /// `VK_KHR_swapchain` is included only for windowed contexts (a headless
    /// context like the 全局光照 baker enables no 表面 so 交换链 is omitted);
    /// RT extensions are included when the hardware supports them. Used by RT
    /// modules to decide which 代码 path to take.
    pub fn enabled_extension_names(&self) -> &[CString] {
        &self.enabled_extensions
    }

    /// Convenience: was a specific 设备 扩展 启用
    pub fn has_extension(&self, name: &CStr) -> bool {
        self.enabled_extensions.iter().any(|c| c.as_c_str() == name)
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        // 外部（宿主）创建的对象由宿主销毁：UI 框架的 device 生命周期比渲染器长，
        // 在这里销毁会让 UI 侧句柄悬空。
        if self.ownership == ContextOwnership::External {
            return;
        }
        unsafe {
            // 销毁 the 调试 messenger *before* the 实例 The old 代码
            // guarded the 销毁 on `debug_utils_instance()`, which itself
            // checks `self._debug_messenger.is_some()` -- but we `take()` the
            // messenger 第一个 so the guard saw `None` and the messenger leaked
            // (VUID-vkDestroyInstance-instance-00629). 构建 the ext handle
            // unconditionally; it's a no-op when the 扩展 isn't 启用
            if let Some(messenger) = self._debug_messenger.take() {
                let ext = ash::ext::debug_utils::Instance::new(&self.entry, &self.instance);
                ext.destroy_debug_utils_messenger(messenger, None);
            }
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

// ---------------------------------------------------------------------------
// 实例
// ---------------------------------------------------------------------------

/// 为**宿主**创建 Vulkan 实例（编辑器 UI 框架需要自己持有 instance 来创建
/// surface / swapchain，同时又要和渲染器共用设备）。
///
/// 扩展列表由宿主给出（通常是 `ash_window::enumerate_required_extensions` 的
/// 结果）；debug/validation 策略与 [`VulkanContext::new`] 一致。
pub fn create_host_instance(
    entry: &ash::Entry,
    extensions: &[&str],
) -> anyhow::Result<(ash::Instance, Option<vk::DebugUtilsMessengerEXT>)> {
    let enable_debug = cfg!(debug_assertions);
    let instance = create_instance(entry, extensions, enable_debug)?;
    let messenger = if enable_debug {
        setup_debug_messenger(entry, &instance)
    } else {
        None
    };
    Ok((instance, messenger))
}

/// 选择一块能向 `surface` 呈现、且有图形队列的物理设备。
///
/// 渲染器要求 `maxPushConstantsSize >= 144`（PathTracePass），这里一并校验，
/// 避免宿主先建好设备、渲染器初始化时才失败。
pub fn pick_host_physical_device(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> anyhow::Result<vk::PhysicalDevice> {
    pick_physical_device_impl(instance, |device| {
        pick_present_queue_family(instance, device, surface_loader, surface).is_some()
    })
}

/// 同时支持图形与呈现的队列 family（共享设备的宿主只能用同一个队列族）。
pub fn pick_present_queue_family(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Option<u32> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    for (index, family) in families.iter().enumerate() {
        if !family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
            continue;
        }
        let supported = unsafe {
            surface_loader.get_physical_device_surface_support(
                physical_device,
                index as u32,
                surface,
            )
        }
        .unwrap_or(false);
        if supported {
            return Some(index as u32);
        }
    }
    None
}

fn create_instance(
    entry: &ash::Entry,
    window_extensions: &[&str],
    enable_debug: bool,
) -> anyhow::Result<ash::Instance> {
    use anyhow::Context as _;

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"PrismaRev")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"PrismaRev")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        // Keep the instance contract at Vulkan 1.1. Optional 1.2/1.3
        // capabilities are queried per-device and enabled only when present.
        .api_version(vk::API_VERSION_1_1);

    // 实例 extensions: 表面 + platform. 调试 utils only in 调试 builds
    // (it's a debugging aid; the 验证 层 warns if 启用 in 释放
    let mut extension_names: Vec<CString> = window_extensions
        .iter()
        .map(|s| CString::new(*s).unwrap())
        .collect();
    if enable_debug {
        extension_names.push(vk::EXT_DEBUG_UTILS_NAME.into());
    }
    let extension_ptrs: Vec<*const c_char> = extension_names.iter().map(|c| c.as_ptr()).collect();

    // 验证 layers only in 调试 builds — and only when actually installed.
    // 无条件请求会导致无 layer 的环境（如无 validation 包的 Termux/CI）
    // 在 create_instance 阶段直接失败；先枚举可用 layer 再启用。
    let mut enabled_layers: Vec<CString> = Vec::new();
    if enable_debug {
        let available = unsafe { entry.enumerate_instance_layer_properties() }
            .map(|props| {
                props.iter().any(|p| {
                    let name = unsafe { CStr::from_ptr(p.layer_name.as_ptr()) };
                    VALIDATION_LAYERS.contains(&name.to_str().unwrap_or(""))
                })
            })
            .unwrap_or(false);
        if available {
            enabled_layers = VALIDATION_LAYERS
                .iter()
                .map(|s| CString::new(*s).unwrap())
                .collect();
        } else {
            log::warn!("validation layers requested but not available; continuing without");
        }
    }
    let layer_ptrs: Vec<*const c_char> = enabled_layers.iter().map(|c| c.as_ptr()).collect();

    let mut create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&extension_ptrs);
    if enable_debug {
        create_info = create_info.enabled_layer_names(&layer_ptrs);
    }

    let instance = unsafe { entry.create_instance(&create_info, None) }
        .context("failed to create Vulkan instance")?;

    Ok(instance)
}

// ---------------------------------------------------------------------------
// 物理 设备
// ---------------------------------------------------------------------------

fn pick_physical_device(instance: &ash::Instance) -> anyhow::Result<vk::PhysicalDevice> {
    pick_physical_device_impl(instance, |_| true)
}

fn pick_physical_device_impl(
    instance: &ash::Instance,
    accept: impl Fn(vk::PhysicalDevice) -> bool,
) -> anyhow::Result<vk::PhysicalDevice> {
    use anyhow::Context as _;
    let devices = unsafe { instance.enumerate_physical_devices() }
        .context("failed to enumerate physical devices")?;

    // Prefer a 离散 GPU, fall 后 to anything with a graphics 队列
    // Bonus points for ray-tracing support: if two GPUs tie on 设备 类型
    // the one with RT wins. RT is *not* required -- a non-RT GPU is still
    // selected and simply renders via the 光栅化 path.
    let mut best = None;
    let mut best_score = -1i32;
    for device in devices {
        if !accept(device) {
            continue;
        }
        let props = unsafe { instance.get_physical_device_properties(device) };
        let score = match props.device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => 3,
            vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
            vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
            _ => 0,
        };
        // Must have a graphics 队列 family or it's useless to us.
        if pick_graphics_queue_family(instance, device).is_some() && score > best_score {
            best_score = score;
            best = Some(device);
        }
    }
    let device = best.ok_or_else(|| anyhow::anyhow!("no GPU with a graphics queue found"))?;
    // The path-trace pass uses a 144-byte push-constant 块 (PtPushConstants,
    // padded per std140: the trailing `ray_max_distance` 浮点数 rounds the 结构体
    // 上 to a 16-byte multiple). Vulkan only guarantees 128 字节 so 验证
    // the 设备 actually supports more before we 写入 past the guaranteed
    // range. All desktop GPUs and modern mobile parts advertise 256; very old
    // / emulator devices may only do 128 and would silently 截断 the PT
    // 推送 constants.
    let props = unsafe { instance.get_physical_device_properties(device) };
    let max_pc = props.limits.max_push_constants_size;
    anyhow::ensure!(
        max_pc >= 144,
        "selected GPU only supports {} bytes of push constants; PT pass needs 144",
        max_pc
    );
    Ok(device)
}

fn pick_graphics_queue_family(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Option<u32> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    for (i, family) in families.iter().enumerate() {
        if family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
            return Some(i as u32);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 设备
// ---------------------------------------------------------------------------

fn create_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    graphics_queue_family: u32,
    rt_caps: &RayTracingCaps,
    has_surface: bool,
    extra_device_extensions: &[&str],
) -> anyhow::Result<(ash::Device, Vec<CString>)> {
    use anyhow::Context as _;
    let priorities = [1.0f32];
    let queue_create_infos = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(graphics_queue_family)
        .queue_priorities(&priorities)];

    // 查询 the available legacy features 验证 层 wants this) and
    // mirror the ones we need into the Features2 链
    let available_features = unsafe { instance.get_physical_device_features(physical_device) };
    let legacy_features = vk::PhysicalDeviceFeatures {
        shader_clip_distance: available_features.shader_clip_distance,
        // MRT pipelines (ForwardPass writes 颜色 + view-space 法线 use
        // different 混合 states per 附件 颜色 = Alpha 混合 法线 =
        // no 混合 Vulkan requires `independentBlend` for that; without it
        // every 附件 must share the same 混合 配置 Universally
        // supported on desktop + modern Android
        independent_blend: available_features.independent_blend,
        // Wireframe（POLYGON_MODE_LINE）需要这个特性；设备不支持时不要请求。
        fill_mode_non_solid: available_features.fill_mode_non_solid,
        ..vk::PhysicalDeviceFeatures::default()
    };

    // --- 构建 the 扩展 列表 交换链 (windowed only) + RT (conditional) ---
    // VK_KHR_swapchain requires VK_KHR_surface on the 实例 a headless
    // context (baker) enables no 表面 实例 extensions, so requesting
    // 交换链 there trips 验证 Only enable it when a 表面 存在
    let mut enabled_extensions: Vec<CString> = Vec::new();
    if has_surface {
        enabled_extensions.push(ash::khr::swapchain::NAME.into());
    }
    // `cmd_pipeline_barrier2` / `ImageMemoryBarrier2` (used unconditionally in
    // `buffer.rs` for 纹理 uploads and mip generation) come from
    // VK_KHR_synchronization2. We 目标 a Vulkan 1.2 实例 where the core
    // `vkCmdPipelineBarrier2` symbol is not exposed; only the `...KHR` variant is
    // available once this 扩展 is 启用 `buffer.rs` therefore drives the
    // 屏障 through `ash::khr::synchronization2::Device`, which resolves the
    // KHR entry point. The 扩展 must be 启用 here or that fails to 加载
    if rt_caps.synchronization2 {
        enabled_extensions.push(ash::khr::synchronization2::NAME.into());
    }
    // `cmd_blit_image2` (used by mip generation in `buffer.rs`) is a Vulkan 1.3
    // core symbol not exposed on a 1.2 设备 it is promoted from
    // VK_KHR_copy_commands2. Enable the 扩展 so the KHR entry point loads,
    // and 调用 it through `ash::khr::copy_commands2::Device` in `buffer.rs`.
    if rt_caps.copy_commands2 {
        enabled_extensions.push(ash::khr::copy_commands2::NAME.into());
    }
    for rt_ext in capabilities::rt_extension_names(rt_caps) {
        enabled_extensions.push(rt_ext.into());
    }
    // 宿主额外要求的扩展（例如 UI 框架的 `VK_KHR_swapchain`）。去重，避免
    // `VUID-VkDeviceCreateInfo-ppEnabledExtensionNames-00374`（重复扩展名）。
    for name in extra_device_extensions {
        let already = enabled_extensions
            .iter()
            .any(|existing| existing.to_str().map(|s| s == *name).unwrap_or(false));
        if !already {
            enabled_extensions.push(CString::new(*name).expect("extension name"));
        }
    }
    let extension_ptrs: Vec<*const c_char> =
        enabled_extensions.iter().map(|c| c.as_ptr()).collect();

    // --- 构建 the VkPhysicalDeviceFeatures2 pNext 链 ---
    // Each 特性 结构体 is declared out here so it outlives the create_info
    // 借用 (same 模式 as validation_features in create_instance).
    let mut vk11 = vk::PhysicalDeviceVulkan11Features::default();
    let mut vk12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut accel_features = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default();
    let mut rt_pipeline_features = vk::PhysicalDeviceRayTracingPipelineFeaturesKHR::default();
    let mut ray_query_features = vk::PhysicalDeviceRayQueryFeaturesKHR::default();
    // `synchronization2` is a Vulkan 1.3 promoted 特性 On this 1.2 设备
    // it is only available via the VK_KHR_synchronization2 *feature* (not just
    // the 扩展 enabling the 扩展 exposes the entry points, but the
    // 特性 bit must also be turned on or `vkCmdPipelineBarrier2KHR` is
    // illegal. `buffer.rs` issues these barriers unconditionally for 纹理
    // upload + mip generation.
    let mut sync2_features = vk::PhysicalDeviceSynchronization2FeaturesKHR::default();

    // Vulkan 1.1 特性 shaderDrawParameters is needed when a 着色器
    // references SV_VertexID (DrawParameters SPIR-V 能力 The skybox
    // 顶点 着色器 uses vid%8 to select cube corners without a 顶点 缓冲区
    vk11.shader_draw_parameters = vk::TRUE;

    // 层 1: Vulkan 1.2 promoted features that RT depends on.
    if rt_caps.buffer_device_address {
        vk12.buffer_device_address = vk::TRUE;
    }
    if rt_caps.descriptor_indexing {
        vk12.descriptor_indexing = vk::TRUE;
        // Bindless (see bindless.rs): a runtime-sized, partially-bound,
        // update-after-bind 数组 of sampled images indexed by u32 handle.
        // These sub-features are all part of Vulkan 1.2 描述符 indexing;
        // enabling them here lets BindlessTextureTable allocate its 集合
        vk12.runtime_descriptor_array = vk::TRUE;
        vk12.descriptor_binding_partially_bound = vk::TRUE;
        vk12.descriptor_binding_sampled_image_update_after_bind = vk::TRUE;
        vk12.descriptor_binding_variable_descriptor_count = vk::TRUE;
        vk12.shader_sampled_image_array_non_uniform_indexing = vk::TRUE;
        // PathTracePass (and potentially other 计算 passes) updates
        // STORAGE_IMAGE and STORAGE_BUFFER descriptors every 帧 while
        // previous-frame 命令 buffers are still in flight, which requires
        // the descriptor-binding-level update-after-bind 特性
        vk12.descriptor_binding_storage_image_update_after_bind = vk::TRUE;
        vk12.descriptor_binding_storage_buffer_update_after_bind = vk::TRUE;
    }
    if rt_caps.timeline_semaphore {
        vk12.timeline_semaphore = vk::TRUE;
    }

    if rt_caps.synchronization2 {
        sync2_features.synchronization2 = vk::TRUE;
    }
    let mut features2 = vk::PhysicalDeviceFeatures2::default()
        .features(legacy_features)
        .push_next(&mut vk11)
        .push_next(&mut vk12)
        .push_next(&mut sync2_features);

    // 层 2-4: RT features only when the caps say they're supported.
    if rt_caps.acceleration_structure {
        accel_features.acceleration_structure = vk::TRUE;
        // PathTracePass updates the TLAS 描述符 绑定 2) every 帧
        // this sub-feature is required when the 描述符 集合 uses
        // UPDATE_AFTER_BIND on ACCELERATION_STRUCTURE bindings.
        accel_features.descriptor_binding_acceleration_structure_update_after_bind = vk::TRUE;
        features2 = features2.push_next(&mut accel_features);
    }
    if rt_caps.ray_tracing_pipeline {
        rt_pipeline_features.ray_tracing_pipeline = vk::TRUE;
        features2 = features2.push_next(&mut rt_pipeline_features);
    }
    if rt_caps.ray_query {
        ray_query_features.ray_query = vk::TRUE;
        features2 = features2.push_next(&mut ray_query_features);
    }

    let create_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_create_infos)
        .enabled_extension_names(&extension_ptrs)
        .push_next(&mut features2);

    let device = unsafe { instance.create_device(physical_device, &create_info, None) }
        .context("failed to create logical device")?;

    if rt_caps.any_ray_tracing() {
        log::info!(
            "device created with ray tracing: pipeline={} query={}",
            rt_caps.has_rt_pipeline(),
            rt_caps.has_ray_query()
        );
    } else {
        log::info!("device created (no ray tracing support)");
    }

    Ok((device, enabled_extensions))
}

// ---------------------------------------------------------------------------
// 调试 messenger
// ---------------------------------------------------------------------------

fn setup_debug_messenger(
    entry: &ash::Entry,
    instance: &ash::Instance,
) -> Option<vk::DebugUtilsMessengerEXT> {
    let available = unsafe { entry.enumerate_instance_layer_properties() }
        .ok()?
        .iter()
        .any(|p| {
            let name = unsafe { CStr::from_ptr(p.layer_name.as_ptr()) };
            name == c"VK_LAYER_KHRONOS_validation"
        });
    if !available {
        log::warn!("validation layers requested but not available");
        return None;
    }

    let ext = ash::ext::debug_utils::Instance::new(entry, instance);
    let create_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(debug_callback));

    Some(
        unsafe { ext.create_debug_utils_messenger(&create_info, None) }
            .expect("failed to create debug messenger despite layer being available"),
    )
}

unsafe extern "system" fn debug_callback(
    message_severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _message_type: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _p_user_data: *mut c_void,
) -> vk::Bool32 {
    let message = if p_callback_data.is_null() {
        String::from("(no message)")
    } else {
        let data = unsafe { &*p_callback_data };
        unsafe { CStr::from_ptr(data.p_message) }
            .to_string_lossy()
            .into_owned()
    };

    if message_severity >= vk::DebugUtilsMessageSeverityFlagsEXT::ERROR {
        log::error!("[validation] {message}");
    } else if message_severity >= vk::DebugUtilsMessageSeverityFlagsEXT::WARNING {
        log::warn!("[validation] {message}");
    }
    vk::FALSE
}
