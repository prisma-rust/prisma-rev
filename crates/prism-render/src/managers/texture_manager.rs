//! `RenderTextureManager` — 由无绑定 SRV 表支持的 RGBA8 纹理
//!
//! 管理器拥有每个纹理的设备本地图像+内存+图像视图，
//! 以及它在无绑定描述符集中占用的槽位。
//! 构造时在槽 0 注册了一个永久的 1×1 洋红色回退，
//! 因此未注册/尚未上传的句柄永远不会产生未绑定描述符读取。
//! 渲染器的着色器路径检查 `TextureHandle::INVALID` 并返回回退颜色，
//! CPU 端的 `get_srv` 始终返回真实槽位。
//!
//! P0 范围（提交 3）：
//! - `RenderTextureManager::new` 构建无绑定表并在槽 0 注册回退视图。
//! - `register` 接受 CPU 端纹理并记录无绑定槽位。
//!   实际的 Vulkan 图像/视图由渲染器在提交 9 中构建（可访问每帧命令池和图形队列），
//!   结果 `ImageView` 通过 `attach_image_view` 在此连接。
//!   这种分离使管理器足够 Vulkan 无关，提交 3 无需拖入暂存缓冲区/屏障代码即可编译和单元测试。
//!
//! P0 scope (commit 9): the `register` path will be replaced with an
//! end-to-end image upload (image + memory + view + bindless write in
//! one call), using the existing `buffer::create_buffer` + a small
//! one-shot command buffer helper.

use anyhow::Context as _;
use ash::vk;
use slotmap::{new_key_type, SlotMap};

use crate::bindless::{BindlessTextureTable, TextureHandle};
use crate::buffer::create_and_upload_image;
use crate::context::VulkanContext;

// Local handle. The engine layer translates asset texture handles
// into this when it calls `RenderTextureManager::reserve` so the render
// crate stays decoupled from the asset pipeline.
new_key_type! {
    /// Slotmap handle into [`RenderTextureManager`].
    pub struct TextureHandleSlot;
}

/// Backwards-compatible alias for the slotmap-typed handle. Public so
/// engine code can name it without depending on the new_key_type
/// expansion directly.
pub type AssetTextureHandle = TextureHandleSlot;

