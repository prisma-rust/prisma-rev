//! PrismaRev Vulkan 渲染后端
//!
//! 基于 [`ash`]（轻量 Vulkan 绑定）构建。里程碑 2 提供了完整的
//! 光栅化管线：渲染 pass、图形管线、网格缓冲区、
//! 描述符集和相机 UBO——足以渲染 ECS 驱动的几何体。
//!
//! ## 模块
//!
//! | 模块 | 用途 |
//! |--------|---------|
//! | [`capabilities`] | 光线追踪能力检测 |
//! | [`context`] | Vulkan 实例、设备、队列 |
//! | swapchain | 交换链 + acquire/present 同步 |
//! | [`render_pass`] | 渲染 pass + 帧缓冲 |
//! | shader | SPIR-V 着色器模块加载 |
//! | buffer | 缓冲区分配和暂存上传 |
//! | mesh | 顶点/索引缓冲区与网格类型 |
//! | pipeline | 图形管线 |
//! | descriptor | 描述符集合、布局、池、UBO |
//! Vulkan renderer for PrismaRev — the GPU half of the engine.
//! Architecture: [`render_graph::RenderGraph`] drives [`RenderPassNode`] exec.

// Crate-level lint enforcement (applies only to this crate, not deps).
#![deny(warnings)]
// New clippy lints in Rust 1.97 — pre-existing issues, fix when time permits.
#![allow(
    clippy::missing_safety_doc,
    clippy::doc_lazy_continuation,
    clippy::needless_borrow,
    clippy::manual_div_ceil,
    clippy::too_many_arguments
)]

pub mod acceleration_structure;
pub mod anf;
#[cfg(test)]
mod anf_tests;
/// 主线程(CPU) ↔ 渲染线程(GPU) 资产解析请求/结果桥接类型。
pub mod asset_bridge;
pub mod asset_decoders;
/// 自动曝光（亮度直方图 + 适应），移植自 RTXGI `AutoExposurePass`。
pub mod auto_exposure;
pub mod bake_common;
pub mod batch;
pub mod bindless;
pub mod buffer;
pub mod capabilities;
pub mod compute;
pub mod context;
pub mod culling;
pub mod descriptor;
pub mod dirty_dispatch;
pub mod external_overlay;
pub mod forward_pass;
pub mod frame_stats;
pub mod gi;
pub mod gizmo;
pub mod gpu_timer;
pub mod graph_renderer;
pub mod ground_grid;
/// Half-resolution ambient occlusion pass (building-block feature `gtao`).
#[cfg(feature = "gtao")]
pub mod gtao;
pub mod hdr;
/// Linear (pre-tonemap) HDR readback of a graph-published float image.
pub mod hdr_readback;
/// Image-based lighting (building-block feature `ibl`); the module is always compiled,
/// falling back to descriptor layout and 1×1 neutral grey fallback when disabled (see `make_ibl`).
pub mod ibl;
pub mod lut;
pub mod managers;
mod math;
pub mod mesh;
pub mod offscreen;
pub mod particle_pass;
/// `PRISMREV_PASS_DUMP`：headless 下把各个 pass 的附件写成 PNG。
pub(crate) mod pass_dump;
pub mod pipeline;
/// pass 级 feature（`shadows` / `gtao`）关闭时的 1×1 占位资源，以及
/// `giTex` 在 ReSTIR GI 尚未产出时的 1×1 兜底图（该 binding 在着色器里无条件声明）。
pub(crate) mod placeholder;
pub mod plugin;
pub mod post;
pub mod primitives;
pub mod probe_loader;
#[cfg(feature = "pathtrace")]
pub mod pt_pass;
pub mod render_graph;
pub mod render_graph_report;
pub mod render_pass;
pub mod render_runtime;
pub mod render_texture;
#[cfg(feature = "restir_gi")]
pub mod restir_gi;
/// Frame-delayed destruction of GPU objects still referenced by in-flight work.
pub(crate) mod retire;
pub mod rt_pass;
pub mod rt_scheduler;
pub mod scene_scope;
pub mod screenshot;
pub mod shader;
/// Slang-reflection-generated binding constants (set/binding indices, entry
/// point names, push-constant sizes). Regenerate with `xtask/shader-bindgen`
/// after recompiling shaders on a host with slangc - see shaders/compile.sh.
pub mod shader_bindings;
pub mod taa;
/// 方向光阴影贴图边长（texel）。
///
/// Placed outside feature gates: needed by engine texel snapping even when the `shadows`
/// building block is disabled in minimal profiles. `shadow_map_pass` re-exports this same constant.
pub const SHADOW_MAP_SIZE: u32 = 2048;

