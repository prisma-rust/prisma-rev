// Temporal anti-aliasing resolve (#883).
//
// Reads the current HDR color + depth and the previous frame's history color,
// reprojects the current pixel with the previous unjittered view-projection,
// clamps the history to the current 3x3 neighborhood, and blends.
//
// Sets (must match `TaaPass::ensure_pipeline`):
//   set 0, binding 0 - current HDR color (Sampler2D)
//   set 0, binding 1 - current scene depth (Sampler2D, D32_SFLOAT)
//   set 0, binding 2 - previous-frame HDR history (Sampler2D)
//   set 0, binding 3 - TaaParams uniform buffer
//   set 0, binding 4 - current screen-space velocity (Sampler2D, RG16F)
//   set 0, binding 5 - view-space normal + reactive mask (Sampler2D)
//
// Compile:
//   slangc taa.slang -profile spirv_1_4 -target spirv \
//          -entry vertexMain -stage vertex -entry fragmentMain -stage fragment \
//          -fvk-use-entrypoint-name -o taa.spv

[[vk::binding(0, 0)]]
Sampler2D colorTex;
[[vk::binding(1, 0)]]
Sampler2D depthTex;
[[vk::binding(2, 0)]]
Sampler2D historyTex;

struct TaaParams
{
    float4x4 invViewProj;
    float4x4 prevViewProj;
    float2 resolution;
    float strength;       // history weight, 0 = current only
    float validHistory;   // 0 on first frame / after resize / camera cut
};
[[vk::binding(3, 0)]]
ConstantBuffer<TaaParams> params;
// #883 Phase 2: per-object motion vectors from ForwardPass SV_Target2.
[[vk::binding(4, 0)]]
Sampler2D velocityTex;
// #883 Phase 2: normal.w is 1 for transparent/reactive pixels.
[[vk::binding(5, 0)]]
Sampler2D normalTex;

struct VertexOut
{
    float4 clipPos : SV_Position;
    float2 uv : TEXCOORD0;
};

[shader("vertex")]
VertexOut vertexMain(uint vid : SV_VertexID)
{
    VertexOut out;
    float2 pos = float2(
        (vid == 1u) ? 3.0 : -1.0,
        (vid == 2u) ? 3.0 : -1.0
    );
    out.clipPos = float4(pos, 0.0, 1.0);
    out.uv = pos * 0.5 + 0.5;
    return out;
}

float2 reproject(float2 uv, float depth)
{
    float4 clip = float4(uv * 2.0 - 1.0, depth, 1.0);
    // See post.slang: Slang/HLSL treats `mul(vector, matrix)` as row-vector *
    // matrix; the CPU uploads column-major matrices.
    float4 world = mul(clip, params.invViewProj);
    if (abs(world.w) < 1e-6)
        return float2(-1.0, -1.0);
    world /= world.w;
    float4 prev = mul(world, params.prevViewProj);
    if (prev.w <= 1e-5)
        return float2(-1.0, -1.0);
    return prev.xy / prev.w * 0.5 + 0.5;
}

float3 neighborhoodMin(float2 uv)
{
    float2 px = 1.0 / params.resolution;
    float3 m = colorTex.Sample(uv).rgb;
    for (int y = -1; y <= 1; y++)
        for (int x = -1; x <= 1; x++)
            m = min(m, colorTex.Sample(uv + float2(x, y) * px).rgb);
    return m;
}

float3 neighborhoodMax(float2 uv)
{
    float2 px = 1.0 / params.resolution;
    float3 m = colorTex.Sample(uv).rgb;
    for (int y = -1; y <= 1; y++)
        for (int x = -1; x <= 1; x++)
            m = max(m, colorTex.Sample(uv + float2(x, y) * px).rgb);
    return m;
}

[shader("fragment")]
float4 fragmentMain(VertexOut input) : SV_Target
{
    float2 uv = input.uv;
    float depth = depthTex.Sample(uv).r;
    float3 current = colorTex.Sample(uv).rgb;

    if (params.validHistory < 0.5 || params.strength <= 0.0)
        return float4(current, 1.0);

    // #883 Phase 2: prefer the rasterized per-object velocity. It already
    // includes camera motion and rigid-object motion; the camera-only
    // reprojection remains the fallback for pixels the forward pass did not
    // cover (e.g. skybox / no motion vector).
    float2 velocity = velocityTex.Sample(uv).rg;
    float reactive = normalTex.Sample(uv).a;
    if (reactive > 0.5)
        return float4(current, 1.0); // transparent: do not reuse history

    float2 prevUv = reproject(uv, depth);
    if (any(abs(velocity) > 1e-7))
        prevUv = uv - velocity;
    if (any(prevUv < 0.0) || any(prevUv > 1.0))
        return float4(current, 1.0);

    // Reject history when the reprojected depth disagrees (disocclusion).
    // Skip this test for fast-moving pixels: the current depth at their
    // previous location may legitimately belong to a different surface.
    float2 motionPixels = velocity * params.resolution;
    if (length(motionPixels) < 0.5)
    {
        float prevDepth = depthTex.Sample(prevUv).r;
        if (abs(prevDepth - depth) > 0.02)
            return float4(current, 1.0);
    }

    float3 history = historyTex.Sample(prevUv).rgb;
    history = clamp(history, neighborhoodMin(uv), neighborhoodMax(uv));
    float t = saturate(params.strength);
    return float4(lerp(current, history, t), 1.0);
}