/// Plain-data texture description used at the manager boundary. The
/// engine layer translates asset texture data into this.
#[derive(Debug, Clone)]
pub struct TextureUploadInput {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    /// KTX2 已含完整 mip 时 >1，否则 0 表示按 width/height 自动推导（仅 Rgba8）
    pub mip_levels: u32,
    /// Tightly packed rows, no 填充 When `mip_levels > 1` this may hold the
    /// whole baked chain (level 0 first, then each halved level); the upload
    /// copies every level instead of blitting it.
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureFormat {
    Rgba8,
    /// sRGB-encoded RGBA8 -> Vulkan `R8G8B8A8_SRGB`. Hardware performs the
    /// sRGB->linear conversion on sample, so the shader receives linear values
    /// and must NOT apply a manual `pow(2.2)`. Used for albedo / emissive.
    Rgba8Srgb,
    // ── 块压缩格式（DESIGN §7.2）：离线编码，运行时直接上传压缩块 ──
    /// BC1 / DXT1（RGB，4×4 块 8 字节）。
    Bc1Unorm,
    Bc1Srgb,
    /// BC3 / DXT5（RGBA，4×4 块 16 字节）。
    Bc3Unorm,
    Bc3Srgb,
    Bc7Unorm,
    Bc7Srgb,
    Bc5Unorm,
    Bc4Unorm,
    Bc6HUfloat,
    Astc4x4Unorm,
    Astc4x4Srgb,
    Astc6x6Unorm,
    Astc6x6Srgb,
    Astc8x8Unorm,
    Astc8x8Srgb,
    Astc12x12Unorm,
    Astc12x12Srgb,
    Etc2R8G8B8A8Unorm,
    Etc2R8G8B8A8Srgb,
}

impl TextureFormat {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            TextureFormat::Rgba8 | TextureFormat::Rgba8Srgb => 4,
            // 块压缩格式按像素平均字节数近似（仅用于估算，精确大小用 compressed_byte_len）
            TextureFormat::Bc1Unorm | TextureFormat::Bc1Srgb => 1, // 0.5B/px，向上取整
            TextureFormat::Bc3Unorm | TextureFormat::Bc3Srgb => 1,
            TextureFormat::Bc7Unorm | TextureFormat::Bc7Srgb => 1, // 4x4 block 16B = 1B/px
            TextureFormat::Bc5Unorm => 1,
            TextureFormat::Bc4Unorm => 1, // 实际 0.5B/px，这里取整用于校验宽松
            TextureFormat::Bc6HUfloat => 1,
            TextureFormat::Astc4x4Unorm | TextureFormat::Astc4x4Srgb => 1, // 8 bpp
            TextureFormat::Astc6x6Unorm | TextureFormat::Astc6x6Srgb => 1, // 3.56 bpp 近似
            TextureFormat::Astc8x8Unorm | TextureFormat::Astc8x8Srgb => 1,
            TextureFormat::Astc12x12Unorm | TextureFormat::Astc12x12Srgb => 1,
            TextureFormat::Etc2R8G8B8A8Unorm | TextureFormat::Etc2R8G8B8A8Srgb => 1,
        }
    }

    pub const fn is_compressed(self) -> bool {
        !matches!(self, TextureFormat::Rgba8 | TextureFormat::Rgba8Srgb)
    }

    /// 精确的压缩块字节数（按 Vulkan 块大小计算）
    pub fn compressed_byte_len(self, width: u32, height: u32) -> usize {
        match self {
            TextureFormat::Rgba8 | TextureFormat::Rgba8Srgb => {
                (width as usize) * (height as usize) * 4
            }
            TextureFormat::Bc3Unorm
            | TextureFormat::Bc3Srgb
            | TextureFormat::Bc7Unorm
            | TextureFormat::Bc7Srgb
            | TextureFormat::Bc5Unorm
            | TextureFormat::Bc6HUfloat => {
                let bw = (width + 3) / 4;
                let bh = (height + 3) / 4;
                (bw as usize) * (bh as usize) * 16
            }
            TextureFormat::Bc4Unorm | TextureFormat::Bc1Unorm | TextureFormat::Bc1Srgb => {
                let bw = (width + 3) / 4;
                let bh = (height + 3) / 4;
                (bw as usize) * (bh as usize) * 8
            }
            TextureFormat::Astc4x4Unorm | TextureFormat::Astc4x4Srgb => {
                let bw = (width + 3) / 4;
                let bh = (height + 3) / 4;
                (bw as usize) * (bh as usize) * 16
            }
            TextureFormat::Astc6x6Unorm | TextureFormat::Astc6x6Srgb => {
                let bw = (width + 5) / 6;
                let bh = (height + 5) / 6;
                (bw as usize) * (bh as usize) * 16
            }
            TextureFormat::Astc8x8Unorm | TextureFormat::Astc8x8Srgb => {
                let bw = (width + 7) / 8;
                let bh = (height + 7) / 8;
                (bw as usize) * (bh as usize) * 16
            }
            TextureFormat::Astc12x12Unorm | TextureFormat::Astc12x12Srgb => {
                let bw = (width + 11) / 12;
                let bh = (height + 11) / 12;
                (bw as usize) * (bh as usize) * 16
            }
            TextureFormat::Etc2R8G8B8A8Unorm | TextureFormat::Etc2R8G8B8A8Srgb => {
                let bw = (width + 3) / 4;
                let bh = (height + 3) / 4;
                (bw as usize) * (bh as usize) * 16
            }
        }
    }

    /// The Vulkan image format to use for this texture kind.
    pub const fn vk_format(self) -> vk::Format {
        match self {
            TextureFormat::Rgba8 => vk::Format::R8G8B8A8_UNORM,
            TextureFormat::Rgba8Srgb => vk::Format::R8G8B8A8_SRGB,
            TextureFormat::Bc1Unorm => vk::Format::BC1_RGBA_UNORM_BLOCK,
            TextureFormat::Bc1Srgb => vk::Format::BC1_RGBA_SRGB_BLOCK,
            TextureFormat::Bc3Unorm => vk::Format::BC3_UNORM_BLOCK,
            TextureFormat::Bc3Srgb => vk::Format::BC3_SRGB_BLOCK,
            TextureFormat::Bc7Unorm => vk::Format::BC7_UNORM_BLOCK,
            TextureFormat::Bc7Srgb => vk::Format::BC7_SRGB_BLOCK,
            TextureFormat::Bc5Unorm => vk::Format::BC5_UNORM_BLOCK,
            TextureFormat::Bc4Unorm => vk::Format::BC4_UNORM_BLOCK,
            TextureFormat::Bc6HUfloat => vk::Format::BC6H_UFLOAT_BLOCK,
            TextureFormat::Astc4x4Unorm => vk::Format::ASTC_4X4_UNORM_BLOCK,
            TextureFormat::Astc4x4Srgb => vk::Format::ASTC_4X4_SRGB_BLOCK,
            TextureFormat::Astc6x6Unorm => vk::Format::ASTC_6X6_UNORM_BLOCK,
            TextureFormat::Astc6x6Srgb => vk::Format::ASTC_6X6_SRGB_BLOCK,
            TextureFormat::Astc8x8Unorm => vk::Format::ASTC_8X8_UNORM_BLOCK,
            TextureFormat::Astc8x8Srgb => vk::Format::ASTC_8X8_SRGB_BLOCK,
            TextureFormat::Astc12x12Unorm => vk::Format::ASTC_12X12_UNORM_BLOCK,
            TextureFormat::Astc12x12Srgb => vk::Format::ASTC_12X12_SRGB_BLOCK,
            TextureFormat::Etc2R8G8B8A8Unorm => vk::Format::ETC2_R8G8B8A8_UNORM_BLOCK,
            TextureFormat::Etc2R8G8B8A8Srgb => vk::Format::ETC2_R8G8B8A8_SRGB_BLOCK,
        }
    }
}

