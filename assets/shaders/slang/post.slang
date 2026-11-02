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
//
// Push constants: PostPush - `tonemapMode` (0 = Reinhard, 1 = ACES,
// 2 = passthrough for 2D scenes so the sprite palette survives untouched),
// `aaMode` (FXAA), `invTexelSize`, the color-grading knobs, HDR bloom controls,
// and the Phase 2 chromatic-aberration strength (#502).
//
// Compile:
//   slangc post.slang -profile spirv_1_5 -target spirv \
//          -entry vertexMain -stage vertex   -fvk-use-entrypoint-name -o post.vert.spv
//   slangc post.slang -profile spirv_1_5 -target spirv \
//          -entry fragmentMain -stage fragment -fvk-use-entrypoint-name -o post.frag.spv

// ---- set 0, binding 0 - HDR color attachment (combined image+sampler).
[[vk::binding(0, 0)]]
Sampler2D hdrTex;

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

// Display-referred color grading (contrast around mid-grey + Rec.709 luma
// saturation). Applied *after* tonemapping so the knobs behave like a photo
// editor: contrast 1 / saturation 1 is an exact no-op.
float3 grade(float3 c)
{
    c = (c - 0.5) * pc.contrast + 0.5;
    float luma = dot(c, float3(0.2126, 0.7152, 0.0722));
    c = lerp(float3(luma, luma, luma), c, pc.saturation);
    return saturate(c);
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

// Sample + bloom + tonemap + grade. Bloom is applied in linear HDR before the
// tonemap so highlights can spill without being clipped first. FXAA runs
// entirely in this display-referred space so the edge-luma it compares is
// perceptually uniform (the hardware sRGB encode to the swapchain happens
// after this shader, so the values written back are still linear).
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
    if (pc.bloomIntensity > 0.0)
        hdr += bloom(uv) * pc.bloomIntensity;
    return grade(saturate(tonemap(hdr)));
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

    // Vignette: darken towards the frame corners. `vignette == 0` skips the
    // multiply entirely so the default path is bit-identical to pre-#502.
    if (pc.vignette > 0.0)
    {
        float2 d = uv * 2.0 - 1.0;
        float r = saturate(length(d) * 0.70710678);
        color *= 1.0 - pc.vignette * smoothstep(0.35, 1.0, r);
    }

    outColor = float4(color, 1.0);
}
