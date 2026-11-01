# Vulkan 渲染循环经验教训

在实现 PrismaRev 里程碑 1（桌面 Vulkan 清屏循环）过程中踩过的坑和学到的经验。

## 1. 工具链损坏要彻底清理

**问题**：`rustup` 状态损坏（manifest 丢失、下载缓存污染、组件标记已安装但二进制缺失）。

**教训**：
- `rustup toolchain install` 显示 "unchanged" 不代表工具链完好，只是 manifest 认为已安装
- Git Bash 的 `rm -rf` 在 Windows 上可能删不掉被占用的文件，要用 `cmd //c "rmdir /s /q ..."`
- 修复步骤：先 `rustup toolchain uninstall`，再清空 `~/.rustup/downloads` 和 `tmp`，最后重装
- 代理设置：`HTTP_PROXY/HTTPS_PROXY=http://127.0.0.1:7897`，rustup 默认后端（reqwest）走 HTTP CONNECT 隧道

## 2. ash 0.38 API 与旧版本差异大

**问题**：照记忆写的 ash API 调用大量编译失败。

**教训**：
- **不要猜 API**，直接读 `~/.cargo/registry/src/` 下的 ash 源码确认签名
- 关键差异：
  - `DebugUtilsObjectNameInfoEXT` 的 `object_type` 是字段不是 builder 方法；`object_handle<T: Handle>()` 是泛型方法
  - `set_debug_utils_object_name` 在 `ash::ext::debug_utils::Device` 上，不是 `Instance`
  - 扩展名常量（如 `vk::EXT_DEBUG_UTILS_NAME`）是 `&'static CStr`，不是 `&str`，不能直接传给 `CString::new`
  - `ash_window::create_surface` 接收 `RawDisplayHandle`/`RawWindowHandle`（值），需要 `.into()` 从 `HasDisplayHandle` 转换
  - `ash_window::enumerate_required_extensions` 返回 `&'static [*const c_char]`

## 3. 设备创建必须启用 swapchain 扩展

**问题**：`Unable to load create_swapchain_khr` panic。

**原因**：创建逻辑设备时没有启用 `VK_KHR_swapchain` 设备扩展，导致 swapchain 函数指针无法加载。

**教训**：实例扩展和设备扩展是两回事。`VK_KHR_swapchain` 是**设备扩展**，必须在 `vk::DeviceCreateInfo::enabled_extension_names` 中启用。

## 4. 交换链重建必须传 old_swapchain

**问题**：重建 swapchain 时报 `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`。

**原因**：`vk::SwapchainCreateInfoKHR` 没有设置 `old_swapchain`，导致实现无法平滑过渡旧交换链。

**教训**：
- 重建时必须把旧 swapchain 传给 `create_info.old_swapchain(old)`
- 创建新 swapchain 成功后再销毁旧的
- 重建要事务化：先创建新的，成功后才销毁旧的资源（view/semaphore），失败时保持旧状态不变

## 5. 信号量同步：per-image 而非 per-frame

**问题**：验证层报信号量重用错误 `VUID-vkQueueSubmit-pSignalSemaphores-00067`。

**原因**：`render_finished` 信号量按 frame-index 轮转，但 `vkAcquireNextImageKHR` 可能连续返回同一 image index，导致同一信号量被两个 present 同时引用。

**教训**：
- `image_available`（acquire 信号量）：按 `MAX_FRAMES_IN_FLIGHT` 轮转，fence 保证复用安全
- `render_finished`（present 等待的信号量）：**按 swapchain image index 索引**，每 image 一个
- 验证层的建议直接照做：`Use a separate semaphore per swapchain image`
- 编辑器窗口的交换链由 `egui-ash` 管理，上游 rev `258aee2` 仍是 per-frame；#581 通过
  `prisma-rust/egui-ash` 的 `prismrev` 分支（commit `6b390d3`）修正后才接入，
  以后升级这个依赖时要重新确认 `src/presenters.rs` 仍然按 `image_index` 取信号量

## 6. 命令缓冲区必须 per-frame-in-flight

**问题**：`vkResetCommandBuffer` 报 "commandBuffer must not be in the pending state"。

**原因**：只有一个命令缓冲区被所有帧共享。帧 A 提交后（pending），帧 B 在未等待帧 A 完成的情况下重置了它。

**教训**：
- 分配 `MAX_FRAMES_IN_FLIGHT` 个命令缓冲区，按 `current_frame` 索引
- fence 等待确保对应帧的命令缓冲区已完成，才能重置和重录

## 7. 帧同步：不要用 image_in_flight 追踪

**问题**：acquire 在第 3 帧永久阻塞。

**原因**：`image_in_flight[image_index]` 被设为 submit fence，但 image 是否可复用取决于 **present 是否完成**，而非 submit 是否完成。`wait_for_fences(image_in_flight[idx])` 等的是 submit fence，不能保证 image 已被 present 释放，形成循环等待。

**教训**：
- 用 vulkan-tutorial 的标准模式：只用 `MAX_FRAMES_IN_FLIGHT` 个 fence 轮转，**不需要 per-image fence 追踪**
- `MAX_FRAMES_IN_FLIGHT=2` + `min_image_count+1=3` 个 swapchain image，始终至少 1 个 image 空闲，acquire 不会阻塞
- fence 等待放在 acquire 开头：`wait_for_fences([current_frame_fence])` -> `reset_fences` -> `acquire`，保证命令缓冲区和 acquire 信号量都可安全复用

## 8. winit 0.30 事件循环模型

**问题**：双击运行时窗口"未响应"，颜色不变。

**原因**：winit 0.30 是事件驱动的，`about_to_wait` 不会自动持续调用。

**教训**：
- 必须调用 `window.request_redraw()` 触发下一帧
- `request_redraw` 放在 `about_to_wait` 里（不是 `RedrawRequested` 回调里），让事件循环在帧之间回到 OS 消息泵
- 渲染逻辑放在 `window_event` 的 `RedrawRequested` 分支中
- 在 Windows 上 FIFO present 需要消息泵运转才能完成，事件循环不呼吸会导致 image 不释放

## 9. 日志在无控制台环境下的问题

**问题**：双击运行时 `env_logger` 写 stderr 可能阻塞。

**教训**：
- 用 `std::io::IsTerminal` 检测是否有控制台
- 无控制台时回退到写文件（`prisma.log`），避免阻塞
- 日志 `format` 闭包里加 `buf.flush()` 确保实时落盘，方便诊断崩溃

## 10. 矩阵乘法与列主序(Column-Major)的陷阱

**问题**：渲染循环跑通了，但屏幕上只显示清屏颜色，完全看不到绘制的物体（所有物体都在裁剪阶段被丢弃）。

**原因**：多个矩阵数学计算由于未考虑列主序（或手误）写错，导致物体坐标映射到了无效的裁剪空间外：
1. **View-Projection 乘法写反**：自定义的矩阵乘法 `C[i][j] += A[i][k] * B[k][j]` 实质上计算的是 `(B * A)^T`。由于 `i` 代表列，`j` 代表行，标准矩阵乘法 `C = A * B` 应该是 `C[i][j] += A[k][j] * B[i][k]`。
2. **投影矩阵 Z/W 元素位置写反**：在列主序矩阵 `proj[col][row]` 中，为了实现 `W_clip = -Z_view`，`-1.0` 应该放在 `col2.w`（即 `proj[2][3]`），而 Z 的平移项应该放在 `col3.z`（即 `proj[3][2]`）。原代码里这两者正好写反了，导致 W 被置为错误的缩放值。
3. **模型矩阵的局部缩放应用错误**：`to_model_matrix` 在把缩放值应用到旋转矩阵时，原本的代码错误地给一行乘上了不同的 sx/sy/sz。对于 `M = T * R * S`，缩放应当在局部坐标系生效，因此旋转矩阵的每个列向量（代表局部 X/Y/Z 轴）应当被整体乘上一个对应的缩放系数。

**教训**：
- **永远要在纸上手动推演一次列主序的二维数组下标**。`m[col][row]` 格式在做 `C = A * B` 时，结果的第 `i` 列是 `B` 的第 `i` 列向量与 `A` 的每一行做点积。
- 在图形 API 中手写底层数学运算极易出错，一个小小的下标错误就会导致黑屏/空屏。建议尽早引入经过测试的第三方数学库（如 `glam` 或 `nalgebra`），如果一定要手写，必须为每个矩阵构建、乘法函数编写严密的单元测试。

## 11. 绕序(Winding Order)与背面剔除的空心错觉

**问题**：场景中的球体光照方向看起来是反的。光源在右上角，但球体似乎是左上角亮。并且当相机转到光源同侧时，球体变成了全黑，而旁边的立方体光照完全正常。

**原因**：球体的三角形顶点索引生成时采用了**顺时针 (CW)** 绕序，而立方体使用的是**逆时针 (CCW)** 绕序。
1. **坐标系与正面判定**：Vulkan 的投影矩阵会翻转 Y 轴（`proj[1][1] = -inv_tan`，这是 Vulkan 的正确写法，非 bug）。这意味着在世界空间（+Y 向上）里是逆时针 (CCW) 的三角形，经过 y 翻转后到了 NDC 会变成顺时针 (CW)（Vulkan 的 NDC y 与 OpenGL 相反：y=+1 对应帧缓冲底部，而非顶部）。
2. **管线状态**：图形管线设置了 `front_face(CLOCKWISE)` 并且开启了 `cull_mode(BACK)`。因此，世界空间里的 CCW 三角形（如立方体）在裁剪空间变成 CW，被判定为“正面”从而保留；而世界空间里的 CW 三角形（如球体）被判定为“背面”从而被剔除。
3. **视觉错觉（空心面具错觉）**：由于球体面向相机的前半个壳被全部剔除，相机实际看到的是球体**背向相机的那半个壳的内侧**！由于内侧顶点的法线是朝向球外的（即背离相机和光源），导致光照计算 `dot(N, L)` 出现完全反直觉的结果，形成光照位置相反、甚至全黑的错觉。

**教训**：
- **统一绕序**：程序生成的所有 3D 几何体（无论是 Cube 还是 Sphere）必须严格遵守统一的绕序（通常约定为世界空间 CCW 为正面）。
- **光照诡异先查法线与剔除**：当发现某个物体的光照方向与其他物体不一致，或者某些角度出现不合理的纯黑时，第一时间检查该物体的绕序、法线方向，或者暂时关闭背面剔除（`CullModeFlags::NONE`）来排除是否渲染了物体的内侧面。

## 总结

Vulkan 渲染循环的核心是**同步原语的正确配对**。最容易出错的地方不是 Vulkan 本身的复杂性，而是同步设计：

| 原语 | 数量 | 索引方式 | 作用 |
|------|------|----------|------|
| acquire 信号量 | `MAX_FRAMES_IN_FLIGHT` | `current_frame` 轮转 | acquire -> submit 等待 |
| render_finished 信号量 | swapchain image 数 | `image_index` | submit -> present 等待 |
| fence | `MAX_FRAMES_IN_FLIGHT` | `current_frame` 轮转 | CPU 等待 GPU 完成命令缓冲区 |

**不要画蛇添足**：标准模式（fence 轮转 + per-image render_finished 信号量）已经足够，不需要 per-image fence 追踪。多余的同步机制反而会引入死锁。

## 12. Vulkan 1.2 下 `VK_KHR_synchronization2` 必须显式启用且用 KHR 包装器

**问题**：`Unable to load cmd_pipeline_barrier2` 非展开 panic（崩在 `create_and_upload_image` 的 `vkCmdPipelineBarrier2`）。

**原因**：
- 实例/设备创建使用 `API_VERSION_1_2`。在 1.2 上核心符号 `vkCmdPipelineBarrier2` **不在 dispatch 表**里，只有 `vkCmdPipelineBarrier2KHR` 可用（该扩展在 1.3 才进核心）。
- 仅 `enabled_extension_names.push(ash::khr::synchronization2::NAME)` **不够**——`ash::Device::cmd_pipeline_barrier2` 只加载核心符号，仍 panic。