/// Whether `pixels` is an acceptable payload for this upload.
///
/// Compressed formats and any upload that claims more than one mip level are
/// allowed to carry a *larger* buffer than the base level — that is how a
/// pre-baked KTX2 chain arrives. Everything else must be exactly the base
/// level, so a plainly wrong buffer is still rejected.
fn accepts_pixel_len(
    format: TextureFormat,
    mip_levels: u32,
    pixels_len: usize,
    width: u32,
    height: u32,
) -> bool {
    let base = if format.is_compressed() {
        format.compressed_byte_len(width, height)
    } else {
        (width as usize) * (height as usize) * format.bytes_per_pixel()
    };
    if format.is_compressed() || mip_levels > 1 {
        pixels_len >= base
    } else {
        pixels_len == base
    }
}

/// A handle into the texture manager plus the bindless SRV slot assigned
/// to it. The GPU image / memory / view are owned by the manager and freed
/// in `destroy`.
pub struct UploadedTexture {
    pub srv: TextureHandle,
    /// Width/height are stored here so the renderer can build the image
    /// view info without keeping the input around.
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    /// Owned GPU objects. Kept so `destroy` can release them; the bindless
    /// SRV descriptor merely references `view`.
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

/// Manager of GPU textures. Owns the [`BindlessTextureTable`] and the
/// bindless slot for every registered texture.
pub struct RenderTextureManager {
    bindless: BindlessTextureTable,
    textures: SlotMap<AssetTextureHandle, UploadedTexture>,
    /// Slot 0 of the bindless table is reserved for the magenta fallback
    /// and is never reallocated.
    fallback_srv: TextureHandle,
    /// The fallback image view — written into any slot that is released
    /// via `unregister` so a stale handle sampling the freed view hits
    /// magenta instead of a use-after-free.
    fallback_view: vk::ImageView,
    /// Total slots the bindless table can hold. User textures start at
    /// slot 1 (slot 0 is the fallback). The total is `fallback_capacity +
    /// user_capacity` to keep the math simple.
    #[allow(dead_code)]
    user_capacity: u32,
    destroyed: bool,
}

impl RenderTextureManager {
    /// Construct a new manager with a 1×1 magenta fallback already in slot
    /// 0 of the bindless table.
    ///
    /// `user_capacity` is the maximum number of user textures the manager
    /// will accept; the fallback is allocated *in addition* to this.
    /// The actual Vulkan image + view for the fallback is a real
    /// 1×1 R8G8B8A8_UNORM image, so a missing-texture shader branch
    /// still samples a sensible pixel.
    pub fn new(
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
        user_capacity: u32,
    ) -> anyhow::Result<Self> {
        let total = user_capacity + 1;
        let mut bindless = BindlessTextureTable::new(&context.device, total)
            .map_err(|e| anyhow::anyhow!("RenderTextureManager::new: bindless: {e}"))?;

        // Magenta fallback: 1×1 opaque magenta (R=1,G=0,B=1,A=1) in the
        // engine's linear working space (the shader applies sRGB→linear on
        // sampled albedo; the fallback is only a "missing texture" marker so
        // its exact color space is irrelevant). Written into bindless slot 0.
        let magenta = [255u8, 0, 255, 255];
        let (fb_image, fb_memory, fb_view) = unsafe {
            create_and_upload_image(
                context,
                command_pool,
                graphics_queue,
                1,
                1,
                &magenta,
                1,
                vk::Format::R8G8B8A8_UNORM,
            )
        }
        .context("RenderTextureManager::new: create magenta fallback")?;
        bindless
            .register_with_handle(0, fb_view)
            .context("RenderTextureManager::new: register fallback in slot 0")?;
        let fallback_srv = TextureHandle(0);
        // Keep the fallback GPU objects alive for the manager's lifetime.
        let fallback_tex = UploadedTexture {
            srv: fallback_srv,
            width: 1,
            height: 1,
            mip_levels: 1,
            image: fb_image,
            memory: fb_memory,
            view: fb_view,
        };
        let fallback_view = fb_view;

        let mut textures = SlotMap::with_key();
        // Store the fallback under a dedicated key so `destroy` frees it.
        // Its `srv` is fixed at slot 0 (register_with_handle advanced the
        // table's `next` past 0), so user textures start at slot 1.
        textures.insert(fallback_tex);

        Ok(Self {
            bindless,
            textures,
            fallback_srv,
            fallback_view,
            user_capacity,
            destroyed: false,
        })
    }

