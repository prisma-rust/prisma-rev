//! 硬件光线追踪能力检测。
//!
//! 在创建逻辑设备之前，我们探测物理设备的光线追踪支持：
//! 哪些扩展已公告，以及特性链是否实际报告它们受支持。
//! 结果（[`RayTracingCaps`]）驱动 [`crate::context`] 中的条件性扩展/特性启用。
//!
//! 检测是分层的：
//!
//! ```text
//! 第 4 层 VK_KHR_ray_query 在任何着色器阶段中的内联光线
//! 第 3 层 VK_KHR_ray_tracing_pipeline RT 核心管线（完整 SBT）
//! 第 2 层 VK_KHR_acceleration_structure + deferred_host_operations
//! 第 1 层 Vulkan 1.2 提升特性（buffer_device_address、descriptor_indexing、timeline_semaphore）
//! ```
//!
//! An extension is only considered *usable* when **both** the extension is
//! advertised by the driver **and** the corresponding feature struct reports
//! it as supported (`vkGetPhysicalDeviceFeatures2`).

use std::collections::HashSet;
use std::ffi::CStr;

use ash::vk;

/// Result of probing one physical device for ray-tracing capabilities.
///
/// Every field is `false` / zero when the feature is absent, so callers can
/// unconditionally branch on these flags without risking a panic on non-RT
/// hardware.
#[derive(Debug, Clone, Default)]
pub struct RayTracingCaps {
    // -- Layer 1: Vulkan 1.2 promoted features (foundation for RT) --
    /// Physical device supports Vulkan 1.2 API (`api_version >= 1.2`).
    pub vulkan_1_2: bool,
    /// `bufferDeviceAddress` available (required by acceleration structures).
    pub buffer_device_address: bool,
    /// `descriptorIndexing` available (used by RT descriptor layouts).
    pub descriptor_indexing: bool,
    /// `timelineSemaphore` available (useful for long-running AS builds).
    pub timeline_semaphore: bool,
    pub synchronization2: bool,
    pub copy_commands2: bool,
    /// `shaderBufferInt64Atomics` available (64-bit atomics, used by RTXGI SHARC).
    pub shader_buffer_int64_atomics: bool,
    /// `shaderInt64` available. RTXGI's hash-grid key packing (`HashGridKey`)
    /// is a `uint64_t`, so a shader module that declares the SPIR-V `Int64`
    /// capability cannot be created without this feature.
    pub shader_int64: bool,
    /// `shaderFloat16` available. SHARC packs cached radiance as `float16_t4`.
    pub shader_float16: bool,
    /// `storageBuffer16BitAccess` available. The packed `SharcPackedData`
    /// storage buffer holds 16-bit components, which needs this feature in
    /// addition to `shaderFloat16`.
    pub storage_buffer_16bit_access: bool,
    /// `uniformAndStorageBuffer16BitAccess` available. Slang emits this
    /// capability (alongside `StorageBuffer16BitAccess`) for the SHARC cache
    /// buffers, and Vulkan 1.1 wants both feature bits enabled to match.
    pub uniform_and_storage_buffer_16bit_access: bool,

    // -- Layer 2: acceleration structures (prerequisite for any RT) --
    /// `VK_KHR_acceleration_structure` extension + feature available.
    pub acceleration_structure: bool,
    /// `VK_KHR_deferred_host_operations` available (AS build dependency).
    pub deferred_host_operations: bool,

    // -- Layer 3: RT-core pipeline --
    /// `VK_KHR_ray_tracing_pipeline` extension + feature available.
    pub ray_tracing_pipeline: bool,

    // -- Layer 4: inline ray queries --
    /// `VK_KHR_ray_query` extension + feature available.
    pub ray_query: bool,

    // -- RT pipeline properties (only meaningful when ray_tracing_pipeline) --
    pub max_recursion_depth: u32,
    pub shader_group_handle_size: u32,
    pub max_shader_group_stride: u32,
    pub shader_group_base_alignment: u32,
    pub max_ray_dispatch_invocation_count: u32,
    pub shader_group_handle_alignment: u32,
    pub max_ray_hit_attribute_size: u32,
}

impl RayTracingCaps {
    /// Convenience: is *any* ray-tracing path available?
    /// True when acceleration structures are present and at least one of
    /// the ray-tracing-pipeline or ray-query layers is usable.
    pub fn any_ray_tracing(&self) -> bool {
        self.acceleration_structure && (self.ray_tracing_pipeline || self.ray_query)
    }

    /// The full RT-core pipeline path (BLAS/TLAS + SBT + rgen/rmiss/rchit).
    pub fn has_rt_pipeline(&self) -> bool {
        self.acceleration_structure && self.ray_tracing_pipeline
    }

    /// The lighter ray-query path (inline `traceRayEXT` in fragment shaders).
    pub fn has_ray_query(&self) -> bool {
        self.acceleration_structure && self.ray_query
    }