**教训**：
- 在 1.2 设备上用到 sync2 barrier，必须把扩展加进 `enabled_extension_names`，并且用 `ash::khr::synchronization2::Device::new(&instance, &device)` 拿到 KHR 包装器来调用。
- 同理 `cmd_blit_image2` 在 1.2 上来自 `VK_KHR_copy_commands2`，也要用对应 KHR 包装器。
- 不要试图把实例升到 1.3 来"取巧"——物理设备可能只支持 1.2，会直接导致 `createDevice` 失败。

## 13. Slang 对 bindless 运行时数组会生成非法 SPIR-V

**问题**：validation 层 `vkCreateShaderModule()` 报 `Invalid explicit layout decorations on type`（`%_runtimearr_XX ArrayStride 8`），VUID-StandaloneSpirv-None-10684。

**原因**：`Texture2D[]` / `SamplerState[]`（`UniformConstant` 运行时数组）被 Slang 错误加上 `OpDecorate <arr> ArrayStride 8`。SPIR-V 校验禁止对"元素是 opaque 类型（image/sampler）的运行时数组"加显式 layout 装饰。该 bug 在 `spirv_1_3`~`spirv_1_6` 所有 profile 上都复现。

**教训**：
- 编译后必须过 `spirv-val`；这种错误在 debug validation 下才会暴露，release 不校验但驱动行为未定义。
- 修复：用 `shaders/fix_spirv.py` 在编译后剥离——只删"元素是 `OpTypeImage`/`OpTypeSampler` 的运行时数组"的 `ArrayStride`；**SSBO / struct 数组的 `ArrayStride` 是合法的，必须保留**。
- 编译脚本（`compile.bat` / `compile.sh`）要把 `fix_spirv.py` 串进 bindless 着色器的产物链。

**手编陷阱（本次新增）**：不要手动调 `slangc` 时加 `-emit-spirv-directly`。该开关绕过了 `fix_spirv.py` 的后处理流程，会直接吐出带非法 `ArrayStride` 的 spv，立刻触发上面的 VUID-10684（`vkCreateShaderModule` 报 `Invalid explicit layout decorations on type for operand '%bindlessSrvs'`）。**统一用 `bash shaders/compile.sh`（或 `run.ps1`，内部调 compile.sh）重编**；手编只用 `compile_stage` 那组参数：`-target spirv -entry <name> -stage <vert|frag> -fvk-use-entrypoint-name`，**不带** `-emit-spirv-directly`。重编后用 `spirv-val.exe <file>.spv` 确认无输出（即无错误）再提交。

## 14. 改了 shader 源文件却没生效？先确认三件事

**问题**：反复改 `scene_bindless.slang` 的 IBL 常量，运行现象纹丝不动（"仍然灰白"）。

**原因（按顺序排查）**：
1. **shader 源文件名/编译脚本错误让 `.spv` 根本没重编**：`compile.sh` 有 `set -euo pipefail`，中途某个 shader 源文件名不对（如 `scene.slang` 实际是 `scene.vert.slang`/`scene.frag.slang`）会让脚本提前 `exit`，**下一步扩展 shader（含 bindless）没被重新生成**，磁盘上还是改之前的旧 `.spv`。profile 名字也要对：当前 slangc 接受 `spirv_1_5`，拒绝旧名 `sspirv_1_5`。
2. **改错了 shader 文件**：sponza 走的是 `GraphRenderer` → `ForwardPass`（`scene_bindless.frag.spv`）直连 swapchain。`GBufferPass`/`LightingPass`/`PostPass` 当前**没被 GraphRenderer 连接**，改 `lighting.slang`/`post.slang` 全是无效功。
3. **`include_bytes!` 是编译期打进二进制的**：必须 `cargo build` 重编 Rust 才能拾取新 `.spv`。

**教训**：
- 改完**必须亲自用 `spirv-dis` 反编译 `.spv`，核对相关 `OpConstant %float` 确实变了**，不能只看 `compile.sh` 打印 "compiled"。
- 改 shader 前先 grep 实际 `include_bytes!` 的 `FRAG_SPV`，以及 `GraphRenderer::render` / `add_pass` 里真正连接的 pass，确认改的是会被执行的 binary。
- "屏蔽某段看还剩什么"是有效的隔离手段（屏蔽 IBL 后灰白消失 → 坐实 IBL 是元凶），但前提是隔离改的是对的文件。

## 15. 色彩空间：SRGB swapchain 会对输出做二次 gamma 编码

**问题**：unlit 直接 `return sampled.rgb`（纹理 sRGB 字节）后，红棕瓦片被提亮成**黄白**。

**原因**：swapchain 是 `B8G8R8A8_SRGB`，Vulkan 会对 shader 输出再做一次 sRGB 编码（gamma 1/2.2）后显示。纹理以 `R8G8B8A8_UNORM`（sRGB 字节）上传，unlit 直接输出 → 被 swapchain 二次编码 → 偏亮偏黄白。
**没有 post 参与**（本次 sponza 路径根本没接 PostPass），"黄白"不能归咎于 post 的 `pow(1/2.2)` —— 那是错误归因。

**教训**：
- unlit 想"所见即纹理"：输出前 `albedo = pow(sampled.rgb, 2.2)`（sRGB→linear），让 SRGB swapchain 编码回去 = 原纹理色。
- **环境光"灰白污染"不是运算符问题**：漫反射 IBL `irradiance * albedo` 本来就是 multiply，正确。真问题是 `irradiance` 是个**恒定灰色常量**（如 0.06~0.12 灰），作为独立 add 项叠加，背光面只剩这层灰白。正确做法是从 `envCube` 采样真实辐照度（带环境色调），而非一坨灰常量。
- **纹理格式应区分 sRGB / linear（根基性修复，仍待办）**：albedo / emissive 是 sRGB 数据，应上传为 `R8G8B8A8_SRGB`（采样自动转 linear，光照在 linear 空间算，输出 SRGB swapchain 编码回去）；normal / metallic-roughness 是 linear 数据，必须保持 `R8G8B8A8_UNORM`。当前所有纹理统一 `UNORM` 上传，是 PBR 色彩正确的根本缺陷，也是那套 gamma hack 的来源。

## 总结（补充）

本次里程碑暴露的问题集中在**扩展/校验/构建链路/色彩空间**四类，而非同步原语：

| 类别 | 关键坑 | 验证手段 |
|------|--------|----------|
| 扩展加载 | 1.2 上 sync2/copy2 要显式启用 + KHR 包装器 | 崩溃栈指向 `load_erased` |
| SPIR-V 合法性 | Slang 给 opaque 运行时数组加非法 `ArrayStride` | `spirv-val` 报 VUID-StandaloneSpirv |
| 构建链路 | 编译脚本中途失败导致 `.spv` 没重编；改错文件 | `spirv-dis` 反编译核对常量 |
| 色彩空间 | SRGB swapchain 二次 gamma 编码；IBL 灰常量 | unlit 输出 + 隔离 IBL |
| 路径确认 | sponza 走 ForwardPass 直连 swapchain，无 post | grep `include_bytes!` + `add_pass` |

**先确认实际路径，再改代码**：所有"改了没生效 / 归因错"都源于先入为主假设了渲染路径。改 shader 前先确认它确实被编译、被连接、被二进制包含。

## 16. 阴影正交投影的 z 映射 + near plane 必须自洽（否则条带/消失）

**背景**：`DirectionalLight` 的 `light_direction` 在 shader 里是 **direction TO the light**（`dot(n, light_dir)` 受光，见 `scene_bindless.slang` 453/455 行），正交阴影投影用 Vulkan 的 `[0,1]` 深度。

**踩过的三个坑（按出现顺序）**：

1. **z 映射公式错 → 远处饱和成条带**：原 `proj[2][2]=-0.5/dist, proj[2][3]=0.5` 把 `view_z∈[-2*dist,0]` 映射到 `[1.5,0.5]`，远端 `clip.z>1` 被裁/饱和到 1.0，地面出现规则条带（shadow acne 类）。正确 Vulkan 0..1 正交：
   ```rust
   // view_z = -z (z 为距光正距离, ∈[n,f]); near=n, far=f
   proj[2][2] = -1.0/(f-n);  proj[2][3] = -n/(f-n);
   ```
2. **near = dist 把原点裁掉 → 阴影完全消失**：`dist = half*2`，光在距原点 `dist` 处，`near=dist` 等于把 near plane 放在原点 → 原点及周围全在 near 之前被裁 → shadow map 保持清空的 1.0 → `SampleCmpLevelZero` 永远返回 lit。必须把 `near` 设得**小于**原点距离，例如 `n = 0.5*dist`（原点落在 depth≈0.17），`f = 3*dist` 覆盖背面几何。
3. **x/y 半宽太小 → 阴影只在中心一小块**：正交半宽用传入的 `half`(=12) 只有 24×24，多数场景几何落在 `uv` 外被当成受光。半宽取 `dist`(=24) 覆盖 48×48。

**教训**：正交阴影投影的 near/far 围绕"光到原点的距离"取值，且 near 必须留余量；z 映射用标准 0..1 正交而非手写缩放。改完用 `spirv-val` + 实跑双确认（条带=精度/偏置，消失=near 裁切，小块=半宽不足）。

## 17. 阴影相机眼位置符号：eye = +direction_to_light（不是 -）

**问题**：把光源眼放在 `-l*dist`（背光侧）看向原点，等于从**暗面**渲染深度图，阴影整体投到受光的错误一侧 —— 表现为"全部错位"。

**定位依据**：`scene_bindless.slang` 第 455 行 `n_dot_l = max(dot(n, light_dir), 0.0)` 证明 `light_dir` 是"指向光源的方向"，所以光源在 `+l` 侧。阴影相机应放在 `+l*dist` 看向原点：
```rust
let eye = [l[0]*dist, l[1]*dist, l[2]*dist];  // NOT -l*dist
```
**教训**：判定光源方向别靠"看起来应该朝向哪"，直接查 shader 里 `light_dir` 的真实用法（受光点积还是光线传播方向）。方向反了是"整体错位"而非"消失"，容易和 z 映射问题混淆——两条独立排查。

## 18. Vulkan 下 shadow map 采样**不要**翻转 Y

**问题**：`sample_shadow` 里 `uv.y = 1.0 - (proj.y*0.5+0.5)`，导致阴影在垂直方向整体镜像偏移（"有阴影但错位"）。

**根因**：shadow map 用**正交投影且没做 y-flip**（`proj[1][1]=+inv`，不同于主相机透视的 `p[1][1]=-inv_tan` Vulkan y-flip）。Vulkan framebuffer 原点在左上，NDC y=-1 已映射到纹理 v=0，**渲染时 `proj.y` 直接对应 `v=proj.y*0.5+0.5`，无需翻转**。occluder（写深度）和 receiver（采样）用同一个 `lightViewProj`，`proj.y` 计算相同，必须查**同一 texel**：
```hlsl
float2 uv = float2(proj.x * 0.5 + 0.5, proj.y * 0.5 + 0.5);  // 去掉 1.0 - 翻转
```
X 方向自始没有翻转，本就正确，所以错位只在垂直方向——与现象吻合。

**教训**：shadow map 的 Y 翻转不能照搬"GL 习惯"。判定要不要翻：看生成 shadow map 的投影**有没有** y-flip。有（主相机类）→ 采样侧翻；没有（正交光投影）→ 采样侧不翻。

## 19. 调试阴影的优先级顺序（避免来回打转）