    /// The bindless SRV slot of the magenta fallback.
    pub fn fallback_srv(&self) -> TextureHandle {
        self.fallback_srv
    }

    /// Raw bindless table — exposed so the renderer can bind the descriptor
    /// set as part of its pipeline setup.
    pub fn bindless(&self) -> &BindlessTextureTable {
        &self.bindless
    }

    /// Mut access to the bindless table for the renderer to write fallback
    /// view / image view creation in commit 9.
    pub fn bindless_mut(&mut self) -> &mut BindlessTextureTable {
        &mut self.bindless
    }

    /// Upload a CPU-side texture to a device-local image, register its view
    /// in the bindless SRV table, and return a handle. The handle maps to
    /// the bindless slot via [`get_srv`](Self::get_srv).
    pub fn reserve(
        &mut self,
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
        input: &TextureUploadInput,
    ) -> anyhow::Result<AssetTextureHandle> {
        // 块压缩能不能采样由设备决定（很多移动 GPU 没有 BC，桌面没有 ASTC）。
        // 不先查就会变成 VUID 刷屏 + 上传失败；这里明确报错，调用方（引擎的
        // 资产解析）会退化成洋红占位（#519）。
        if input.format.is_compressed()
            && !texture_format_supported(
                &context.instance,
                context.physical_device,
                input.format.vk_format(),
            )
        {
            anyhow::bail!(
                "device cannot sample {:?}; compressed texture rejected",
                input.format
            );
        }
        let expected = if input.format.is_compressed() {
            input.format.compressed_byte_len(input.width, input.height)
        } else {
            (input.width as usize) * (input.height as usize) * input.format.bytes_per_pixel()
        };
        // 压缩格式允许 mip 链时 pixels 更长，宽松校验：至少 base mip 大小
        let valid = accepts_pixel_len(
            input.format,
            input.mip_levels,
            input.pixels.len(),
            input.width,
            input.height,
        );
        if !valid {
            anyhow::bail!(
                "TextureUploadInput: pixel buffer size {} does not match {}x{}*{} (expected >= {})",
                input.pixels.len(),
                input.width,
                input.height,
                input.format.bytes_per_pixel(),
                expected
            );
        }
        if self.textures.len() as u32 > self.user_capacity {
            anyhow::bail!(
                "RenderTextureManager: user capacity {} exhausted",
                self.user_capacity
            );
        }

        // Upload pixels → VkImage + VkImageView (transferDst + SAMPLED).
        let mip_levels = if input.mip_levels != 0 {
            input.mip_levels
        } else if input.format.is_compressed() || input.width <= 1 || input.height <= 1 {
            // 压缩格式 mip 由 KTX2 预生成；1x1 也没有可生成的 mip。
            1
        } else {
            (input.width.max(input.height) as f32).log2().floor() as u32 + 1
        };
        let (image, memory, view) = unsafe {
            create_and_upload_image(
                context,
                command_pool,
                graphics_queue,
                input.width,
                input.height,
                &input.pixels,
                mip_levels,
                input.format.vk_format(),
            )
        }
        .context("RenderTextureManager::reserve: upload texture")?;

        // Reserve the next bindless SRV slot (slot 0 is the magenta fallback,
        // already taken, so this returns 1, 2, ...).
        let srv = self
            .bindless
            .register(view)
            .context("RenderTextureManager::reserve: register bindless SRV")?;

        let handle = self.textures.insert(UploadedTexture {
            srv,
            width: input.width,
            height: input.height,
            mip_levels,
            image,
            memory,
            view,
        });
        Ok(handle)
    }

