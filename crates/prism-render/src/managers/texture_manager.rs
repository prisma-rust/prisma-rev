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
//! P0 scope 提交 9): the `register` path will be replaced with an
//! end-to-end 图像 upload 图像 + 内存 + 视图 + bindless 写入 in
//! one 调用 using the existing `buffer::create_buffer` + a small
//! one-shot 命令 缓冲区 helper.

use anyhow::Context as _;
use ash::vk;
use slotmap::{new_key_type, SlotMap};

use crate::bindless::{BindlessTextureTable, TextureHandle};
use crate::buffer::create_and_upload_image;
use crate::context::VulkanContext;

// 局部 handle. The engine 层 translates 资源 纹理 handles
// into this when it calls `RenderTextureManager::reserve` so the 渲染
// crate stays decoupled from the 资源 管线
new_key_type! {
    /// Slotmap handle into [`RenderTextureManager`].
    pub struct TextureHandleSlot;
}

/// Backwards-compatible alias for the slotmap-typed handle. 公开 so
/// engine 代码 can name it without depending on the new_key_type
/// expansion directly.
pub type AssetTextureHandle = TextureHandleSlot;

/// Plain-data 纹理 描述 used at the 管理器 boundary. The
/// engine 层 translates 资源 纹理 data into this.
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
    /// sRGB->linear conversion on 样本 so the 着色器 receives 线性 values
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
            TextureFormat::Etc2R8G8B8A8Srgb => 1,
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
            TextureFormat::Etc2R8G8B8A8Srgb => {
                let bw = (width + 3) / 4;
                let bh = (height + 3) / 4;
                (bw as usize) * (bh as usize) * 16
            }
        }
    }

    /// The Vulkan 图像 格式 to use for this 纹理 kind.
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

/// A handle into the 纹理 管理器 plus the bindless SRV 槽 assigned
/// to it. The GPU 图像 / 内存 / 视图 are owned by the 管理器 and freed
/// in 销毁
pub struct UploadedTexture {
    pub srv: TextureHandle,
    /// Width/height are stored here so the 渲染器 can 构建 the 图像
    /// 视图 信息 without keeping the 输入 around.
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    /// Owned GPU objects. Kept so 销毁 can 释放 them; the bindless
    /// SRV 描述符 merely references 视图
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

/// 管理器 of GPU textures. Owns the [`BindlessTextureTable`] and the
/// bindless 槽 for every registered 纹理
pub struct RenderTextureManager {
    bindless: BindlessTextureTable,
    textures: SlotMap<AssetTextureHandle, UploadedTexture>,
    /// 槽 0 of the bindless 表 is reserved for the magenta 回退
    /// and is never reallocated.
    fallback_srv: TextureHandle,
    /// The fallback image view — written into any slot that is released
    /// via `unregister` so a stale handle sampling the freed view hits
    /// magenta instead of a use-after-free.
    fallback_view: vk::ImageView,
    /// 总计 slots the bindless 表 can hold. User textures start at
    /// 槽 1 槽 0 is the 回退 The 总计 is `fallback_capacity +
    /// user_capacity` to keep the math simple.
    #[allow(dead_code)]
    user_capacity: u32,
    destroyed: bool,
}

impl RenderTextureManager {
    /// Construct a new 管理器 with a 1×1 magenta 回退 already in 槽
    /// 0 of the bindless 表
    ///
    /// `user_capacity` is the 最大 number of user textures the 管理器
    /// will accept; the 回退 is allocated *in addition* to this.
    /// The actual Vulkan 图像 + 视图 for the 回退 is a real
    /// 1×1 R8G8B8A8_UNORM 图像 so a missing-texture 着色器 分支
    /// still samples a sensible 像素
    pub fn new(
        context: &VulkanContext,
        command_pool: vk::CommandPool,
        graphics_queue: vk::Queue,
        user_capacity: u32,
    ) -> anyhow::Result<Self> {
        let total = user_capacity + 1;
        let mut bindless = BindlessTextureTable::new(&context.device, total)
            .map_err(|e| anyhow::anyhow!("RenderTextureManager::new: bindless: {e}"))?;

        // Magenta 回退 1×1 不透明 magenta (R=1,G=0,B=1,A=1) in the
        // engine's 线性 working 空间 (the 着色器 applies sRGB→linear on
        // sampled albedo; the 回退 is only a 缺少 纹理 marker so
        // its 精确 颜色 空间 is irrelevant). Written into bindless 槽 0.
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
        // Keep the 回退 GPU objects alive for the manager's 生命周期
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
        // 存储 the 回退 under a dedicated 调 so 销毁 frees it.
        // Its `srv` is fixed at 槽 0 (register_with_handle advanced the
        // table's 下一个 past 0), so user textures start at 槽 1.
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

    /// The bindless SRV 槽 of the magenta 回退
    pub fn fallback_srv(&self) -> TextureHandle {
        self.fallback_srv
    }

    /// Raw bindless 表 — exposed so the 渲染器 can bind the 描述符
    /// 集合 as part of its 管线 setup.
    pub fn bindless(&self) -> &BindlessTextureTable {
        &self.bindless
    }

    /// Mut 访问 to the bindless 表 for the 渲染器 to 写入 回退
    /// 视图 / 图像 视图 creation in 提交 9.
    pub fn bindless_mut(&mut self) -> &mut BindlessTextureTable {
        &mut self.bindless
    }

    /// Upload a CPU-side 纹理 to a device-local 图像 register its 视图
    /// in the bindless SRV 表 and return a handle. The handle maps to
    /// the bindless 槽 via [`get_srv`](Self::get_srv).
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

        // 预留 the 下一个 bindless SRV 槽 槽 0 is the magenta 回退
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

    /// Like [`reserve`](Self::reserve) but records the 图像 upload into a
    /// shared [`BatchUploader`](crate::batch::BatchUploader) so many textures
    /// can be uploaded with a single submit + 围栏 The 调用者 must finish
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

    /// Translate an asset-side 纹理 handle to its bindless SRV 槽
    /// Returns `fallback_srv` (not 无效 when the handle is unknown
    /// so shaders can always 样本 something 可见
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

    /// 释放 every entry (GPU image/memory/view + bindless 槽 The
    /// underlying bindless 表 is dropped when this 管理器 is dropped,
    /// which destroys the 描述符 池 集合 布局 and 4 samplers.
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
