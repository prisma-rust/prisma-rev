// Post-process pass - fullscreen-triangle tonemap (Reinhard / ACES) from the
// offscreen HDR color attachment into the sRGB swapchain.
//
// The ForwardPass renders the lit scene into an R16G16B16A16_SFLOAT HDR image;
// this pass samples that image, applies the selectable tonemap operator, and
// writes the result to the swapchain (an sRGB format, so the hardware encodes
// the linear output to sRGB on write).
//
// Sets (must match `PostPass::ensure_pipeline`):
//   set 0, binding 0 - HDR color texture (combined Sampler2D)
//   set 0, binding 1 - scene depth (combined Sampler2D, DOF)
//   set 0, binding 2 - 3D color-grading LUT (combined Sampler3D, #502)
//   set 0, binding 3 - motion-blur matrices (uniform buffer, #502 Phase 2)
//
// Push constants: PostPush - `tonemapMode` (0 = Reinhard, 1 = ACES,
// 2 = passthrough for 2D scenes so the sprite palette survives untouched),
// `aaMode` (FXAA), `invTexelSize`, the color-grading knobs, HDR bloom controls,
// and the Phase 2 chromatic-aberration / film-grain strengths (#502).
//
// Compile:
//   slangc post.slang -profile spirv_1_5 -target spirv \
//          -entry vertexMain -stage vertex   -fvk-use-entrypoint-name -o post.vert.spv
//   slangc post.slang -profile spirv_1_5 -target spirv \
//          -entry fragmentMain -stage fragment -fvk-use-entrypoint-name -o post.frag.spv

// ---- set 0, binding 0 - HDR color attachment (combined image+sampler).
[[vk::binding(0, 0)]]
Sampler2D hdrTex;

// ---- set 0, binding 1 - scene depth (combined image+sampler, D32_SFLOAT).
// Used by the depth-of-field effect (#502 Phase 2). Debug RT views disable DOF
// on the Rust side, so the depth texture is only sampled for effect.
[[vk::binding(1, 0)]]
Sampler2D depthTex;

// ---- set 0, binding 2 - 3D color-grading LUT (#502).
// `lutStrength == 0` skips the fetch entirely, so an unconfigured project is
// bit-identical to the pre-LUT shader. The pass always binds an identity LUT
// (or the loaded `.cube`) so the descriptor is never unbound.
[[vk::binding(2, 0)]]
Sampler3D lutTex;

// ---- set 0, binding 3 - camera motion-blur matrices (#502 Phase 2) ----
// `invViewProj` reconstructs world space from the current depth; `prevViewProj`
// projects that world point into the previous frame. `resolution` converts a
// normalized velocity to pixels for the trail clamp.
struct MotionBlurData
{
    float4x4 invViewProj;
    float4x4 prevViewProj;
    float2 resolution;
    float2 _pad;
};
[[vk::binding(3, 0)]]
ConstantBuffer<MotionBlurData> motion;

// ---- set 0, binding 4 - adapted luminance from AutoExposurePass ----
// One R32_UINT holding a float bit pattern: the scene luminance that maps to
// middle grey. It reads as zero until the metering pass has run at least once,
// and zero leaves the image untouched (see `auto_exposure` below). This is the
// same buffer layout RTXGI's tonemapper consumes, so a future DLSS integration
// can share it.
[[vk::binding(4, 0)]]
StructuredBuffer<uint> adaptedLuminance;

// Scene luminance mapped to middle grey (RTXGI `AUTO_EXPOSURE_KEY`).
static const float AUTO_EXPOSURE_KEY = 0.18;

// Exposure gain from the metered luminance. Returns 1 (a no-op) until the
// metering pass has written a usable value, and for non-finite input so a
// broken frame cannot blow out the whole image.
float auto_exposure()
{
    const float adapted = asfloat(adaptedLuminance[0]);
    if (!isfinite(adapted) || adapted <= 0.0)
        return 1.0;
    return AUTO_EXPOSURE_KEY / adapted;
}