    /// Like [`reserve`](Self::reserve) but records the image upload into a
    /// shared [`BatchUploader`](crate::batch::BatchUploader) so many textures
    /// can be uploaded with a single submit + fence. The caller must finish
    /// the uploader before sampling the textures.
    pub fn reserve_into(
        &mut self,
        _context: &VulkanContext,
        uploader: &mut crate::batch::BatchUploader<'_>,
        input: &TextureUploadInput,
    ) -> anyhow::Result<AssetTextureHandle> {
        let expected = if input.format.is_compressed() {
            input.format.compressed_byte_len(input.width, input.height)
        } else {
            (input.width as usize) * (input.height as usize) * input.format.bytes_per_pixel()
        };
        let valid = accepts_pixel_len(
            input.format,
            input.mip_levels,
            input.pixels.len(),
            input.width,
            input.height,
        );
        if !valid {
            anyhow::bail!(
                "TextureUploadInput: pixel buffer size {} does not match {}x{}*{} (expected >= {})",
                input.pixels.len(),
                input.width,
                input.height,
                input.format.bytes_per_pixel(),
                expected
            );
        }
        if self.textures.len() as u32 > self.user_capacity {
            anyhow::bail!(
                "RenderTextureManager: user capacity {} exhausted",
                self.user_capacity
            );
        }

        let mip_levels = if input.mip_levels != 0 {
            input.mip_levels
        } else if input.format.is_compressed() {
            1
        } else {
            crate::batch::mip_level_count(input.width, input.height)
        };
        let (image, memory, view) = uploader
            .upload_image(
                input.width,
                input.height,
                mip_levels,
                &input.pixels,
                input.format.vk_format(),
            )
            .context("RenderTextureManager::reserve_into: upload texture")?;

        let srv = self
            .bindless
            .register(view)
            .context("RenderTextureManager::reserve_into: register bindless SRV")?;

        let handle = self.textures.insert(UploadedTexture {
            srv,
            width: input.width,
            height: input.height,
            mip_levels,
            image,
            memory,
            view,
        });
        Ok(handle)
    }