按这个顺序排查，每步一个变量：
1. **先确认阴影被启用**：默认 `debug_flags` 必须含 `PBR_FLAG_SHADOW`(bit8) 且 `PBR_FLAG_DIRECT`(bit0) 也开，否则没有"直接光"可被遮挡，开阴影也看不见变化（本次默认 `0` 即纯 baseColor，是"开了没反应"的真因）。
2. **阴影全无 vs 全错位 vs 垂直偏移 vs 条带** 各自对应不同根因：
   - 全无/只在中心小块 → near 裁切 / 半宽不足（§16）
   - 全部反向错位 → eye 符号（§17）
   - 垂直镜像偏移 → 采样 Y 翻转（§18）
   - 规则条带 → z 映射饱和 + 深度偏置太小（§16 + §20）
3. **深度偏置**：D32_SFLOAT 下 `depth_bias_constant_factor` 乘格式最小可表示差（≈2^-23），`1.0` 实际≈0，地面大平面必出 acne 条带。用 `constant_factor≈64, slope_factor≈8`（可据 acne/peter-panning 微调方向）。

**横向经验**：阴影链路（方向推导 → 投影 z → near/far → 半宽 → eye 符号 → 采样 Y 翻转 → 深度偏置）每一环独立，改一处只验证一处现象，别同时动多个变量。


## 20. MRT 改动必须同步改所有"在同一 render pass 里绘制的"pipeline 的 blend state

**问题**：ForwardPass 从 1 个 color attachment 改成 2 个（HDR color + view-space normal MRT）后，验证层报：
```
vkCreateGraphicsPipelines(): pCreateInfos[0].pColorBlendState->pAttachments[1] is different
than pAttachments[0] and independentBlend feature was not enabled.
VUID-VkPipelineColorBlendStateCreateInfo-pAttachments-00605
```

**原因**：SkyboxPass 和 Gizmo 都在 ForwardPass 的 render pass 里绘制（`ForwardPass::execute` 先画 skybox，最后画 gizmo）。它们各自创建 pipeline 时只声明 1 个 blend attachment（`color_attachment_count: None` -> 默认 1），但 render pass 的 subpass 现在有 2 个 color attachment。Vulkan 规定：pipeline 的 `VkPipelineColorBlendStateCreateInfo::attachmentCount` 必须等于 subpass 的 `colorAttachmentCount`。原来 1 个 attachment 时凑巧一致；改 MRT 后就破了。

**教训**：
- **render pass 的 attachment 数是所有"在其中绘制的"pipeline 的公约数**。改 ForwardPass 的 attachment 数，必须同步改 SkyboxPass、Gizmo 的 blend state 数，哪怕它们只写 attachment 0（attachment 1 的 `colorWriteMask = 0` 即可"预留不写"）。
- 不想每个 pipeline 都写满全量 blend state 的话，启用 `independentBlend` feature（`PhysicalDeviceFeatures::independent_blend`，桌面+现代 Android 通用支持）就能让每个 attachment 用不同 blend config。本项目选了"启用 feature + 各 pipeline 写满"双保险。
- `GraphicsPipeline::new`（`pipeline.rs`）原本只支持 0/1 个 attachment（`slice::from_ref`），为 MRT 扩展成接受 `color_blend_attachments: Option<&[...]>` 切片。

## 21. `vkCmdBeginRenderPass` 的 `clearValueCount` 必须 >= 最大被 CLEAR 的 attachment index + 1

**问题**：ForwardPass 改 3 attachment（color + depth + normal）后报：
```
vkCmdBeginRenderPass(): pRenderPassBegin->clearValueCount is 2 but there must be at least
3 entries... VUID-VkRenderPassBeginInfo-clearValueCount-00902
```

**原因**：`execute` 里 `clear_values` 数组只放了 `[color, depth]` 2 个，但 attachment 2（normal）也是 `LOAD_OP_CLEAR`。clear values 按 **attachment number** 索引，所以即使中间某个 attachment 不是 CLEAR，只要最高 index 的 CLEAR attachment 是 N，数组就得有 N+1 个元素（中间非 CLEAR 的位会被忽略）。

**教训**：改 render pass 的 attachment 数后，立刻同步改 `execute` 里的 `clear_values` 数组长度，且顺序严格对应 attachment number。clear value 本身无所谓（normal attachment 被 fragment shader 全覆盖，clear 值用啥都不影响），但 count 必须对。

## 22. 采样 depth attachment 必须给 image 加 `SAMPLED` usage

**问题**：GTAO pass 采样 ForwardPass 的 D32_SFLOAT depth 时报：
```
vkUpdateDescriptorSets(): pDescriptorWrites[0].pImageInfo[0].imageView was created with
VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT, but descriptorType is VK_DESCRIPTOR_TYPE_SAMPLED_IMAGE.
VUID-VkWriteDescriptorSet-descriptorType-00337
```

**原因**：`DepthImage::new`（`render_pass.rs`）创建 depth image 时 usage 只写了 `DEPTH_STENCIL_ATTACHMENT`。原来 depth 只用于场景内的深度测试（`STORE_OP_DONT_CARE`），从不被采样。GTAO 要读它，必须加 `SAMPLED` usage。

**教训**：
- Vulkan image 的 usage flags 在 **创建时** 固定，事后不能加。任何"既要当 attachment 又要被采样"的 image（depth、normal MRT、HDR color），创建时 usage 就要 `| SAMPLED`。
- 同理 GTAO 要采样 normal MRT，`NormalImage::new` 的 usage 直接写 `COLOR_ATTACHMENT | SAMPLED`；PostPass 要采样 HDR color，ForwardPass 的 color image 也是 `COLOR_ATTACHMENT | SAMPLED`（复用 `NormalImage` helper）。
- depth attachment 改成 `STORE_OP_STORE`（原来是 `DONT_CARE`）才能把内容保留到 pass 结束后被采样。

## 23. 每帧重写的 descriptor set 必须 per-frame-in-flight，不能全局共享

**问题**：ForwardPass 的 AO descriptor set（set 4，每帧指向上一帧 GTAO 输出）和 PostPass 的 HDR-input descriptor set 都报：
```
vkUpdateDescriptorSets(): dstSet is in use by VkCommandBuffer... in the pending state.
VUID-vkUpdateDescriptorSets-None-03047
```

**原因**：最初两个 pass 都只分配 **1 个** descriptor set，每帧 `set_ao`/`set_input` 重写它指向新 view。但 frame N 提交后还在 GPU 跑（pending），frame N+1 就 update 了同一个 set -- 验证层判定"修改了 in-flight command buffer 正在用的 set"。

**教训**：
- **任何每帧 `vkUpdateDescriptorSets` 重写的 descriptor set，都要按 frame-in-flight 分配 N 份**（本项目 N=2）。frame N 只更新 `sets[N]`，fence 等待保证该 set 不再被 GPU 使用。
- 对比：shadow map descriptor set（ForwardPass set 3）从不重写（view 在 `set_resources` 里固定一次），所以 1 份够用。判断标准是"这个 set 的 binding 会不会在运行期被 `update_descriptor_sets` 改"。
- GTAO pass 的 depth/normal input descriptor 也是每帧重写，按 `[frame][set]` 二维分配（4 份：2 frame x 2 set，因为 shader 声明了 set 0 depth + set 1 normal）。

## 24. 新创建的 image 在被 descriptor 引用前必须先 transition 到声明的 layout

**问题**：GTAO 的 AO image 在 `new` 里创建后，第一帧 scene shader 的 AO descriptor 就指向它（`ao_view((frame+1)%2)`），但 AO image 此刻还是 `UNDEFINED` layout，而 descriptor 声明的是 `SHADER_READ_ONLY_OPTIMAL`。报：
```
vkQueueSubmit(): command buffer expects VkImage ... to be in layout SHADER_READ_ONLY_OPTIMAL
-- instead, current layout is UNDEFINED. VUID-vkCmdDraw-None-09600
```

**原因**：第一帧 GTAO 还没运行（scene -> gtao -> post 顺序里 gtao 在 scene 之后），但 scene 已经在采样"上一帧"的 AO（即 `ao[(frame+1)%2]`，frame 0 时是 `ao[1]`，从未被写过）。descriptor 的 layout 声明和 image 实际 layout 不匹配，验证层即使该 binding 没被 shader 真正采样（`PBR_FLAG_AO` 默认 off）也会报。

**教训**：
- **跨帧依赖的 image（上一帧写、本帧读）创建后立刻 transition 到"读"layout**，让 descriptor 声明成立。GTAO 在 `new` 和 `recreate_target` 里用一次性 command buffer 把两个 AO image 从 `UNDEFINED` 转到 `SHADER_READ_ONLY_OPTIMAL`。
- 被转的 image 下一步扩展被 GTAO render pass 写时，render pass 的 `initial_layout = UNDEFINED` 容忍任何 incoming layout（配合 `LOAD_OP_CLEAR`），所以预 transition 不会和下一步扩展写入冲突。
- 对比：PostPass 读的 HDR color image 不需要预 transition -- 它总是被 ForwardPass 先写（`COLOR_ATTACHMENT_OPTIMAL`）再被 PostPass 读，第一帧也是 scene 先跑。只有"读"发生在"写"之前的跨帧 image 才需要预 transition。

## 25. Slang 没有 `gl_FragCoord`，用 `SV_Position` input

**问题**：GTAO shader 里写 `float2(gl_FragCoord.xy) / pc.viewport`，slangc 编译报 `error 30015: undefined identifier 'gl_FragCoord'`。

**原因**：Slang/HLSL 习惯用 `SV_Position` 语义拿像素坐标，不暴露 GLSL 的 `gl_FragCoord` 内置变量。fragment shader 的 `clipPos : SV_Position` input 就是像素坐标（`xy` = framebuffer pixel coords，左上原点）。

**教训**：Slang shader 里拿像素坐标一律用 `SV_Position` input（`float4 clipPos : SV_Position`，`clipPos.xy` 即像素坐标），不要写 `gl_FragCoord`。fullscreen-triangle pass 的 vertex stage 已经输出了 `SV_Position`，fragment stage 直接接即可。

## 26. push constant 大小不必是 16 的倍数

**问题**：GTAO push constant struct 算出来 84 字节（`inv_proj`(64) + `viewport`(8) + `radius`(4) + `mode`(4) + `_pad0`(4)），最初注释写"round up to 96 for 16-byte alignment"，测试断言 `size_of == 96` 失败（实际 84）。

**原因**：误解了 Vulkan push constant 的对齐要求。Vulkan 规定 push constant range 的 `size` 只需 >= shader 实际读取的字节数，**不要求是 16 的倍数**（也不要求 4 的倍数，虽然实践上按 4 对齐）。Rust `#[repr(C)]` 也不加尾部 padding，所以 84 就是 84。

**教训**：
- push constant 的 Rust mirror 和 Slang struct 必须**字节对字节**匹配，包括尾部 padding。slangc 对 struct 也是紧凑布局（不加尾部 align padding），和 `#[repr(C)]` 一致。
- 测试断言写实际算出来的字节数（84），不要写"对齐后的"96。Vulkan 保证 push constant 至少 128 字节可用，84 远低于这个限制，没问题。
- 对比 UBO（std140）则**必须** 16 字节对齐（`FrameUBOData` 256 字节，尾部 `_pad: [u32; 3]` 就是补齐用的）。两者规则不同，别混。

## 27. 跨 pass 读 scene 输出：上一帧 AO x 本帧 IBL 的 1 帧延迟模式

**背景**：GTAO 必须在 ForwardPass 之后跑（它要读 scene 的 depth+normal），但 IBL 是在 ForwardPass 内部算的。如果等 GTAO 算完再回 scene 重画 IBL，要么死锁要么多遍。

**方案**：GTAO 每帧产出 AO 写到 `ao[frame]`，scene 读 `ao[(frame+1)%2]`（上一帧 GTAO 的输出）。1 帧延迟，游戏通用做法。

**教训**：
- **跨 pass 数据依赖且有时序矛盾时，1 帧延迟是标准解法**。双缓冲 AO image（按 frame-in-flight），scene 读旧帧、GTAO 写新帧，互不干扰。
- 镜头快速移动时 AO 会"拖影"一帧 -- 可接受；下一步扩展可加 temporal filter 抹平。
- 半分辨率 GTAO + scene 用 linear sampler 上采样到全分辨率，GTAO 本身是低频信号，轻微模糊可接受。遵循 `DESIGN.md` 2.1 mobile-first（heavy pass 半分辨率，RayQuery 已有 `scale=0.5` 先例）。

