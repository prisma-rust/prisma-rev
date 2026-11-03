//! Shader module loading from SPIR-V bytecode.
//!
//! SPIR-V shaders are compiled offline from Slang via `slangc` (see
//! `shaders/compile.sh`).  Built-in shaders are embedded at compile time via
//! `include_bytes!` (the default path).  Content / user shaders can be loaded
//! from the asset pipeline via [`super::shader_asset::load_shader_module_from_rm`].
//!
//! See also `crates/prism-engine/src/shader_asset.rs` for the RM-based entry
//! point.

use anyhow::Context as _;
use ash::vk;

/// Load a shader module from SPIR-V bytecode already in memory.
///
/// The byte slice does **not** need to be 4-byte aligned; a temporary copy is
/// made if necessary.
pub fn load_shader_module(device: &ash::Device, code: &[u8]) -> anyhow::Result<vk::ShaderModule> {
    assert!(
        code.len().is_multiple_of(4),
        "SPIR-V bytecode length ({}) must be a multiple of 4",
        code.len()
    );

    // Align to u32. `include_bytes!` doesn't guarantee alignment, so we try
    // `align_to` first and fall back to a safe copy when misaligned.
    let words: Vec<u32> = if (code.as_ptr() as usize).is_multiple_of(4) {
        // Already aligned - reinterpret without copying.
        let words =
            unsafe { std::slice::from_raw_parts(code.as_ptr() as *const u32, code.len() / 4) };
        words.to_vec()
    } else {
        // Misaligned - copy byte-by-byte.
        code.as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_ne_bytes(*chunk))
            .collect()
    };

    let create_info = vk::ShaderModuleCreateInfo::default().code(&words);
    let module = unsafe { device.create_shader_module(&create_info, None) }
        .context("create shader module")?;
    Ok(module)
}

/// Build a `VkPipelineShaderStageCreateInfo` from a shader module and entry
/// point name (as `&CStr`).
///
/// The caller must ensure the `CStr` lives as long as the returned info
/// (ash stores a raw pointer). Entry-point names come from Slang reflection
/// (e.g. `vertexMain`/`fragmentMain`); see `shader_bindings`.
pub fn shader_stage<'a>(
    stage: vk::ShaderStageFlags,
    module: vk::ShaderModule,
    entry_point: &'a std::ffi::CStr,
) -> vk::PipelineShaderStageCreateInfo<'a> {
    vk::PipelineShaderStageCreateInfo::default()
        .stage(stage)
        .module(module)
        .name(entry_point)
}