// Push constants: tonemap operator selector (matches `tonemapMode` in FrameUBO).
//
// `aaMode` selects the anti-aliasing path applied after tonemapping:
//   0 = off (single tap, exactly the pre-#494 behaviour)
//   1 = FXAA (luma-based edge blend over the tonemapped image)
//
// `invTexelSize` is 1/extent so the FXAA taps can step by one texel. It is a
// push-constant field rather than a sampled texture size so the shader needs no
// extra resource binding.
public struct PostPush
{
    public uint tonemapMode;
    public uint aaMode;
    public float2 invTexelSize;
    // Display-referred color grading (#502). Defaults keep the pre-#502 image:
    // contrast 1, saturation 1, vignette 0.
    public float contrast;
    public float saturation;
    public float vignette;
    // HDR bloom (#502). `bloomIntensity == 0` skips the extra taps entirely.
    public float bloomIntensity;
    public float bloomThreshold;
    // Lens chromatic aberration (#502 Phase 2). `0` skips the extra taps and is
    // bit-identical to the pre-effect path; `1` is the strongest useful amount.
    public float chromaticAberration;
    // Film grain (#502 Phase 2). Display-referred monochrome noise over the
    // final image; `0` skips the hash and is bit-identical to the pre-effect
    // path, `1` is the strongest useful amount.
    public float filmGrain;
    // White balance (#502 Phase 1). `temperature` moves the white point warm
    // (> 0) or cool (< 0); `tint` moves it magenta (> 0) or green (< 0).
    // Both are in [-1, 1], and 0 / 0 is the identity gain.
    public float temperature;
    public float tint;
    // Depth of field (#502 Phase 2). `nearPlane` / `farPlane` linearize the
    // sampled depth; `dofFocusDistance` is in metres, `dofAperture` in [0, 1]
    // and `dofMaxBlur` is the largest blur radius in pixels. aperture 0 skips
    // the whole effect and is bit-identical to the pre-effect path.
    public float nearPlane;
    public float farPlane;
    public float dofFocusDistance;
    public float dofAperture;
    public float dofMaxBlur;
    // 3D LUT color grading (#502). `lutStrength` is in [0, 1] and 0 skips the
    // fetch; `lutSize` is the edge length of the bound cube texture (usually
    // 32) and is used to place samples on texel centres.
    public float lutStrength;
    public float lutSize;
    // Camera motion blur (#502 Phase 2). `motionBlur` scales the reprojected
    // velocity; 0 skips the depth reconstruction and is bit-identical to the
    // pre-effect path. `motionBlurMaxPixels` caps long camera jumps.
    public float motionBlur;
    public float motionBlurMaxPixels;
};
[[vk::push_constant]]
PostPush pc;

// ACES filmic tonemap (Narkowicz 2016) - self-contained copy of the operator
// used in scene_frag.slang.
float3 aces(float3 x)
{
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), 0.0, 1.0);
}

[shader("vertex")]
public void vertexMain(
    uint vid : SV_VertexID,
    out float4 clipPos : SV_Position,
    out float2 uv : TEXCOORD0
)
{
    // Fullscreen triangle covering NDC [-1,1]^2. The three vertices are
    // (-1,-1), (3,-1), (-1,3); the clipped region is the whole screen. No
    // vertex buffer is bound - the corner is selected by `vid`.
    float2 pos = float2(
        (vid == 1u) ? 3.0 : -1.0,
        (vid == 2u) ? 3.0 : -1.0
    );
    clipPos = float4(pos, 0.0, 1.0);
    // Vulkan NDC y = -1 is the top; UV.y = 0 at the top. Match the framebuffer
    // top-left origin so the HDR image keeps the ForwardPass orientation.
    uv = pos * 0.5 + 0.5;
}

// Tonemap one HDR sample with the operator selected by `pc.tonemapMode`.
float3 tonemap(float3 hdr)
{
    if (pc.tonemapMode == 0u)
        return hdr / (hdr + 1.0);                 // Reinhard
    if (pc.tonemapMode == 2u)
        return saturate(hdr);                      // passthrough (2D pixel art)
    return aces(hdr);                              // ACES Narkowicz
}

// White-balance gain (#502): warm/cool (temperature) and magenta/green (tint)
// around a neutral 1.0 white point. 0 / 0 returns exactly (1, 1, 1), so the
// default path is bit-identical to the pre-effect shader.
float3 whiteBalanceGain()
{
    float t = pc.temperature;
    float m = pc.tint;
    return max(float3(
        1.0 + 0.30 * t + 0.20 * m,
        1.0 - 0.10 * t - 0.20 * m,
        1.0 - 0.30 * t + 0.20 * m), 0.0);
}