## 28. ForwardPass 改渲染到中间 HDR target + 拆 PostPass：比想象中改动大

**用户要求**："拆出 postpass，把 reinhard/aces 放在 postpass"。看似只是把 tonemap 从 `scene_frag.slang` 挪到新 pass，实际牵一发动全身：

- ForwardPass 不再直渲 swapchain，改成渲染到 `R16G16B16A16_SFLOAT` HDR 中间 target（每 swapchain image 一个，类似 depth image）。
- `set_target` 签名变（不再接 `swapchain_views`，改接 `image_count`），framebuffer attachments 从 `[swapchain_view, depth]` 变 `[hdr_view, depth, normal_view]`。
- `color_format` 字段从 swapchain 格式变成 HDR 格式。
- PostPass 新增：own render pass（swapchain 格式，`initial_layout = UNDEFINED` 容忍上一帧的 PRESENT_SRC_KHR）、per-swapchain-image framebuffer、per-frame-in-flight descriptor set（读 HDR view）、tonemap push constant。
- `GraphRenderer::render` 帧流程从 `scene -> egui/barrier` 变成 `scene -> gtao -> post -> egui/barrier`，swapchain 的 `COLOR_ATTACHMENT_OPTIMAL -> PRESENT_SRC_KHR` 转移点从 ForwardPass 后挪到 PostPass 后。

**教训**：
- **"把 X 拆到新 pass"往往伴随"原 pass 的输出目标改变"**，因为新 pass 要读原 pass 的输出作为输入 texture，原 pass 就不能再直写最终目标（swapchain）。先评估"原 pass 的 render target 要不要换成中间 target"，再动手。
- 中间 target 的生命周期要和 swapchain 同步（`drop_target` / `recreate`），不能 leak。每 swapchain image 一个中间 target（不是每 frame-in-flight），因为 framebuffer 按 image index 索引。
- tonemap 拆出去后，scene shader 输出的是 linear HDR；swapchain 是 sRGB，PostPass 写 swapchain 时 sRGB 编码自动发生（Vulkan `B8G8R8A8_SRGB` 格式）。不要在 PostPass 里手动 `pow(1/2.2)`，那会二次编码。

## 29. 验证层报错优先级：先修"创建时"错误，再修"运行时"错误

GTAO+PostPass 集成时一次性暴露了 5 个验证错误，按修复顺序：

1. **`independentBlend` feature 未启用**（pipeline 创建时）- 改 `context.rs` 启用 feature
2. **`clearValueCount` 不足**（render pass begin 时）- 改 `execute` 的 clear_values 数组
3. **depth image 缺 `SAMPLED` usage**（descriptor 写入时）- 改 `DepthImage::new` 的 usage flags
4. **descriptor set 被 in-flight command buffer 占用**（descriptor 写入时）- 改成 per-frame-in-flight 分配
5. **AO image layout UNDEFINED 与 descriptor 声明不符**（queue submit 时）- 创建后预 transition

**教训**：
- 验证错误按 Vulkan 调用顺序报（创建 -> 记录 -> 提交），**前面的错误可能掩盖后面的**。修一个跑一次，别一次性猜所有错。
- 错误 4（in-flight descriptor）最初被错误 3（usage 不匹配）的 noise 掩盖 -- 修完 3 才看清 4 是独立问题。每个 VUID 独立排查，别假设"都是 descriptor 引起的"。
- `RUST_LOG=info,prism_render=trace` + 日志写文件（`target/debug/prisma.log`，main.rs 的 logger 在非终端时自动落盘）是验证渲染管线的标准手段：grep `validation|ERROR|VUID` 计数，grep `ForwardPass:|GtaoPass:|PostPass:` 确认各 pass 都在执行。

## 30. CI 漂移 / 工具链类教训（本次修 CI 全绿踩的坑）

**背景**：一次 `git pull` 拉进大量新 shader 源（新增 `gtao.slang` / `post.slang` / `skybox.slang`，重命名 `mesh->mesh_vert`、`bindless->scene_frag`、`shadow->shadow_depth`），但没人跑 `shaders/compile.sh` + `xtask shader-bindgen` 重新生成产物，导致 CI 的 5 个 job 全红。修完一共 4 个 commit。

### 30.1 lint 类（fmt + clippy）

- **`cargo fmt --all -- --check` 必须过**：pull 进来的代码若不符合当前 rustfmt 版本，`cargo fmt --all` 会改动一批文件（即使不是你逻辑改的），CI 第一步就挂。本地先 `cargo fmt --all` 再提交。
- **clippy 以 CI 的 rustc 版本为准，不是本地**：CI 用最新 stable，本地 Termux 的 rustc 可能旧一两个小版本，会漏掉新 lint。本次 `crash_dialog_linux.rs` / `macos.rs` 的 `std::io::Error::new(ErrorKind::Other, ...)` 触发 `io_other_error` lint（CI 新版 rustc 才有），本地 1.96 不报。**本地复现不到的 lint 只能靠 CI 迭代**。根因修法：`ErrorKind::Other` → `std::io::Error::other(...)`。
- **clippy `-D warnings` 下所有 `too_many_arguments`（阈值 7）必须重构，不要用 `#[allow]`**：本项目约定修根因。把 8+ 参数拆成参数分组 struct（`barrier2` 的 `ImageBarrier` + `color_subresource`；`transition_image_single` 收 `vk::ImageSubresourceRange`；`RenderGraph::execute` 收聚合的 `&RenderContext`）。
- 其他常见 clippy 根因修法（均已落）：`and_then(|x| Some(y))` → `map`；`impl Default` 可派生就 derive + `#[default]`；`x as usize` 当 `x: usize` 时去掉 cast；`bits = (bits<<16)|(bits>>16)` → `bits.rotate_right(16)`；过高精度 float 字面量截断；`clone()` 在 Copy 类型上去掉；`to_json(&self)` 对 Copy 类型改 `to_json(self)`；`write!(..,"{}\n")` → `writeln!`；可合并的 `if let` 合并；显式无用 lifetime 省略。

### 30.2 shaders job 环境依赖

- **`shaders/compile.sh` 依赖 `spirv-tools`（`spirv-dis` / `spirv-as`），CI 的 shaders job 之前没装**：`fix_spirv.py`（剥离 bindless 运行时数组的非法 `ArrayStride`，见 §13）调用 `spirv-dis` 反汇编，缺工具直接 `FileNotFoundError` 崩溃。修复：在 ci.yml 的 shaders job 加 `sudo apt-get install -y spirv-tools`。
- **`shaders/compile.sh` 只在 `scene_frag` 上跑 `fix_spv`**（仅 bindless 那个 shader 有 ArrayStride 问题）。其余 shader 不调 `fix_spirv`，不需要 spirv-tools。

### 30.3 drift check：改了 shader 源必须重生成并提交产物

- CI shaders job 末尾有 **drift guard**：跑 `shaders/compile.sh` 重新编译 `.spv` + reflection JSON，再 `xtask shader-bindgen` 重新生成 `crates/prism-render/src/shader_bindings.rs`，然后 `git diff --quiet` 比对仓库里已提交的 `.spv` 和 `shader_bindings.rs`。**任何不一致 → `exit 1`**。
- **规则：动了 `shaders/slang/*.slang`，必须本地跑 `bash shaders/compile.sh` + `cd xtask && cargo run --bin shader-bindgen -- ../shaders/reflection ../crates/prism-render/src/shader_bindings.rs`，把生成的 `.spv` / `reflection/*.json` / `shader_bindings.rs` 一起 commit**。否则 CI 必红。
- **Termux 本地跑不了 `shaders/compile.sh`**：slangc 官方预编译是 glibc 二进制，Termux（bionic）无法运行；且 `.spv` 是 `include_bytes!` 进二进制的，必须和 CI 用**同版本 slangc**（当前 `2026.13.1`）生成才一致。本次因本地无 slangc，借 **CI artifact**（drift step 前的 `Upload compiled SPIR-V + reflection (debug)` 已上传重新编译的 `.spv` 和 reflection JSON）取回正确产物：下载 artifact 的 `.spv` + `reflection/*.json`，本地只跑 `xtask shader-bindgen`（它只吃 reflection JSON，不需要 slangc）生成 `shader_bindings.rs`，覆盖后提交。
- **`.spv` 和 `shader_bindings.rs` 必须同时更新**：只更新一个，drift 仍挂（drift 同时检查两者）。

### 30.4 环境 / 操作

- **Termux 下 `rm -rf` 被系统禁止**：清理目录用 `python3 -c "import shutil; shutil.rmtree(path)"` 或 `rmdir`（空目录），不要用 `rm -rf`。单文件 `rm` 无 `-r` 正常。
- **crates.io 直连 TLS 抖动**：Termux 下 `static.crates.io` 下载偶发 `SSL connect error`。配置 `~/.cargo/config.toml` 走国内镜像（当前 `rsproxy`：`registry = "sparse+https://rsproxy.cn/index/"`）稳定。
- **CI 失败先抓 `gh run view <id> --log-failed`**：GitHub API 偶发 503，用重试循环抓；失败 job 的真实错误在 `--log-failed` 末尾（`##[error]Process completed with exit code 1` 上方）。本次链路：`lint(clippy)` → `shaders(spirv-tools 缺失)` → `shaders(drift: shader 产物陈旧)` 三层，逐层迭代修。

## 31. 场景加载耗时拆解 + 冗余拷贝消除（Sponza 7.6s -> 1.8s）

**背景**：Sponza 加载日志只有一行 `gltf parse+import: 5430ms` 和 `texture upload: 1498ms`，看不出时间花在哪。先加阶段计时定位，再修最大的冗余拷贝。

### 31.1 先拆日志，再动手优化

**做法**：在 `gltf_loader::load` 按阶段插 `Instant::now()`，把 `parse+import` 拆成 parse JSON / import buffers / decode images / materials / textures / meshes / scene nodes 七段；在 `load_demo_scene` 把 `texture upload` / `mesh upload` 各拆成 collect（clone scene_store 数据）+ upload（register_*_into）两段，并统计像素/顶点总量。

**拆出来的真相**（Intel Sponza 2022 测试资产：72 张 4K 纹理 = 1208 MP，PNG 磁盘 1.89 GB → RGBA8 解压 4.50 GB；405 mesh / 2M verts）。注意这套资产**不代表真实游戏负载**，见 §31.7。

| 阶段 | 耗时 | 占比 |
|------|------|------|
| `gltf phase: decode images` | 713ms | 14% |
| **`gltf phase: textures`** | **4406ms** | **84%** ← 大头 |
| `gltf phase: meshes` | 86ms | 2% |
| `texture upload collect` (clone) | 543ms | - |
| `texture upload` (per-tex pixels.clone) | 1001ms | - |
| `batch upload submit+wait` (GPU) | 540ms | - |

**教训**：聚合日志（"X 总耗时 Nms"）对定位瓶颈没用。任何超过 ~200ms 的阶段都要再拆一层，并带上规模指标（纹理总兆像素 / 顶点数 / 索引数），这样才能区分"数量多"还是"单资源大"。本次 `gltf phase: textures (72 images, 1208.0 MP)` 一行就立刻暴露 4.5 GB 像素数据是被反复拷贝的。**另一个坑**：不能拿磁盘上的 PNG 大小（1.89 GB）估算内存拷贝开销 -- PNG 解压成 RGBA8 后膨胀 ~2.4×，真正的内存压力看解压后的像素字节数（1208M × 4 = 4.5 GB）。

### 31.2 `to_rgba8` 在做无意义的全量拷贝（4406ms 的根因）