    /// 纹理上传路径所需的扩展能力。该能力缺失时，设备创建应失败，
    /// 以避免在上传阶段才触发无效的 Vulkan 命令。
    pub fn has_transfer_commands(&self) -> bool {
        self.synchronization2 && self.copy_commands2
    }
}

impl std::fmt::Display for RayTracingCaps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "vulkan_1_2={} buffer_device_address={} descriptor_indexing={} \
             timeline_semaphore={} synchronization2={} copy_commands2={} accel_struct={} deferred_host={} \
             rt_pipeline={} ray_query={}",
            self.vulkan_1_2,
            self.buffer_device_address,
            self.descriptor_indexing,
            self.timeline_semaphore,
            self.synchronization2,
            self.copy_commands2,
            self.acceleration_structure,
            self.deferred_host_operations,
            self.ray_tracing_pipeline,
            self.ray_query,
        )?;
        if self.ray_tracing_pipeline {
            write!(
                f,
                " [max_recursion={} handle_size={} stride={} base_align={}]",
                self.max_recursion_depth,
                self.shader_group_handle_size,
                self.max_shader_group_stride,
                self.shader_group_base_alignment,
            )?;
        }
        Ok(())
    }
}

/// Collect all device extension names advertised by the physical device.
///
/// Pure function over the enumerated properties; kept separate from
/// [`probe`] so it can be unit-tested with synthetic data.
pub fn collect_extension_names(props: &[vk::ExtensionProperties]) -> HashSet<String> {
    props
        .iter()
        .map(|p| {
            unsafe { CStr::from_ptr(p.extension_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Does the advertised extension set contain `name`?
pub fn has_extension(available: &HashSet<String>, name: &str) -> bool {
    available.contains(name)
}

/// The set of device extensions that should be enabled for ray tracing,
/// given the probed capabilities. Always includes `VK_KHR_swapchain`
/// (the caller already adds it). Returns the RT-specific extension names
/// as `&'static CStr` for direct use in the extension pointer array.
pub fn rt_extension_names(caps: &RayTracingCaps) -> Vec<&'static CStr> {
    let mut names = Vec::new();
    if caps.acceleration_structure {
        // The instance targets Vulkan 1.1, so promoted Vulkan 1.2
        // dependencies must still be enabled by their extension names.
        names.push(vk::EXT_DESCRIPTOR_INDEXING_NAME);
        names.push(vk::KHR_BUFFER_DEVICE_ADDRESS_NAME);
        names.push(vk::KHR_ACCELERATION_STRUCTURE_NAME);
        names.push(vk::KHR_DEFERRED_HOST_OPERATIONS_NAME);
    }
    if caps.ray_tracing_pipeline {
        // Ray-tracing pipeline and ray-query both require SPIR-V 1.4.
        names.push(vk::KHR_SPIRV_1_4_NAME);
        names.push(vk::KHR_SHADER_FLOAT_CONTROLS_NAME);
        names.push(vk::KHR_RAY_TRACING_PIPELINE_NAME);
        names.push(vk::KHR_PIPELINE_LIBRARY_NAME);
    }
    if caps.ray_query {
        names.push(vk::KHR_RAY_QUERY_NAME);
    }
    names
}

/// Probe a physical device for ray-tracing capabilities.
///
/// # Safety
///
/// `instance` and `physical_device` must be valid Vulkan handles obtained
/// from a loaded `ash::Entry`.
pub unsafe fn probe(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> RayTracingCaps {
    // --- api version ---
    let props = unsafe { instance.get_physical_device_properties(physical_device) };
    let vulkan_1_2 = props.api_version >= vk::API_VERSION_1_2;
    let vulkan_1_3 = props.api_version >= vk::API_VERSION_1_3;

    // --- advertised extensions ---
    let ext_props = unsafe {
        instance
            .enumerate_device_extension_properties(physical_device)
            .unwrap_or_default()
    };
    let available = collect_extension_names(&ext_props);

    let has_accel_ext = has_extension(
        &available,
        vk::KHR_ACCELERATION_STRUCTURE_NAME.to_str().unwrap(),
    );
    let has_rt_pipeline_ext = has_extension(
        &available,
        vk::KHR_RAY_TRACING_PIPELINE_NAME.to_str().unwrap(),
    );
    let has_ray_query_ext = has_extension(&available, vk::KHR_RAY_QUERY_NAME.to_str().unwrap());
    let has_deferred_ext = has_extension(
        &available,
        vk::KHR_DEFERRED_HOST_OPERATIONS_NAME.to_str().unwrap(),
    );
    let has_descriptor_indexing_ext = has_extension(
        &available,
        vk::EXT_DESCRIPTOR_INDEXING_NAME.to_str().unwrap(),
    );
    let has_buffer_device_address_ext = has_extension(
        &available,
        vk::KHR_BUFFER_DEVICE_ADDRESS_NAME.to_str().unwrap(),
    );
    let has_spirv_1_4_ext = has_extension(&available, vk::KHR_SPIRV_1_4_NAME.to_str().unwrap());
    let has_shader_float_controls_ext = has_extension(
        &available,
        vk::KHR_SHADER_FLOAT_CONTROLS_NAME.to_str().unwrap(),
    );

    // --- feature chain: query what the driver actually supports ---
    // We chain Vulkan12Features + the three RT feature structs (when their
    // extensions are advertised) and read back the support bools.
    let mut vk11 = vk::PhysicalDeviceVulkan11Features::default();
    let mut vk12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut accel_features = vk::PhysicalDeviceAccelerationStructureFeaturesKHR::default();
    let mut rt_pipeline_features = vk::PhysicalDeviceRayTracingPipelineFeaturesKHR::default();
    let mut ray_query_features = vk::PhysicalDeviceRayQueryFeaturesKHR::default();

    let mut features2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut vk11)
        .push_next(&mut vk12);

    if has_accel_ext {
        features2 = features2.push_next(&mut accel_features);
    }
    if has_rt_pipeline_ext {
        features2 = features2.push_next(&mut rt_pipeline_features);
    }
    if has_ray_query_ext {
        features2 = features2.push_next(&mut ray_query_features);
    }

    unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };

    // Layer 1: Vulkan 1.2 promoted features.
    let buffer_device_address = vk12.buffer_device_address == vk::TRUE;
    let descriptor_indexing = vk12.descriptor_indexing == vk::TRUE;
    let timeline_semaphore = vk12.timeline_semaphore == vk::TRUE;
    let shader_buffer_int64_atomics = vk12.shader_buffer_int64_atomics == vk::TRUE;
    let shader_float16 = vk12.shader_float16 == vk::TRUE;
    let storage_buffer_16bit_access = vk11.storage_buffer16_bit_access == vk::TRUE;
    let uniform_and_storage_buffer_16bit_access =
        vk11.uniform_and_storage_buffer16_bit_access == vk::TRUE;
    // `shaderInt64` lives in the legacy `VkPhysicalDeviceFeatures`, not in the
    // Vulkan12 struct, so it needs its own query.
    let legacy_features = unsafe { instance.get_physical_device_features(physical_device) };
    let shader_int64 = legacy_features.shader_int64 == vk::TRUE;

    // Layer 2: acceleration structure (real only when ext + feature agree).
    let acceleration_structure = has_accel_ext
        && has_deferred_ext
        && has_descriptor_indexing_ext
        && has_buffer_device_address_ext
        && accel_features.acceleration_structure == vk::TRUE;
    let deferred_host_operations = has_deferred_ext;

    // Layer 3/4: RT pipeline / ray query (independent of each other).
    let ray_tracing_pipeline = has_rt_pipeline_ext
        && has_spirv_1_4_ext
        && has_shader_float_controls_ext
        && rt_pipeline_features.ray_tracing_pipeline == vk::TRUE;
    let ray_query = has_ray_query_ext
        && has_spirv_1_4_ext
        && has_shader_float_controls_ext
        && ray_query_features.ray_query == vk::TRUE;

    // --- RT pipeline properties (SBT alignment etc.) ---
    let mut rt_props = vk::PhysicalDeviceRayTracingPipelinePropertiesKHR::default();
    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut rt_props);
    if ray_tracing_pipeline {
        unsafe { instance.get_physical_device_properties2(physical_device, &mut props2) };
    }

    RayTracingCaps {
        vulkan_1_2,
        buffer_device_address,
        descriptor_indexing,
        timeline_semaphore,
        synchronization2: vulkan_1_3
            || has_extension(&available, vk::KHR_SYNCHRONIZATION2_NAME.to_str().unwrap()),
        copy_commands2: vulkan_1_3
            || has_extension(&available, vk::KHR_COPY_COMMANDS2_NAME.to_str().unwrap()),
        shader_buffer_int64_atomics,
        shader_int64,
        shader_float16,
        storage_buffer_16bit_access,
        uniform_and_storage_buffer_16bit_access,
        acceleration_structure,
        deferred_host_operations,
        ray_tracing_pipeline,
        ray_query,
        max_recursion_depth: rt_props.max_ray_recursion_depth,
        shader_group_handle_size: rt_props.shader_group_handle_size,
        max_shader_group_stride: rt_props.max_shader_group_stride,
        shader_group_base_alignment: rt_props.shader_group_base_alignment,
        max_ray_dispatch_invocation_count: rt_props.max_ray_dispatch_invocation_count,
        shader_group_handle_alignment: rt_props.shader_group_handle_alignment,
        max_ray_hit_attribute_size: rt_props.max_ray_hit_attribute_size,
    }
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;