// Display-referred color grading (white balance + contrast around mid-grey +
// Rec.709 luma saturation). Applied *after* tonemapping so the knobs behave
// like a photo editor: the neutral settings are an exact no-op.
float3 grade(float3 c)
{
    c *= whiteBalanceGain();
    c = (c - 0.5) * pc.contrast + 0.5;
    float luma = dot(c, float3(0.2126, 0.7152, 0.0722));
    c = lerp(float3(luma, luma, luma), c, pc.saturation);
    return saturate(c);
}

// Deterministic monochrome noise for film grain (#502 Phase 2). The hash is
// taken from the pixel coordinate, so the pattern is spatially stable; an
// animated grain needs a time uniform this pass does not have yet.
float grainNoise(float2 pixel)
{
    float2 p = frac(pixel * float2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return frac(p.x * p.y);
}

// Soft-threshold a HDR sample: pixels brighter than `pc.bloomThreshold` keep
// the excess, dimmer pixels contribute nothing. `maxComponent` is cheaper than
// a luminance dot and catches coloured emitters (fire, neon) as well as white.
float3 brightPass(float3 hdr)
{
    float peak = max(hdr.r, max(hdr.g, hdr.b));
    float excess = max(peak - pc.bloomThreshold, 0.0);
    return hdr * (excess / max(peak, 1e-4));
}

// Single-pass bloom approximation: an 8-tap ring around the current pixel at
// two radii, weighted toward the centre. A true multi-level downsample chain
// is the next step (kept separate so this pass stays one fullscreen draw).
float3 bloom(float2 uv)
{
    float2 px = pc.invTexelSize;
    float3 sum = 0.0;
    // inner ring (4 taps, radius 2 px)
    sum += brightPass(hdrTex.Sample(uv + float2( 2.0,  0.0) * px).rgb) * 0.16;
    sum += brightPass(hdrTex.Sample(uv + float2(-2.0,  0.0) * px).rgb) * 0.16;
    sum += brightPass(hdrTex.Sample(uv + float2( 0.0,  2.0) * px).rgb) * 0.16;
    sum += brightPass(hdrTex.Sample(uv + float2( 0.0, -2.0) * px).rgb) * 0.16;
    // outer ring (4 diagonal taps, radius 5 px) catches wider halos
    sum += brightPass(hdrTex.Sample(uv + float2( 5.0,  5.0) * px).rgb) * 0.09;
    sum += brightPass(hdrTex.Sample(uv + float2(-5.0,  5.0) * px).rgb) * 0.09;
    sum += brightPass(hdrTex.Sample(uv + float2( 5.0, -5.0) * px).rgb) * 0.09;
    sum += brightPass(hdrTex.Sample(uv + float2(-5.0, -5.0) * px).rgb) * 0.09;
    return sum;
}

// Perspective depth in [0, 1] -> positive view-space distance in metres.
// Matches the engine's Vulkan projection (`near` maps to 0, `far` to 1).
float linearDepth(float depth)
{
    return pc.nearPlane * pc.farPlane
         / max(pc.farPlane - depth * (pc.farPlane - pc.nearPlane), 1e-4);
}

// Circle of confusion in pixels (#502 Phase 2). A pixel at the focus distance
// is sharp; the blur grows with the relative depth error, the aperture and the
// configured maximum radius.
float circleOfConfusion(float depth)
{
    float z = linearDepth(depth);
    float focus = max(pc.dofFocusDistance, 1e-3);
    float coc = saturate(abs(z - focus) / max(z, 1e-3) * pc.dofAperture);
    return coc * pc.dofMaxBlur;
}

// Fixed 12-tap golden-angle disk. The centre tap plus 11 ring samples keeps the
// effect a single fullscreen pass with no extra render target. `radius < 1`
// leaves the pixel untouched; aperture 0 never reaches this function.
float3 depthOfField(float2 uv, float3 sharp)
{
    float radius = circleOfConfusion(depthTex.Sample(uv).r);
    if (radius < 1.0)
        return sharp;

    const float TAU = 6.28318530718;
    float2 px = pc.invTexelSize;
    float3 sum = sharp;
    float weight = 1.0;
    for (int i = 1; i < 12; i++)
    {
        // sqrt distributes the taps evenly over the disk instead of clustering
        // them at the centre; the golden angle keeps successive taps apart.
        float r = sqrt(float(i) / 11.0) * radius;
        float a = float(i) * 2.39996323;
        float2 offset = float2(cos(a), sin(a)) * r * px;
        sum += hdrTex.Sample(uv + offset).rgb;
        weight += 1.0;
    }
    return sum / weight;
}

// Screen-space velocity for a static world point at `depth`. The current depth
// is enough because the scene is opaque: reconstruct the world position with
// the current inverse view-projection, then project it with the previous
// frame's view-projection. Missing / behind-camera previous positions return 0
// so a camera cut does not smear the whole frame.
float2 cameraMotionVector(float2 uv, float depth)
{
    float4 clip = float4(uv * 2.0 - 1.0, depth, 1.0);
    // Slang/HLSL treat `mul(vector, matrix)` as row-vector * matrix; the
    // matrices are uploaded column-major from Rust, so this is the correct
    // order for a constant buffer read on SPIR-V.
    float4 world = mul(clip, motion.invViewProj);
    if (abs(world.w) < 1e-6)
        return float2(0.0, 0.0);
    world /= world.w;
    float4 prev = mul(world, motion.prevViewProj);
    if (prev.w <= 1e-5)
        return float2(0.0, 0.0);
    float2 prevUv = prev.xy / prev.w * 0.5 + 0.5;
    return prevUv - uv;
}

// Fixed 8-tap trail along the camera-motion vector. `motionBlur = 0` never
// reaches this function; the trail is clamped to `motionBlurMaxPixels` so a
// fast camera turn stays a blur instead of sampling the whole screen.
float3 applyMotionBlur(float2 uv, float3 sharp)
{
    float2 velocity = cameraMotionVector(uv, depthTex.Sample(uv).r)
                    * pc.motionBlur;
    float2 maxUv = pc.motionBlurMaxPixels
                 / max(motion.resolution, float2(1.0, 1.0));
    velocity = clamp(velocity, -maxUv, maxUv);

    // Sub-pixel motion is visually identical to staying sharp; skip the taps
    // so the idle camera path costs only the depth sample above.
    if (length(velocity * motion.resolution) < 0.5)
        return sharp;

    float3 sum = sharp;
    float weight = 1.0;
    for (int i = 1; i < 8; i++)
    {
        float t = float(i) / 7.0;
        sum += hdrTex.Sample(uv - velocity * t).rgb;
        weight += 1.0;
    }
    return sum / weight;
}

// Sample + depth of field + motion blur + bloom + tonemap + grade. DOF and
// camera motion blur run in linear HDR before bloom so out-of-focus / moving
// highlights can still bloom. Bloom is applied before the tonemap so highlights
// can spill without being clipped first.
// FXAA runs entirely in this display-referred space so the edge-luma it
// compares is perceptually uniform (the hardware sRGB encode to the swapchain
// happens after this shader, so the values written back are still linear).
float3 shade(float2 uv)
{
    float3 hdr;
    if (pc.chromaticAberration > 0.0)
    {
        // Radial R/B split: zero at the centre, growing toward the frame edge,
        // like a real lens. The green channel stays centred so the image does
        // not shift. 0.01 keeps the maximum displacement at ~1% of the frame.
        float2 offset = (uv - 0.5) * pc.chromaticAberration * 0.01;
        hdr = float3(
            hdrTex.Sample(uv + offset).r,
            hdrTex.Sample(uv).g,
            hdrTex.Sample(uv - offset).b);
    }
    else
    {
        hdr = hdrTex.Sample(uv).rgb;
    }
    if (pc.dofAperture > 0.0)
        hdr = depthOfField(uv, hdr);
    if (pc.motionBlur > 0.0)
        hdr = applyMotionBlur(uv, hdr);
    if (pc.bloomIntensity > 0.0)
        hdr += bloom(uv) * pc.bloomIntensity;
    // Automatic exposure. The scene shaders already applied the camera's manual
    // exposure, which the metering pass divides back out, so this gain is
    // purely the difference between the metered scene luminance and middle
    // grey. It runs before the tonemap so the curve sees properly exposed HDR.
    hdr *= auto_exposure();
    return grade(saturate(tonemap(hdr)));
}

// Sample the 3D color-grading LUT at `c` (display-referred, [0, 1]).
// The texel-centre remap keeps the two outer slices from being clumped: a
// coordinate of exactly 0 / 1 would otherwise sample half a texel outside the
// cube and clamp, flattening the darkest / brightest slice.
float3 sampleLut(float3 c)
{
    float n = max(pc.lutSize, 2.0);
    float3 coord = saturate(c) * ((n - 1.0) / n) + (0.5 / n);
    return lutTex.Sample(coord).rgb;
}

// Perceptual luma for edge detection. Gamma 2.0 (sqrt) is used instead of the
// exact 2.2/sRGB curve: it is the cheap approximation FXAA luma is usually fed
// with, and each tap already costs a texture fetch plus a tonemap.
float perceptualLuma(float3 displayColor)
{
    float3 g = sqrt(max(displayColor, 0.0));
    return dot(g, float3(0.299, 0.587, 0.114));
}

// FXAA (Lottes' "quality" preset, simplified): find the dominant edge
// direction from a 2x2 luma cross, then blend along it with a short tap pair
// and fall back to a wider pair when the result leaves the local luma range.
float3 fxaa(float2 uv)
{
    float3 rgbM  = shade(uv);
    float2 px = pc.invTexelSize;

    float3 rgbNW = shade(uv + float2(-px.x, -px.y));
    float3 rgbNE = shade(uv + float2( px.x, -px.y));
    float3 rgbSW = shade(uv + float2(-px.x,  px.y));
    float3 rgbSE = shade(uv + float2( px.x,  px.y));

    float lumaM  = perceptualLuma(rgbM);
    float lumaNW = perceptualLuma(rgbNW);
    float lumaNE = perceptualLuma(rgbNE);
    float lumaSW = perceptualLuma(rgbSW);
    float lumaSE = perceptualLuma(rgbSE);

    float lumaMin = min(lumaM, min(min(lumaNW, lumaNE), min(lumaSW, lumaSE)));
    float lumaMax = max(lumaM, max(max(lumaNW, lumaNE), max(lumaSW, lumaSE)));

    // Contrast below this is a flat area - skip the extra taps entirely.
    if (lumaMax - lumaMin < max(0.0312, lumaMax * 0.125))
        return rgbM;

    float2 dir;
    dir.x = -((lumaNW + lumaNE) - (lumaSW + lumaSE));
    dir.y =  ((lumaNW + lumaSW) - (lumaNE + lumaSE));
    float dirReduce = max((lumaNW + lumaNE + lumaSW + lumaSE) * 0.03125, 0.0078125);
    float rcpDirMin = 1.0 / (min(abs(dir.x), abs(dir.y)) + dirReduce);
    dir = clamp(dir * rcpDirMin, float2(-8.0, -8.0), float2(8.0, 8.0)) * px;

    float3 rgbA = 0.5 * (
        shade(uv + dir * (1.0 / 3.0 - 0.5)) +
        shade(uv + dir * (2.0 / 3.0 - 0.5)));
    float3 rgbB = 0.5 * rgbA + 0.25 * (
        shade(uv + dir * -0.5) +
        shade(uv + dir *  0.5));

    float lumaB = perceptualLuma(rgbB);
    // The wide blend must not overshoot the local luma range (that is how FXAA
    // keeps thin geometry and text from smearing); otherwise use the narrow one.
    return ((lumaB < lumaMin) || (lumaB > lumaMax)) ? rgbA : rgbB;
}

[shader("fragment")]
public void fragmentMain(
    float4 clipPos : SV_Position,
    float2 uv : TEXCOORD0,
    out float4 outColor : SV_Target0
)
{
    float3 color = (pc.aaMode == 1u) ? fxaa(uv) : shade(uv);

    // 3D LUT color grading (#502). Runs after AA (so FXAA compares pre-LUT
    // edge luma) and before vignette / grain, like a photo-editor LUT layer.
    // `lutStrength == 0` skips the sample and the lerp entirely.
    if (pc.lutStrength > 0.0)
        color = lerp(color, sampleLut(color), saturate(pc.lutStrength));

    // Vignette: darken towards the frame corners. `vignette == 0` skips the
    // multiply entirely so the default path is bit-identical to pre-#502.
    if (pc.vignette > 0.0)
    {
        float2 d = uv * 2.0 - 1.0;
        float r = saturate(length(d) * 0.70710678);
        color *= 1.0 - pc.vignette * smoothstep(0.35, 1.0, r);
    }

    // Film grain (#502 Phase 2): monochrome noise over the final display-referred
    // image. `0` skips the hash entirely, keeping the default path bit-identical.
    if (pc.filmGrain > 0.0)
    {
        float noise = grainNoise(clipPos.xy) - 0.5;
        color = saturate(color + noise * pc.filmGrain * 0.25);
    }

    outColor = float4(color, 1.0);
}