**问题**：`import_images_parallel` 已经把每张图转成 `R8G8B8A8`（`dyn_image.to_rgba8()`），但 `to_rgba8(&image)` 又对这 4.5 GB 做了三遍：`image.pixels.clone()`（4.5 GB 读 + 4.5 GB 写）+ `Vec::with_capacity(count*4)`（4.5 GB 分配）+ 逐 4 字节走 `expand_to_rgba` 函数拷贝（4.5 GB 写，非 memcpy）。≈ 18 GB 内存吞吐，在 ~3.3 GB/s 带宽下正好 ≈ 5.4s，和观测的 4406ms 吻合。

**修复**（`crates/prism-asset/src/gltf_loader.rs`）：
- `to_rgba8` 改为按值接收 `GltfImageData`（`&GltfImageData` -> `GltfImageData`），调用处 `images.iter()` -> `images.into_iter()`。
- RGBA8 源格式走快路径：`return Ok(RgbaPixels { pixels: image.pixels })`，零拷贝 move。
- 非 RGBA8 源格式走慢路径，**打 `log::warn!`** 报告分辨率 + 格式 + 通道数 -- 我们自己的并行 import 永远产 RGBA8，命中慢路径说明有别的代码路径喂数 RGBA8 以外的格式，必须可见。
- 效果：`gltf phase: textures` 4406ms -> **0ms**。

**教训**：转换函数如果"输入已经是目标格式"，必须走零拷贝 move，不能无条件 clone + 逐元素处理。尤其是像素/顶点这种 GB 级数据，一次冗余拷贝就是秒级开销。代码里的 `image.pixels.clone()` + `Vec::with_capacity` + `chunks_exact` 三连是典型的"安全但浪费"写法，发现规模指标（1208 MP）后立刻能定位。

### 31.3 上传路径两处冗余 clone（collect + per-tex pixels.clone）

**问题**：`load_demo_scene` 的纹理上传循环做了两次 4.5 GB 克隆：
1. `texture_data: Vec<_> = scene_store.textures().map(|(h, d)| (h, d.clone())).collect()` -- 543ms
2. 循环里 `pixels: data.pixels.clone()` 进 `TextureUploadInput` -- 又一次

**修复**（`crates/prism-engine/src/app.rs` + `crates/prism-asset/src/scene_store.rs`）：
- 给 `SceneStore` 加 `textures_mut() -> impl Iterator<Item = (TextureHandle, &mut TextureData)>`。
- 上传循环改成 drain 模式：`std::mem::take(&mut data.pixels)` 直接把像素 buffer move 进 `TextureUploadInput`，store 里留空 `Vec`。上传后 CPU 侧像素本就是死重量（GPU 有 device-local 副本），drain 安全。
- 借用坑：`for (h, data) in self.scene_store.textures_mut()` 持有 `&mut scene_store.textures`，循环体内不能再调 `self.scene_store.take_texture_pixels(h)`（二次可变借用）。正解是直接从迭代器给的 `&mut TextureData` 上 `mem::take(&mut data.pixels)`，不需要额外 accessor -- 所以最终没留 `take_texture_pixels` 方法，只留 `textures_mut`。
- 先把 `(asset_h, TextureUploadInput)` 收集进本地 Vec，让 `&mut self.scene_store` 借用在调 `&mut self.renderer` 之前结束，避免 self 借用冲突。
- 效果：`texture upload collect` 543ms -> **0ms**，`texture upload` total 1545ms -> **330ms**。

**教训**：
- **上传后不再被读的 CPU 侧数据应该 drain，不是 clone**。判断标准：grep 这个 store accessor 的所有调用点，如果上传后只剩 `.count()` 之类的元数据查询（本项目 `scene_store.textures()` 上传后只在汇总日志里 `.count()`），drain pixels 就是安全的。
- 借用冲突时优先"从已有的 `&mut` 引用上直接操作"，不要为了对称再加一个 `take_xxx(handle)` accessor -- 后者会触发"同一结构的二次可变借用"。`textures_mut` + `mem::take` 是 Rust 里 drain slotmap 表的标准模式。
- `TextureUploadInput` 持有 `pixels: Vec<u8>` 所有权是合理的（`reserve_into` 只借一次做 staging map），所以 move 进去后无需改 manager 签名。

### 31.4 mesh 上传路径没动（刻意）

`mesh upload: collect=23ms upload=147ms` 已经很小（2M verts ≈ 100 MB，clone 成本可忽略），且 mesh 数据上传后仍可能被 scene_store 读取（不像纹理那样明确 drain 安全）。**刻意不优化**：优化的第一条原则是先看规模指标，别对 23ms 的阶段动刀。

### 31.5 修复后瓶颈分布（~1.8s，可接受）

```
gltf parse+import:        805ms  (45%)
  └ decode images:        701ms  (39%)  ← PNG/JPEG 解码，rayon 已并行
  └ import buffers:        23ms
  └ meshes:                74ms
batch upload submit+wait: 522ms  (29%)  ← GPU 实际 PCIe 传输
texture upload (CPU):     330ms  (18%)  ← 72 次 vkCreateImage + alloc + staging map
mesh upload (CPU):        168ms   (9%)
```

剩下的大头已经是真活儿，不是冗余拷贝。

### 31.6 下一步扩展优化方向（未做，按性价比排序）

0. **GPU 块压缩上传（最高性价比，根治内存）**：当前所有纹理按 `R8G8B8A8_UNORM` 上传，4.5 GB device-local + 4.5 GB host staging 是这套资产内存爆的根本原因（§15 已记录这是 PBR 色彩正确性缺陷）。改成 BC7（0.5 bytes/pixel）上传后：GPU 纹理 ~1.1 GB，staging ~1.1 GB，CPU 峰值砍到 ~2.3 GB。需要：(a) glTF 加载阶段对 RGBA8 做 BC7 编码（CPU 侧，可用 `bc7enc` crate）；(b) `TextureUploadInput` / `BatchUploader::upload_image` 支持 BC7 格式 + 不再生成 mip blit（BC 压缩后再做 blit 要重新编码）。**这条比下面的 decode images 优先级更高**，因为它解决的是内存（GB 级），不只是加载时间（秒级）。注意 albedo/emissive 应上传为 sRGB variant（`BC7_SRGB`），normal/MR 保持 UNORM（§15 的色彩空间缺陷一起修）。

1. **decode images 701ms（次高性价比）**：72 张 4K PNG 的 `image` crate 解码，已 rayon 并行。继续压只有换解码器：
   - `zune-image` / `zune-png`（Rust 原生，SIMD，比 `image` 的 PNG 快 2-3×）
   - 或 `lodepng` 纯 PNG 路径
   - 注意：换解码器要保留"输出 RGBA8"的契约，否则 `to_rgba8` 的快路径会失效（会命中慢路径并打 warn）。
2. **texture upload 330ms（中性价比）**：72 次 `vkCreateImage` + `vkAllocateMemory` + staging map。改成 suballocator（如 `gpu-allocator` crate 或自写 bump allocator）把逐资源分配改成批量分配，预计砍一半。改动较大，动 `BatchUploader` 和 `RenderTextureManager` 的内存所有权。
3. **batch upload submit+wait 522ms（低性价比）**：4.5 GB 像素 + 100 MB mesh 过 PCIe，已接近物理带宽上限。除非上 async / timeline semaphore 让上传和首帧渲染重叠（本项目 `BatchUploader` 注释里提到的 follow-up），否则压不动。改 async 是架构级改动。
4. **懒加载 / 流式上传（架构级，最彻底）**：把"全量加载完再渲染"改成"先加载 mipmap 0 的低分辨率 + 首帧立即可见，后台流式补全高 mip"。需要改 `SceneStore` 的所有权模型和渲染管线的"纹理就绪"判定，工作量大，适合里程碑级重构。

**当前 ~1.8s 对这套 4.5 GB RGBA8 极端资产已经合理（真实游戏场景会小得多，见 §31.7）**。本次的核心教训是 31.1：**先拆日志定位，再按规模指标找冗余拷贝**，不要凭感觉猜瓶颈。`to_rgba8` 的零拷贝快路径 + drain 上传是两个通用的"GB 级数据搬运"反模式修复，下一步扩展加新资源类型（音频、动画）时直接套用。

### 31.7 测试资产不代表真实游戏负载（重要归因）

本次优化的所有数字（4.5 GB 像素、9 GB CPU 峰值、~1.8s 加载）都建立在 **Intel Sponza 2022** 测试资产上，这套资产的纹理规模**远超真实游戏**，不能用它的绝对数字推断引擎在生产场景下的表现。

**Sponza 版本对比**：

| 资产版本 | 纹理数 | 单张尺寸 | 像素总量 | RGBA8 解压 | 用途 |
|----------|--------|----------|----------|-----------|------|
| Crytek Sponza（经典，游戏常用） | ~20 | 混合 512/1024/2048 | ~200 MP | ~0.8 GB | 实时渲染基准 |
| **Intel Sponza 2022（本项目在用）** | **135** | **全部 4096×4096** | **2184 MP** | **~8.5 GB** | 离线渲染/参考图 |
| 典型 3A 游戏 Sponza 级场景 | ~30-50 | 混合 1K/2K/4K | ~400 MP | ~1.5 GB | 加上 BC 压缩后 ~0.4 GB |

gltf 引用的子集是 72 张全 4K（1.89 GB PNG -> 4.5 GB RGBA8），已经是极端情况。**没有任何真实游戏会给 Sponza 全 4K 纹理** -- Intel Sponza 2022 是为路径追踪参考图做的 4K 扫描资产，不是为实时渲染设计的。

**真实游戏如何控制纹理内存**（引擎下一步扩展应支持，按优先级）：

1. **GPU 块压缩（BC1/BC3/BC7/BCN）** -- §31.6 第 0 条已列。4096×4096 RGBA8 = 64 MB，BC7 = 16 MB（4:1），BC1 = 8 MB（8:1，无 alpha）。这是真实游戏纹理内存控制在 GB 级以下的根本手段。本项目当前全 RGBA8 上传是已知缺陷（§15），不只是色彩问题，也是内存问题。
2. **mip chain 流式加载**：只加载可视距离需要的 mip。4K 纹理完整 mip chain = 21 MB，但只有最近物体的 1-2 张需要 4K，远处用 1K/512。真实游戏按"表面离玩家多近"动态选 mip，CPU/GPU 内存峰值远低于全量加载。
3. **虚拟纹理（VT）**：3A 游戏把 GB 级纹理切成 64KB page 按需加载，CPU/GPU 只驻留可见 page。这是把"全 4K Sponza"压到可运行的唯一办法，但实现复杂（page table、page fault、反馈通道），属于引擎级里程碑。
4. **分辨率分级**：不是所有表面都给 4K。真实美术会按"这块表面离玩家多近、多常被看"给 1K/2K/4K 混合，而不是 Sponza 2022 那样无脑全 4K。

**对本项目的实际影响**：

- 引擎本身**没有内存泄漏或浪费**（§31.2/§31.3 的冗余拷贝已修）。4.5 GB 是这套资产解压后的真实大小，任何引擎加载它都会面临同样的像素数据量。
- 但引擎**缺真实游戏必备的纹理管线**：块压缩、流式加载、分辨率分级都没有。这是下一步扩展里程碑的工作，不是 bug。
- **测试建议**：下一步扩展性能基线应该用 Crytek Sponza 或自制的"混合分辨率"场景，而不是 Intel Sponza 2022，否则优化决策会被极端资产带偏（例如为了 4.5 GB 去做 suballocator，但真实场景 0.4 GB 根本不需要）。

**教训**：性能优化的结论要和测试资产绑定。用极端资产（Intel Sponza 2022 全 4K）得到的瓶颈分布，不能直接外推到生产场景。每次记录耗时数字时，旁边要标明资产规模（"72 张 4K = 4.5 GB RGBA8"），否则后人会误以为"引擎加载 Sponza 要 1.8s"是普遍结论，而实际 Crytek Sponza 大概几百毫秒就进去了。