/// 阴影贴图边长的合法范围（#526）：低于 256 基本不可用，高于 8192 在移动端 /
/// 老桌面 GPU 上会直接分配失败。
pub const MIN_SHADOW_MAP_SIZE: u32 = 256;
pub const MAX_SHADOW_MAP_SIZE: u32 = 8192;

/// Directional shadow-map pass (building-block feature `shadows`).
#[cfg(feature = "shadows")]
pub mod shadow_map_pass;
#[cfg(feature = "sharc")]
pub mod sharc;
pub mod skybox_pass;
pub mod sprite_pass;
pub mod suballocator;
pub mod swapchain;
pub mod tangents;
pub mod ui_overlay;
pub(crate) mod vulkan_compat;

// SceneDrawItem is the engine<->renderer exchange type for resolved draws.
pub use graph_renderer::SceneDrawItem;

pub use anf::{is_qualcomm_adreno, AnfCaps, AnfPass};
pub use buffer::create_buffer;
pub use capabilities::RayTracingCaps;
pub use context::VulkanContext;
pub use descriptor::{
    DescriptorLayout, DescriptorPool, FrameUBO, FrameUBOData, GpuLight, PtAnalyticLight,
    PtEmissiveTri, ReSTIRReservoir, GPU_LIGHT_KIND_POINT, GPU_LIGHT_KIND_SPOT, GPU_LIGHT_SIZE,
    LIGHT_MAX, PT_EMISSIVE_MAX, PT_LIGHT_MAX,
};
pub use dirty_dispatch::DirtyDispatchPlan;
pub use external_overlay::{OverlayMessage, OverlayRecordCtx, SwapchainOverlay};
pub use forward_pass::ForwardPass;
pub use gi::{
    eval_sh9, sample_probe_irradiance, sh_basis, trilinear_weights, world_to_probe_coord,
    ProbeVolumeInfo, SH_COEFF_COUNT,
};
pub use gizmo::{Gizmo, GizmoInstance};
pub use graph_renderer::{FrameCtx, FrameInput, GraphRenderer};
pub use ground_grid::GroundGrid;
#[cfg(feature = "gtao")]
pub use gtao::{GtaoFrameInputs, GtaoPass};
pub use hdr_readback::HdrReadback;
pub use mesh::{Mesh, Vertex};
pub use particle_pass::ParticlePass;
pub use pipeline::GraphicsPipeline;
pub use post::PostPass;
pub use probe_loader::{load_probe_volume, save_probe_volume, ProbeVolumeData};
#[cfg(feature = "pathtrace")]
pub use pt_pass::{PathTracePass, PtVariant, SharcTrainPass};
pub use render_graph::{
    clip_bounds, internal_extent, post_destination, sort_draw_items, trim_sprite_lights, DlssMode,
    DrawItem, DrawState, GiMode, InternalScaleMode, ParticleDrawItem, PassInfo, PassKind,
    PostGrade, RenderGraphSnapshot, RenderMode, RenderSettings, ResourceHandle, ResourceInfo,
    ResourceType, ShadowMode, SpriteDrawItem, SpriteLight, FORWARD_COLOR_H, FORWARD_DEPTH_H,
    FORWARD_NORMAL_H, NO_CLIP, PT_COLOR_H, SHADOW_MAP_H, SPRITE_LIGHT_AMBIENT,
    SPRITE_LIGHT_DIRECTIONAL, SPRITE_LIGHT_MAX, SPRITE_LIGHT_POINT,
};
pub use render_pass::{DepthImage, Framebuffers, NormalImage, RenderPass};
#[cfg(feature = "restir_gi")]
pub use restir_gi::{RestirGiFrame, RestirGiPass, RestirGiReservoir, RESERVOIR_BYTES};
pub use shader::load_shader_module;
#[cfg(feature = "shadows")]
pub use shadow_map_pass::ShadowMapPass;
#[cfg(feature = "sharc")]
pub use sharc::{
    sharc_active, SharcInvalidation, SharcResolveNode, SharcResolvePass, SharcResolvePush,
    SharcTargets, DEFAULT_SHARC_CAPACITY,
};
pub use skybox_pass::SkyboxSettings;
pub use sprite_pass::SpritePass;
pub use suballocator::{MemorySuballocator, StagingArena, Suballocation, SuballocatorStats};
pub use swapchain::Swapchain;
#[cfg(feature = "ui")]
pub use ui_overlay::UiOverlay;
pub use ui_overlay::UiOverlayInput;