    /// Translate an asset-side texture handle to its bindless SRV slot.
    /// Returns `fallback_srv` (not `INVALID`) when the handle is unknown
    /// so shaders can always sample something visible.
    pub fn get_srv(&self, handle: AssetTextureHandle) -> TextureHandle {
        self.textures
            .get(handle)
            .map(|t| t.srv)
            .unwrap_or(self.fallback_srv)
    }

    /// 放置 a single entry and 释放 its GPU image/memory/view and
    /// reclaims its bindless SRV 槽 The 槽 is rewritten with the
    /// magenta fallback view first, so a stale handle sampling it gets
    /// magenta instead of a freed-view use-after-free. The 槽 number is
    /// returned to the free-list so a later `reserve` reuses it.
    pub fn unregister(&mut self, handle: AssetTextureHandle, device: &ash::Device) {
        if let Some(tex) = self.textures.remove(handle) {
            // Rewrite the descriptor with the fallback view before the
            // image view is destroyed, so any shader holding the old
            // handle samples magenta rather than a freed view.
            self.bindless.write_srv(tex.srv.0, self.fallback_view);
            self.bindless.release_slot(tex.srv);
            unsafe {
                device.destroy_image_view(tex.view, None);
                device.destroy_image(tex.image, None);
                device.free_memory(tex.memory, None);
            }
        }
    }

    /// 按 **bindless SRV 槽位** 释放（#495 热重载 / 实体销毁）。
    ///
    /// 引擎侧组件只存槽位（`SpriteRef.texture_slot` 等），拿不到管理器的
    /// `AssetTextureHandle`；这里反查一次再走 `unregister`，保证图像 / 内存 /
    /// 视图与槽位一起回收，而不是留在显存里。
    pub fn unregister_srv(&mut self, srv: u32, device: &ash::Device) -> bool {
        let Some(handle) = self
            .textures
            .iter()
            .find(|(_, tex)| tex.srv.0 == srv)
            .map(|(handle, _)| handle)
        else {
            return false;
        };
        self.unregister(handle, device);
        true
    }

    /// Release every entry (GPU image/memory/view + bindless slot). The
    /// underlying bindless table is dropped when this manager is dropped,
    /// which destroys the descriptor pool, set, layout, and 4 samplers.
    pub fn destroy(&mut self) {
        let device = self.bindless.device();
        for (_, tex) in self.textures.drain() {
            unsafe {
                device.destroy_image_view(tex.view, None);
                device.destroy_image(tex.image, None);
                device.free_memory(tex.memory, None);
            }
        }
        self.destroyed = true;
    }

    pub fn len(&self) -> usize {
        self.textures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }

    /// 驻留纹理数与估算字节数（base mip，未乘 mip 链；#518 Phase 2）。
    pub fn stats(&self) -> (usize, u64) {
        let bytes = self
            .textures
            .values()
            .map(|texture| texture.width as u64 * texture.height as u64 * 4)
            .sum();
        (self.textures.len(), bytes)
    }
}

impl Drop for RenderTextureManager {
    fn drop(&mut self) {
        debug_assert!(
            self.destroyed || self.textures.is_empty(),
            "RenderTextureManager dropped without explicit destroy()"
        );
    }
}

/// 设备能否按 `optimalTiling` 采样某种格式（含块压缩的 `BLOCK_COMPRESSION` 位）。
///
/// `vkGetPhysicalDeviceFormatProperties` 是纯查询、没有同步问题，所以每次上传都查；
/// 贴图上传本来就很少发生，不值得为此维护缓存。
pub fn texture_format_supported(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
) -> bool {
    let properties =
        unsafe { instance.get_physical_device_format_properties(physical_device, format) };
    // Vulkan 没有「块压缩」特性位：某个格式能不能采样，就是看它自己的
    // `optimalTilingFeatures` 有没有 `SAMPLED_IMAGE`（桌面普遍支持 BC，ASTC/ETC2
    // 只有移动 GPU 有）。
    properties
        .optimal_tiling_features
        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
}

#[cfg(test)]
#[path = "texture_manager_tests.rs"]
mod tests;