### 32 IBL CPU 卷积磁盘缓存

**背景**：`IblResources::new()` 在引擎启动时对 HDR 环境贴图做三类 CPU 卷积 + BRDF LUT 积分，总计 ~4127ms（Intel Sponza 2022 + 1K HDR），是启动流程中最长的单次冻结（约占 64%）。其中 BRDF LUT（3260ms）占 IBL 总耗时的 79%。

#### 32.1 瓶颈分布（缓存前）

| 卷积步骤 | 耗时 | 占比 | 说明 |
|----------|------|------|------|
| BRDF LUT（512x512, 1024 samples） | 3260ms | 79% | 双重循环 512x512x1024 = 268M 次迭代，最密集 |
| 预滤波卷积（5 mips, 128 samples） | 792ms | 19% | 次热点，但 mip 链多 pass |
| 辐照度卷积（64x64, 4096 samples） | 75ms | 2% | 分辨率低，耗时少 |
| 总计 | 4127ms | 100% | |

#### 32.2 磁盘缓存设计

**核心思路**：将 CPU 卷积结果缓存到 `assets/ibr/` 目录，下一步扩展启动跳过计算直接加载 f32 原始数据回 `Vec<f32>`，Vulkan 上传管线（f16 转换 + staging buffer + submit）保持不变。

**缓存 key 方案**：

- **BRDF LUT**：固定文件名 `brdf_lut_512.bin`，无依赖（纯 GGX Smith 数学函数，任何场景、任何 HDR 结果相同）。缓存永远有效。
- **Env 相关（cubemap / irradiance / prefiltered）**：对 `env_bytes`（原始 .hdr 文件内容）做 `std::hash::DefaultHasher`（SipHash-1-3），hex 编码为目录名。HDR 文件内容变化 → hash 不同 → 自动产生新缓存。

**文件格式**：`u32 LE（元素数）+ f32 LE × 元素数`，带长度校验。共 9 个文件：

| 文件 | 大小 | 说明 |
|------|------|------|
| `assets/ibr/brdf_lut_512.bin` | ~2 MB | 512×512×2 floats, 独立于 HDR |
| `assets/ibr/<hash>/cube_512.bin` | ~24 MB | 6×512×512×4 floats |
| `assets/ibr/<hash>/irradiance_64.bin` | ~393 KB | 6×64×64×4 floats |
| `assets/ibr/<hash>/prefiltered_mip{0..4}.bin` | ~2.1 MB | 各 mip 独立文件，方便单 mip 缓存命中 |
| **总计** | **~28.5 MB** | 每 HDR 环境占 ~28MB 磁盘 |

**实现位置**：仅在 `crates/prism-render/src/ibl.rs` 内新增 5 个辅助函数（`ensure_cache_dir` / `cache_path` / `env_content_hash` / `save_f32_cache` / `load_f32_cache`），不新增外部依赖（只用 `std::hash::DefaultHasher` + `std::fs`），不改动任何其他文件。

#### 32.3 改动的权衡

| 取舍 | 选择 | 理由 |
|------|------|------|
| 存储格式 | f32 原始数组（非 f16） | 写回时直接可用，不需转换；28MB 可接受 |
| Hash 算法 | SipHash-1-3 而非 FNV/SHA | Rust std 内置，确定性，碰撞概率可忽略 |
| 缓存粒度 | 逐文件（9 个文件）而非单一大包 | 增量失效：换 HDR 只重建 env 相关，保留 BRDF LUT |
| 加载时机 | 同步阻塞（startup） | 当前 IBL 必须就绪才能渲染第一帧；异步下一步扩展 PR |
| 错误处理 | 任何 I/O 失败回退到重新计算 | 幂等：计算结果与缓存内容一致 |

**为什么不用更复杂的方案**：

- **LZ4 压缩**：28MB 不值得压缩，况且 f32 数据近乎白噪声（随机采样结果），压缩比极差。
- **异步/后台加载**：第一帧就需要 IBL 纹理（场景反射），异步需要先渲染无 IBL 再闪现的过渡逻辑，复杂度远大于收益。
- **GPU 卷积**：IBL 卷积（辐照度/预滤波）用 compute shader 在 GPU 做更快（~5-10ms），但 BRDF LUT 是中心瓶颈，且 GPU 卷积需要额外的 compute pipeline + 同步逻辑。

#### 32.4 结果

| 指标 | 缓存前 | 缓存后 | 加速比 |
|------|--------|--------|--------|
| IBL 卷积总耗时 | 4127ms | **1ms** | **~4000×** |
| Renderer 初始化（含 IBL） | 4441ms | 233ms | 19× |
| 启动至第一帧（含场景加载） | 6419ms | **2075ms** | **3.1×** |

#### 32.5 经验教训

1. **BRDF LUT 是最容易的优化**：不依赖任何外部输入，纯数学函数，缓存永远有效，3260ms→0ms 只花了几十行代码。任何引擎的第一个 IBL 优化都应该是 BRDF LUT 缓存。

2. **逐阶段拆解定位瓶颈有效**：从笼统的 "IBL: 3678ms" 拆成 8 个子阶段日志（HDR decode / generate_cubemap / convolve_irradiance / prefiltered / brdf_lut / create_images / upload / views），一眼锁定 BRDF LUT 是头号瓶颈，避免了对辐照度或预滤波做无谓优化。

3. **内容 hash 做缓存 key 比时间戳可靠**：文件修改时间在 git checkout / 复制时会被重置，导致不必要的缓存失效。内容 hash（SipHash of HDR bytes）保证"同内容同缓存"。

4. **缓存路径设计要考虑增量失效**：BRDF LUT 和 env 相关数据分开目录，换 HDR 时不破坏 BRDF LUT 缓存。

5. **f32 原始数组是最简单的序列化格式**：无 schema、无依赖、零拷贝反序列化（`ptr::copy_nonoverlapping`）。对于已知大小的数组，`u32 长度前缀 + f32 数据` 足够。

6. **"缓存失效"不是问题，依赖关系才是**：BRDF LUT 零依赖→永远有效；env 数据只依赖 .hdr 内容→内容 hash 驱动失效。不存在"我改了配置但缓存没刷新"的歧义。

#### 32.6 下一步扩展方向（不在本次范围）

- **skybox 组件化**：将 HDR + 预计算 IBL 数据 + BRDF LUT 打包为一个可复用的 skybox 资源组件（ECS 或 asset），支持运行时切换环境贴图而不用重建 `IblResources`。
- **GPU 卷积**：辐照度和预滤波可以用 compute shader 在 GPU 上完成（~5-10ms vs CPU ~900ms），并直接生成 Vulkan image 而非 CPU f32 staging，省掉 f16 转换 + 上传步骤。
- **BRDF LUT samples 可配置**：当前 1024 固定，可暴露为常量或运行时参数（512 约 800ms，256 约 200ms，质量差异 < 0.5 dB）。
- **IBL 加载进度反馈**：引擎启动时显示 "loading environment..." 或使用后台线程 + 信号量，避免无响应窗口。

## 33. GI 烘焙器两大 bug：inside-solid 漏判墙体 + slangc 调度导致 SH 系数腐败

**场景**：开启 GI 后场景五颜六色。用户怀疑"是不是把点光源也算上了"。实际上 GI 烘焙器从头到尾只用一个方向光，五颜六色是两个独立 bug 叠加的结果。

### 33.1 inside-solid 检测漏判墙体内部

**问题**：烘焙器的 inside-solid 检测（判定哪些 probe 落在墙/地板实体内并清零其 SH）用"6 条轴对齐短射线全部命中"作为判据。对真实墙体失效：

- **薄墙**：probe 落在墙板里，沿法线的 2 条轴射线命中近表面，但沿墙面内的 4 条轴射线从墙板边缘逃逸 -> 2/6 命中 -> 漏判，墙体内部产生错误的亮/暗 SH 泄漏进探针体积。
- **门口附近的墙边**：某条轴射线从门口逃逸 -> 5/6 命中 -> 漏判。

**原因**：6 轴全命中要求太苛刻。任何一条轴射线逃逸（薄板边缘、门洞、开口）就漏判，而真实场景的墙体恰好充满这种情况。对照实验（旧逻辑 vs 新逻辑）显示 Sponza 场景 inside-solid 从 23 升到 112（多识别 5 倍墙体内部 probe）。

**修复**：改用"背面命中 + 短距离"两个信号判定，并 fold 进主射线循环（每个 hit 本来就要取三角形法线做 bounce 光照，只多一次点积 + 比较）：

- **背面**（几何法线与射线同向 = 射线正离开实体）区分"在墙里"与"在房间里"：房间的内表面是正面命中，实体的外表面是背面命中。
- **短距离**（`CommittedRayT() < test_len` = 半个 probe 间距）区分"在墙里"与"在大空壳里"（如 test cube）：两者都看到背面，但空壳的内表面在房间尺度，远超 test_len。

两个信号缺一不可：只看背面会误判空壳，只看距离会误判狭窄走廊。判定阈值 `≥50%` 的射线为短距离背面命中，容忍少数射线从薄板边缘或门洞逃逸仍判为 inside-solid。test cube（空壳）验证未被误判（inside-solid=1/216，仅边界 probe），证明"短距离"信号成功区分了"墙内"与"大空壳内"。

### 33.2 slangc 指令调度 bug 导致 SH 系数腐败（五颜六色的真正根因）

**问题**：27.7% 的 probe DC（c=0，平均辐照度）为负，最低 -7.478。DC = `Σ radiance·C0·w`，每一项非负，**数学上必须 ≥ 0**。负 SH 系数经 `EvalSH9(normal, sh)` 重建后随法线方向产生任意正负大值，乘 albedo/π 就是满屏色块。

**排查过程（控制变量实验定位故障层）**：

1. 强制 `radiance=(1,1,1)` -> 负 DC 归零 -> readback/内存布局无罪，问题在 radiance 计算。
2. 强制 `hit_albedo=(1,1,1)` -> 负 DC 归零 -> 嫌疑锁定 albedo 读取。
3. CPU 端 vertex color 全 ∈ [0,1] + 索引全部 in-bounds -> 数据无罪，问题在 GPU 读取。

**根因**：slangc 2025.11 对 `vertexData.Load3(... + COLOR_OFFSET)` 的颜色加载做了错误指令调度，返回垃圾值（含负值）。特征：位置加载正常、颜色加载异常（同一 buffer、同一套偏移）；R/G/B 通道**独立**腐败（不是统一符号错误，是读到了相邻内存）。

**这个 bug 的抗性（以下全部无效）**：
- `-O0`（不是优化级别问题，是调度路径问题）
- `Load4` 替代 `Load3`（不是 3 分量专用路径问题）
- 函数提取（被内联了）
- 连续加载、未使用的 `CommittedRayT()`、`0.0 * CommittedRayT()`（被 DCE 了）
- 简单的 never-taken `continue` 分支

**唯一有效**的修复：在颜色插值后，用相同重心权重重新消费 `v0/v1/v2`（位置），重建命中点位置 `recon = w0*v0 + bary.x*v1 + bary.y*v2`，与 `world_pos + dir*CommittedRayT()` 比对，用 `step(err, 0.5)` 乘到 albedo 上。正常情况下 `recon ≈ hit_pos`，乘数为 1.0（无副作用）；这个计算迫使编译器保留 v0/v1/v2 跨过颜色加载的生命周期，规避调度 bug。sentinel 实验证明该检查从不触发（max DC 中无 999 标记）--它是"语义上的 no-op"修了 bug。

### 33.3 经验教训

1. **数学不变量是最强的 bug 信号**。DC 必须 ≥ 0，烘焙出 -7.478 直接违反公理。早期日志里一直有 `DC stats: min [-7.478,...]` 但被当成"室内暗"的正常现象忽略了。**反常的数值本身就在喊有 bug**，别用"看起来合理"的视觉印象掩盖数学矛盾。

2. **控制变量实验比推理猜测高效**。"强制注入已知正确值"的二分法（radiance=1 / albedo=1）每个实验把嫌疑缩小一半，三轮就锁定到"GPU 颜色加载"。比逐行读代码快得多。

3. **先建 baseline，判断 bug 是新还是旧**。一度怀疑是自己改 inside-solid 引入的。`git stash` 还原旧 shader 跑一次，DC 数据逐字节相同 -> 确认是既有 bug。**改动后出问题，先证明因果关系再修**，别凭"我刚改了这块"就认领。

4. **"语义上的 no-op"能修 bug，但必须留下证据**。recon 校验在正常情况下对结果零影响（从不触发），却修了 bug，极度反直觉，容易在日后被当死代码删掉。注释里必须写清：它**既**是真实的数据完整性校验，**也**是规避 slangc 调度 bug 的手段，两重作用缺一不可。**workaround 要带"为什么不能删"的注释**。

5. **假设要先用最小代价验证再深入**。排查中假设过三个错误根因（NaN from 退化三角形 / `-O` 优化 bug / StructuredBuffer 替换），每个都消耗一次完整 GPU 烘焙（~30 秒）。其中 StructuredBuffer 方案没算清 std140 会把 float3 填充到 16 字节、stride 从 56 变 80，反而更糟。改 layout 前先算清内存影响。

	6. **slangc bug 的识别特征**：产生"数学上不可能"的值 + 通道独立腐败 + 同一 buffer 的部分加载异常。这类 bug 与 §13（Slang 给 opaque 运行时数组加非法 `ArrayStride`）同属编译器正确性问题，`spirv-val` 抓不到（SPIR-V 合法但语义错），只能靠运行时数据不变量（DC ≥ 0）发现。

## 34. 设备丢失 (Device Lost) 排查：围栏同步 + TDR + BLAS 索引双倍偏移

**背景**：Sponza 场景加载（405 个实例）在 BLAS/TLAS 构建阶段触发 `VK_ERROR_DEVICE_LOST`。初看是 `queue_wait_idle` 同步问题，实际有三层嵌套。

### 34.1 第一层：`queue_wait_idle` + 空围栏是不可靠的同步模式

**现象**：`vkFreeCommandBuffers(): pCommandBuffers[0] is in use` 验证错误 → 设备丢失。

**问题**：`submit_and_wait` 辅助函数用 `vkQueueSubmit(queue, &submit, VK_NULL_HANDLE)` + `vkQueueWaitIdle(queue)` 来等待 GPU 完成。`queue_wait_idle` 在 Windows 驱动上可能在校验完成前返回，导致命令缓冲区在 GPU 仍在执行时被释放。验证层捕获到释放 in-use 命令缓冲区，驱动程序内部状态被破坏，最终发生设备丢失。

**修复**：全部替换为专用围栏模式：
```rust
// ❌ 不可靠
device.queue_submit(queue, &submit, vk::Fence::null());
device.queue_wait_idle(queue);
device.free_command_buffers(pool, &cmds);

// ✅ 可靠
let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
device.queue_submit(queue, &submit, fence)?;
device.wait_for_fences(&[fence], true, u64::MAX)?;
device.destroy_fence(fence, None);
device.free_command_buffers(pool, &cmds);
```

**教训**：
- `queue_wait_idle` 不是 `wait_for_fences` 的替代品。它在某些驱动上可能返回过早，而 `wait_for_fences` 保证该特定提交已完成。
- `buffer::upload_to_buffer` 已有正确的围栏模式（注释里明确写了为什么不用 `queue_wait_idle`），但 `acceleration_structure.rs`、`ibl.rs`、`gtao.rs`、`scene_scope.rs` 各有独立的 `submit_and_wait` 副本，且都用错了。**Vulkan 辅助函数容易滋生碎片化的错误副本**，应该统一到一个共享帮助函数中。
- `queue_wait_idle` 唯一合适的用途是程序退出前的"排空（drain）"：在释放所有资源之前确保 GPU 空闲。

**受影响的代码位置**（6 处，全部修复）：

| 文件 | 函数/位置 | 用途 |
|------|-----------|------|
| `acceleration_structure.rs` | `submit_and_wait` | BLAS + TLAS 构建 |
| `ibl.rs` | `submit_and_wait` | IBL 上传 |
| `gtao.rs` | `transition_ao_images_to_shader_read` | GTAO 图像转换 |
| `scene_scope.rs` | `upload_probe_volume` | GI 探测体积上传 |
| `prism_bake_gi.rs` | 两处内联 | 烘焙读取回 |
| `prism_bake_image.rs` | 两处内联 | 图像读取回 |

### 34.2 第二层：405 次独立 BLAS 构建触发 Windows TDR

**现象**：围栏修复后验证错误消失，但设备丢失仍在发生，且耗时从 ~200ms 变为 ~5s。新的耗时恰好是将每个 BLAS 构建提交到 GPU 并等待单个围栏所需的时间总和。

**问题**：405 个 BLAS 构建分别提交（每个都有 fence create/submit/wait/free/destroy），产生了 ~5 秒的连续执行时间。`batch upload submit+wait` 约 750ms（同数据量的纹理上传）提示 BLAS 构建不应该比纹理上传慢 6 倍。

**第一次修复——单次批量提交**：将所有 405 个 BLAS 构建记录到单个命令缓冲区中，提交一次，等待一次。这消除了 ~405× 的提交开销，但连续 GPU 执行时间仍然是 ~5 秒，触发了 Windows TDR（默认 2 秒超时）。

**第二次修复——分块提交**：将 405 个 BLAS 构建分成每组 64 个的批次（7 个批次），每一批用一个命令缓冲区 + 一次提交 + 一次围栏等待。这样每批的 GPU 执行时间 < 2 秒，TDR 不会触发，同时提交开销只有 7 次而非 405 次。

**教训**：
- Windows TDR（超时检测与恢复）会在 GPU 连续超过 2 秒无响应时重置 GPU。大块 GPU 工作（数百个 BLAS 构建、大型计算分发）必须分成更小的批次，每个批次之间有围栏等待，让操作系统有机会与 GPU 通信。
- 围栏等待不仅仅是 CPU-GPU 同步原语——它们也是 TDR 规避机制：GPU 在围栏信号处写入时间戳，驱动程序借此确认 GPU 仍在运转。
- 批量提交到单个命令缓冲区 + 单个围栏等待，与逐个提交 + 逐个等待相比，CPU 开销更低（1 个分配/释放 vs 405 个）。最佳方案是**分块批量**：将工作分块，使每块执行时间 < 2 秒，但块数 << 资源数。

**分块策略对比**：

| 策略 | CPU 开销 | GPU 突发 | TDR？ |
|------|----------|----------|-------|
| 逐个构建（405 次提交） | 极高（405× 围栏/缓冲区） | 短（~12ms） | 否 |
| 全量批量（1 次提交） | 低（1× 围栏/缓冲区） | 长（~5s） | **是** |
| **分块（64/块，7 次提交）** | **低（7× 围栏/缓冲区）** | **适中（~700ms）** | **否** |

### 34.3 第三层（根因）：组合索引缓冲区中的索引偏移导致 GPU 越界读取

**现象**：分块后设备丢失仍然存在，但执行时间未变（仍是 ~5 秒 GPU 时间），说明问题不是 TDR。

**根因**：`build_pt_scene` 将所有实例的顶点和索引连接到一个组合缓冲区中，索引被偏移了 `vertex_base`（每个实例在组合顶点数组中的起始位置）。然后对每个实例调用 `BlasEntry::build_at(vertex_addr=组合偏移处地址, index_addr=组合索引偏移处地址, vertex_count=实例顶点数)`。

在 BLAS 几何中：
- 顶点缓冲区从 `vaddr = vbase + vertex_base × stride` 开始（实例的第一个顶点）
- 索引缓冲区包含 `original_index + vertex_base`（偏移后的值）
- GPU 计算顶点位置：`vaddr + (original_index + vertex_base) × stride`
- = `(vbase + vertex_base × stride) + (original_index + vertex_base) × stride`
- = `vbase + (2 × vertex_base + original_index) × stride` ← **双倍偏移！**

对于后面的实例（`vertex_base` 较大），`2 × vertex_base + original_index` 轻松超过总顶点数，导致越界 GPU 内存读取 → GPU 页错误 → 设备丢失！

旧的 `queue_wait_idle` 代码没有实际执行 BLAS 构建（立即返回），所以这个 bug 从未被触发。围栏修复让它暴露了。

**修复**：将 `vertex_addr` 设置为组合缓冲区基地址（`vbase`），将 `vertex_count`（作为 `max_vertex`）设置为总顶点数。由于索引值已经是相对于 `vbase` 的（`vertex_base + original_index`），GPU 能正确计算：
`vbase + (vertex_base + original_index) × stride` ✓

**教训**：
- **`queue_wait_idle` 返回过早会隐藏 GPU 错误**。旧的代码可能用了几周都没发现这个 BLAS 寻址 bug，因为 GPU 从来没有实际执行过 BLAS 构建——提交被吞掉了，验证层是唯一抱怨的。切换到正确围栏同步，所有累积的 GPU 错误立刻变为设备丢失。
- **"围栏修复导致新问题"往往是旧 bug 被暴露了**。先后出现的现象（验证错误 → 耗时变长 → 设备丢失）是同一个根本原因（索引偏移）经过不同同步层后的不同表现。
- **验证层错误 vs 设备丢失**：验证层错误提示 API 滥用（释放 in-use 缓冲区），但不一定直接导致崩溃。设备丢失是 GPU 处于不可恢复状态。先修验证层错误，再处理设备丢失。
- 写索引连接循环时要检查相对偏移 vs 绝对基地址的一致性。特别是当同一缓冲区被 GPU 的不同单元（BLAS 构建、着色器内存访问）以不同方式解读时，每个消费者的偏移约定必须独立验证。
- `BlasEntry::build_at` 的设计（接受 `vertex_addr` 但根据 `vertex_count` 推断 `max_vertex`）让调用者有义务传递正确的基地址。如果调用者传递的是偏移地址，`max_vertex` 必须是相对于该偏移地址的（即实例的本地 `vertex_count - 1`）；如果传递的是组合基地址，`max_vertex` 必须是总顶点数。**这个"相对谁？"的约定没有在 API 文档中说明**，是两个消费者之间的不匹配。更好的设计：`BlasBuildParams` 要求显式传递 `max_vertex`（而非 `vertex_count`），迫使调用者思考自己在做什么。

### 34.4 排查逻辑

```
设备丢失
├─ 验证层：vkFreeCommandBuffers in-use
│  └─ queue_wait_idle 返回过早 → 替换为围栏 ✓
│
└─ 设备丢失（围栏修复后仍然）
   ├─ 耗时 ~5s → 怀疑 TDR
   │  ├─ 单次批量提交 → 仍然 ~5s + 设备丢失
   │  └─ 分块 64/批 → 耗时不变 + 设备丢失仍然
   │     └─ 不是 TDR，是 GPU 执行时崩溃
   │
   └─ BLAS 寻址错误 → 索引双倍偏移 ✓
      └─ 旧代码从未真正执行 BLAS（queue_wait_idle 返回过早），
         bug 被隐藏。围栏修复暴露了它。
```

### 34.5 总结

| 层 | 现象 | 根因 | 修复 |
|----|------|------|------|
| 同步 | `vkFreeCommandBuffers in-use` 验证错误 | `queue_wait_idle` 返回过早 | 替换为专用围栏 |
| GPU 时间 | 围栏修复后 ~5s 执行时间 | 405 次独立提交，每次 12ms | 分块批量（64/块，7 次提交） |
| GPU 崩溃 | 设备丢失持续发生 | BLAS 索引偏移双倍 `vertex_base` | `vertex_addr` 使用组合基地址 |

这三层中的每一层都可能被误认为是根本原因，但实际上只有上层症状。真正的 bug（索引偏移）在旧的同步代码下一直被隐藏。**围栏修复是诊断工具，不是最终修复**——它揭示了真正的问题。

## 35. 描述符集每帧更新需 `UPDATE_AFTER_BIND` 标志

**背景**：`PathTracePass` 每帧调用 `update_ds()` 写入全部描述符（bindings 0-7），因为几何/材质/输出资源在帧间可能变化（TLAS 重建、accum 图像 resize 等）。描述符集布局创建时没有设置 `UPDATE_AFTER_BIND`，导致 Vulkan 验证层报错：

```
vkUpdateDescriptorSets(): dstBinding (0) was created with VkDescriptorBindingFlags(0),
but VkDescriptorSet ... is in use by VkCommandBuffer ...
```

**根因**：PathTracePass 的描述符集在 `new()` 时创建，布局用默认 flag（= 无更新后绑定）。但 `execute()` 每帧调用 `update_ds()` 时，上一帧的命令缓冲区可能仍在飞行（pending），写入正在被 GPU 引用的描述符集违反 Vulkan 规范。

**修复**：
- 描述符集布局绑定标志加 `UPDATE_AFTER_BIND | PARTIALLY_BOUND`
- 布局创建加 `UPDATE_AFTER_BIND_POOL` flag
- 池创建加 `UPDATE_AFTER_BIND` flag

```rust
// 修改前
let ds_layout = device.create_descriptor_set_layout(
    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings), None)?;

// 修改后
let binding_flags = [
    vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
        | vk::DescriptorBindingFlags::PARTIALLY_BOUND,
    // ... 每个 binding 相同
];
let mut flags_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default()
    .binding_flags(&binding_flags);
let ds_layout = device.create_descriptor_set_layout(
    &vk::DescriptorSetLayoutCreateInfo::default()
        .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
        .bindings(&bindings)
        .push_next(&mut flags_info), None)?;
let ds_pool = device.create_descriptor_pool(
    &vk::DescriptorPoolCreateInfo::default()
        .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)  // 池也要
        .max_sets(1).pool_sizes(&pool_sizes), None)?;
```

**哪类描述符集需要 `UPDATE_AFTER_BIND`**：
| 使用模式 | 需要 UAB？ | 替代方案 |
|----------|-----------|----------|
| 只在初始化时写入一次，之后只读 | 不需要 | — |
| 每帧写入相同 set，但只有一帧飞行（单缓冲） | 不需要 | 若 `wait_idle` 或围栏保证 |
| 每帧写入相同 set，多帧并行（双/三缓冲） | **需要** | 或每帧分配新 set（开销大） |
| 只更新设置了 `UPDATE_AFTER_BIND` 的 binding | 仅该 binding | 其他 binding 仍受限 |
| 用到 `PARTIALLY_BOUND`（有未绑定的描述符槽） | 推荐 | — |

**教训**：
- 描述符集的绑定标志不是可选的锦上添花——当更新模式涉及"写入正在 GPU 使用的 set"时，它们是强制性的。
- `BindlessTextureTable` 已正确使用这些标志。`PathTracePass`（以及将来类似的时间性累积/重建每帧资源的 pass）必须遵循相同模式。
- 一个识别信号：如果 `execute()` 里调了 `update_descriptor_sets()`，几乎肯定需要 `UPDATE_AFTER_BIND`。

## 36. `fix_spv` 遗漏：所有 import `path_integrator.slang` 的 compute shader 也需要 ArrayStride 修复

**背景**：`compile.sh` 对 `scene_frag.frag.spv` 调用了 `fix_spv`（line 130），但三个 compute shader（`pt_render.comp.spv`、`gi_bake.comp.spv`、`gi_render.comp.spv`）编译后没有调用 `fix_spv`。导致 `pt_render.comp.spv` 加载时触发 SPIR-V 验证错误：

```
vkCreateShaderModule(): pCreateInfo->pCode (spirv-val produced an error):
Invalid explicit layout decorations on type for operand '580[%580]'
  %bindlessSrvs = OpVariable %_ptr_UniformConstant__runtimearr_579 UniformConstant
```

**根因**：`path_integrator.slang` 声明了 bindless 纹理/采样器 runtime array：
```slang
[[vk::binding(0, 1)]]
SamplerState globalSamplers[];
[[vk::binding(1, 1)]]
Texture2D bindlessSrvs[];
```
Slang 编译器对这些 runtime array 的 opaque 元素类型生成非法的 `OpDecorate ArrayStride <n>` 装饰。`fix_spirv.py` 可以剥离这些非法装饰，但：

- `scene_frag.slang` 直接声明了相同 binding（line 78-85），但被 `fix_spv` 修复了 ✅
- `pt_render.slang` 通过 `import path_integrator` 间接声明相同 binding，但没有 `fix_spv` ❌

`compile.sh` 当初加 `fix_spv` 时只考虑了 rasterizer 的 fragment shader，遗漏了后来添加的 compute shader。

**修复**：为所有 import `path_integrator` 的 compute shader 添加 `fix_spv` 调用：
```bash
# compile.sh line 152-168
fix_spv "$OUT/gi_bake.comp.spv"
fix_spv "$OUT/gi_render.comp.spv"
fix_spv "$OUT/pt_render.comp.spv"
```

**教训**：
- `fix_spv` 不是针对特定文件名，而是针对**任何包含 bindless runtime array 声明**的 SPIR-V 模块。添加新的 slang 文件时，如果它声明或 import 了 `SamplerState[]` / `Texture2D[]` 等 opaque runtime array，就必须加上 `fix_spv`。
- 一个简单的判断规则：如果 `.slang` 文件包含 `globalSamplers[]` 或 `bindlessSrvs[]` 声明，或者 import 了包含这些声明的模块（`path_integrator.slang`），则编译后需要 `fix_spv`。
- Slang 的 runtime array ArrayStride bug 影响所有使用 bindless 纹理的 compute shader，不只是 fragment shader。§13 提到的是 fragment shader 的复现，§33 提到 slangc 数据腐败——这里是第三个变种：编译器生成的 SPIR-V 被 `spirv-val` 拒绝。

## 37. `UPDATE_AFTER_BIND` 需要对应描述符类型的设备子功能启用

**背景**：§35 给 `PathTracePass` 的 descriptor set layout 加上了 `UPDATE_AFTER_BIND | PARTIALLY_BOUND` 标志，结果 `vkCreateDescriptorSetLayout` 报错：

```
pBindingFlags[0] includes VK_DESCRIPTOR_BINDING_UPDATE_AFTER_BIND_BIT
but pBindings[0].descriptorType is VK_DESCRIPTOR_TYPE_STORAGE_IMAGE
but descriptorBindingStorageImageUpdateAfterBind was not enabled.
```

同样的报错重复了 bindings 0-7，三种描述符类型各对应不同的 feature bit。

**根因**：`UPDATE_AFTER_BIND` 是一个绑定级别的标志，但 Vulkan 要求对每种描述符**类型**在设备创建时启用对应的子功能。只加 layout/pool flag 不够——Vulkan 1.2 的 `PhysicalDeviceVulkan12Features` 里有一组粒度很细的 `descriptorBinding<Type>UpdateAfterBind` 字段，每个控制一种描述符类型的 update-after-bind 行为。

**修复**：在 `context.rs` 的设备创建处，对 `PathTracePass` 用到的三种描述符类型启用对应的 feature：

| 描述符类型 | Vulkan feature 字段 | 所在结构体 |
|-----------|-------------------|-----------|
| `VK_DESCRIPTOR_TYPE_STORAGE_IMAGE` | `descriptor_binding_storage_image_update_after_bind` | `PhysicalDeviceVulkan12Features` |
| `VK_DESCRIPTOR_TYPE_STORAGE_BUFFER` | `descriptor_binding_storage_buffer_update_after_bind` | `PhysicalDeviceVulkan12Features` |
| `VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR` | `descriptor_binding_acceleration_structure_update_after_bind` | `PhysicalDeviceAccelerationStructureFeaturesKHR` |

```rust
// 在 descriptor_indexing 块内
vk12.descriptor_binding_storage_image_update_after_bind = vk::TRUE;
vk12.descriptor_binding_storage_buffer_update_after_bind = vk::TRUE;

// 在 acceleration_structure 块内
accel_features.descriptor_binding_acceleration_structure_update_after_bind = vk::TRUE;
```

**为什么之前 `BindlessTextureTable` 没有触发这个问题？**

`BindlessTextureTable` 只有两种描述符类型：`SAMPLER` 和 `SAMPLED_IMAGE`。对 `SAMPLED_IMAGE` 来说需要的子功能是 `descriptor_binding_sampled_image_update_after_bind`，而它已经在 `if rt_caps.descriptor_indexing` 块中被启用了（当初就是为了 bindless 加的）。所以 bindless table 一切正常。

`PathTracePass` 额外使用了 `STORAGE_IMAGE`、`STORAGE_BUFFER`、`ACCELERATION_STRUCTURE_KHR`，每种都需要单独启用其 update-after-bind 子功能。Vulkan 的 feature 设计故意做到类型级别的粒度——你可以让 sampled image 支持 update-after-bind 而 storage image 不支持，如果应用只需要 bindless 纹理的话。

**教训**：
- `UPDATE_AFTER_BIND` 涉及**三层**启用，缺一不可：
  1. **设备功能**：`PhysicalDeviceVulkan12Features` 或对应扩展 feature 结构体中的 `descriptorBinding<Type>UpdateAfterBind`
  2. **布局 flag**：`DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL` + `DescriptorSetLayoutBindingFlagsCreateInfo` 里的 `UPDATE_AFTER_BIND_BIT`
  3. **池 flag**：`DescriptorPoolCreateFlags::UPDATE_AFTER_BIND`
- 新增描述符类型到 update-after-bind set 时，先查对应的 `descriptorBinding<Type>UpdateAfterBind` 是否已在设备创建处启用——这是最常见的遗漏点。
- `BindlessTextureTable::required_features()` 返回了它需要的 feature 集，但 `PathTracePass` 没有对应的 `required_features()` 方法。实践中，每个需要特殊 device feature 的模块应该暴露一个 `required_features()`，然后在设备创建处汇总启用。

## MIS 只对「两种策略都能采到同一个光源」成立

`path_integrator.slang` 的多光源 NEE 曾经这样算权重：

```slang
float p_nee_al = 1.0 / (4.0 * PI * num_lights);   // 当成每球面度 pdf
total += brdf * L * p_nee_al / (p_nee_al*p_nee_al + p_brdf*p_brdf);
```

点光/聚光是**解析 delta 光源**：它们不在 TLAS 里，BRDF 采样永远打不到，
所以 NEE 是唯一策略 —— 不存在第二个 pdf 可以平衡。更糟的是那个 `1/(4πN)` 是
**选灯概率**，量纲是「每盏灯」，而 `p_brdf` 是「每球面度」；把两者塞进同一个
power heuristic，等于用一个量纲错的 pdf 去缩放贡献：N = 1 时点光只剩
`p²/(p²+p_brdf²) ≈ 6%` 的能量。

要做 MIS，必须先把采样策略的概率换算到**同一个测度**（通常是对立体角）：面积
采样 → 立体角要乘 Jacobian `d² / |cos_light|`（emissive triangle 的 NEE 就是这么写的）。
delta 光源没有面积，也就不存在这个 Jacobian——正确做法是权重取 1。

**教训**：
- 加 MIS 前先问：两个策略是否都能采到同一个光源？delta 光源（点光、方向光、
  用来近似面光的中心点采样）不能和 BRDF 采样做 MIS。
- pdf 的量纲要对齐：离散概率（选灯 `1/N`）、面积 pdf、立体角 pdf 三者不同，
  混用会静默压暗/提亮，编译器不会报错哪。
- 这类错误只在「有解析光源 + 开 NEE」时才出现，视觉上像「灯不够亮」，
  很容易被误当成 tonemap 或曝光问题。
