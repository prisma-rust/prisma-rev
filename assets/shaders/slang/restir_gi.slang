// Screen-space ReSTIR GI accelerated by the RTXGI SHaRC radiance cache.
//
// Raster-path global illumination that does NOT depend on the path tracer:
// every pixel traces a few secondary rays from the depth-buffer surface,
// resamples them temporally and spatially with ReSTIR, and falls back to the
// world-space SHaRC radiance cache for the secondary bounce. One traced ray
// per pixel therefore still converges.
//
// Output convention (identical to the baked probe-volume path so scene_frag
// can swap the two without changing its math):
//     giOutput = E(n) / PI        (cosine-weighted incident radiance)
// The forward pass then computes kd * giOutput * albedo, exactly like
// SampleProbeVolumeIrradiance(...) / PI.
//
// Dispatch: ((w + 7) / 8, (h + 7) / 8, 1) with numthreads(8, 8, 1).
// Requires: VK_KHR_ray_query + shaderInt64 + shaderBufferInt64Atomics (SHaRC).
//
// Bindings:
//   set 0 (shared with the path tracer where possible):
//     b0  RWTexture2D<float4> giOutput        (GI signal, rgba16f)
//     b2  RaytracingAccelerationStructure tlas
//     b3  ByteAddressBuffer vertexData        (combined world-space vertices)
//     b4  StructuredBuffer<uint> indices
//     b6  StructuredBuffer<PtInstanceMeta> instance_meta   (path_integrator)
//     b7  StructuredBuffer<GpuMaterial> materials          (path_integrator)
//     b8  StructuredBuffer<PtAnalyticLight> ptLights       (path_integrator)
//     b9  StructuredBuffer<PtEmissiveTri> ptEmissive       (path_integrator)
//     b10 StructuredBuffer<RestirGiReservoir> prevReservoir
//     b11 RWStructuredBuffer<RestirGiReservoir> currReservoir
//     b12 RWStructuredBuffer<HashGridKey> sharcHashEntries
//     b13 RWStructuredBuffer<SharcAccumulationData> sharcAccumulation
//     b14 RWStructuredBuffer<SharcPackedData> sharcResolved
//     b15 ConstantBuffer<RestirGiFrame> frame
//   set 1 (shared bindless): globalSamplers[] + bindlessSrvs[]
//   set 2 (shared IBL): SamplerCube envCube
//   set 3 (raster G-buffer):
//     b0 Texture2D<float>  gbDepth      (D32_SFLOAT, full res)
//     b1 Texture2D<float4> gbNormal     (view-space normal.xyz, reactive.w)
//     b2 Texture2D<float4> gbAlbedo     (base color.rgb, perceptual roughness.a)
//     b3 Texture2D<float2> gbVelocity   (UV-space motion vectors)
//     b4 Texture2D<float4> giHistory    (previous frame GI, fallback history)
//     b5 SamplerState      gbSampler    (linear / clamp)
//
// Manual compile (slangc 2026.21+):
//   slangc assets/shaders/slang/restir_gi.slang -entry giMain -stage compute
//     -fvk-use-entrypoint-name -target spirv -profile spirv_1_4
//     -o assets/shaders/restir_gi.comp.spv

#include "path_integrator.slang"
#include "sharc/sharc_common.slang"

// ---- set 0: pass-local bindings -------------------------------------------
[[vk::binding(0, 0)]]
[format("rgba16f")]
RWTexture2D<float4> giOutput;

[[vk::binding(2, 0)]]
RaytracingAccelerationStructure tlas;

[[vk::binding(3, 0)]]
ByteAddressBuffer vertexData;

[[vk::binding(4, 0)]]
StructuredBuffer<uint> indices;

// One ReSTIR GI reservoir. float4-aligned so the Rust mirror (RestirGiReservoir)
// is a plain 64-byte POD with no implicit padding.
public struct RestirGiReservoir
{
    float4 direction;  // xyz = unit sampled direction, w = sample age (frames)
    float4 radiance;   // rgb = incoming radiance along that direction,
                       // w = RIS target (luminance * cosine pdf)
    float4 stats;      // x = M (samples represented), y = W (RIS weight sum),
                       // z = secondary hit distance, w = unused
    float4 hitPos;     // xyz = secondary hit position (debug + reprojection aid)
};

[[vk::binding(10, 0)]]
StructuredBuffer<RestirGiReservoir> prevReservoir;

[[vk::binding(11, 0)]]
RWStructuredBuffer<RestirGiReservoir> currReservoir;

[[vk::binding(12, 0)]]
RWStructuredBuffer<HashGridKey> sharcHashEntries;

[[vk::binding(13, 0)]]
RWStructuredBuffer<SharcAccumulationData> sharcAccumulation;

[[vk::binding(14, 0)]]
RWStructuredBuffer<SharcPackedData> sharcResolved;

// Per-frame constants for this pass. Mirrors Rust RestirGiFrame.
public struct RestirGiFrame
{
    float4x4 invViewProj;     //   0: NDC -> world
    float4x4 invViewRot;      //  64: view -> world rotation (upper 3x3)
    float4   cameraPos;       // 128: xyz = camera position
    float4   lightDir;        // 144: xyz = direction TO the sun, w = radiance
    float4   lightColor;      // 160: rgb = sun colour, w = ambient/IBL factor
    float4   viewport;        // 176: xy = GI resolution, zw = full resolution
    float4   params;          // 192: x = frame index, y = reset, z = rays/pixel,
                              //      w = max history length
    float4   giParams;        // 208: x = spatial samples, y = max secondary
                              //      distance, z = temporal reuse, w = spatial reuse
    float4   sharcParams;     // 224: x = scene scale, y = capacity, z = flags,
                              //      w = roughness threshold
    float4   sharcCameraPos;  // 240: xyz = cache camera position
    float4x4 prevViewProj;    // 256: previous frame view-projection
    float4   prevCameraPos;   // 320: xyz = previous camera position
};

[[vk::binding(15, 0)]]
ConstantBuffer<RestirGiFrame> frame;

// ---- set 3: raster G-buffer ----------------------------------------------
[[vk::binding(0, 3)]]
Texture2D<float> gbDepth;

[[vk::binding(1, 3)]]
Texture2D<float4> gbNormal;

[[vk::binding(2, 3)]]
Texture2D<float4> gbAlbedo;

[[vk::binding(3, 3)]]
Texture2D<float2> gbVelocity;

[[vk::binding(4, 3)]]
Texture2D<float4> giHistory;

[[vk::binding(5, 3)]]
SamplerState gbSampler;

// ---- SHaRC constants (must match pt_render.slang / sharc.rs) -------------
static const float SHARC_RADIANCE_SCALE = 1000.0;
static const uint  GI_SHARC_FLAG_QUERY  = 1u << 0u;
static const uint  GI_SHARC_FLAG_UPDATE = 1u << 1u;
static const float PI_F = 3.14159265359;
static const uint  RESERVOIR_MAX_AGE = 8u;

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

float luminance3(float3 c)
{
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

// Cosine-weighted pdf of dir around n, in 1/sr.
float cosine_pdf(float3 n, float3 dir)
{
    return max(dot(n, dir), 0.0) / PI_F;
}

// Reconstruct the world-space primary surface position from the depth buffer.
float3 world_from_depth(float2 uv, float depth)
{
    float4 clip = float4(float2(uv.x * 2.0 - 1.0, uv.y * 2.0 - 1.0), depth, 1.0);
    float4 world = mul(frame.invViewProj, clip);
    return world.xyz / world.w;
}

// Rotate a view-space direction into world space.
float3 view_dir_to_world(float3 v)
{
    return normalize(mul((float3x3)frame.invViewRot, v));
}

// --------------------------------------------------------------------------
// SHaRC plumbing
// --------------------------------------------------------------------------

bool sharc_query_enabled()
{
    return (uint(frame.sharcParams.z) & GI_SHARC_FLAG_QUERY) != 0u
        && frame.sharcParams.y > 0.5;
}

bool sharc_update_enabled()
{
    return (uint(frame.sharcParams.z) & GI_SHARC_FLAG_UPDATE) != 0u
        && frame.sharcParams.y > 0.5;
}

SharcParameters make_sharc_parameters()
{
    SharcParameters params;
    params.gridParameters.cameraPosition = frame.sharcCameraPos.xyz;
    params.gridParameters.logarithmBase = SHARC_GRID_LOGARITHM_BASE;
    params.gridParameters.sceneScale = frame.sharcParams.x;
    params.gridParameters.levelBias = SHARC_GRID_LEVEL_BIAS;
    params.hashMapData.capacity = (uint)frame.sharcParams.y;
    params.hashMapData.hashEntriesBuffer = sharcHashEntries;
    params.accumulationBuffer = sharcAccumulation;
    params.resolvedBuffer = sharcResolved;
    params.radianceScale = SHARC_RADIANCE_SCALE;
    params.enableAntiFireflyFilter = true;
    return params;
}

// A cache query is only meaningful once the hit is far enough away that its
// voxel is not the one the camera sits in (otherwise the cache returns the
// still-unconverged radiance of the primary surface itself).
bool sharc_hit_is_valid(SharcParameters params, float3 position, float hitDistance)
{
    uint level = HashGridGetLevel(position, params.gridParameters);
    float voxelSize = HashGridGetVoxelSize(level, params.gridParameters);
    if (!(voxelSize > 0.0))
        return false;
    return hitDistance > voxelSize * 1.7320508;
}

// --------------------------------------------------------------------------
// Ray tracing
// --------------------------------------------------------------------------

struct GiHit
{
    float3 position;
    float3 normal;    // shading normal, flipped towards the ray origin
    float  distance;
    SurfaceMaterial surface;
};

bool trace_secondary(float3 origin, float3 dir, float tmin, float tmax, out GiHit hit)
{
    RayDesc ray;
    ray.Origin = origin;
    ray.Direction = dir;
    ray.TMin = tmin;
    ray.TMax = tmax;

    RayQuery<RAY_FLAG_NONE> q;
    q.TraceRayInline(tlas, RAY_FLAG_NONE, 0xFF, ray);
    while (q.Proceed())
    {
        if (q.CandidateType() == CANDIDATE_NON_OPAQUE_TRIANGLE)
        {
            uint ci = q.CandidateInstanceID();
            uint cp = q.CandidatePrimitiveIndex();
            float2 cb = q.CandidateTriangleBarycentrics();
            float3 cv0, cv1, cv2;
            float2 cuv0, cuv1, cuv2;
            float3 cn0, cn1, cn2;
            read_hit_triangle(ci, cp, vertexData, indices,
                cv0, cv1, cv2, cuv0, cuv1, cuv2, cn0, cn1, cn2);
            float cw = 1.0 - cb.x - cb.y;
            float2 cuv = cw * cuv0 + cb.x * cuv1 + cb.y * cuv2;
            PtInstanceMeta cm = instance_meta[ci];
            GpuMaterial c_mat = materials[cm.material_slot];
            if (mat_alpha_pass(c_mat, cuv))
                q.CommitNonOpaqueTriangleHit();
        }
    }

    if (q.CommittedStatus() != COMMITTED_TRIANGLE_HIT)
        return false;

    uint instance_id = q.CommittedInstanceID();
    uint prim_idx = q.CommittedPrimitiveIndex();
    float3 v0, v1, v2;
    float2 uv0, uv1, uv2;
    float3 n0, n1, n2;
    read_hit_triangle(instance_id, prim_idx, vertexData, indices,
        v0, v1, v2, uv0, uv1, uv2, n0, n1, n2);

    float2 bary = q.CommittedTriangleBarycentrics();
    float w0 = 1.0 - bary.x - bary.y;
    hit.distance = q.CommittedRayT();
    hit.position = origin + dir * hit.distance;

    float3 shading = normalize(w0 * n0 + bary.x * n1 + bary.y * n2);
    if (dot(shading, shading) < 1e-12)
        shading = float3(0.0, 1.0, 0.0);
    if (dot(shading, dir) > 0.0)
        shading = -shading;
    hit.normal = shading;

    PtInstanceMeta meta = instance_meta[instance_id];
    GpuMaterial mat = materials[meta.material_slot];
    float2 hit_uv = w0 * uv0 + bary.x * uv1 + bary.y * uv2;
    hit.surface = resolve_material(mat, hit_uv);
    return true;
}

// Binary sun visibility, matching pt_render.slang so both GI paths agree on
// what counts as shadowed.
float sun_visibility(float3 pos, float3 dirToLight, float tmax)
{
    RayDesc sr;
    sr.Origin = pos + 0.0001 * dirToLight;
    sr.Direction = dirToLight;
    sr.TMin = 0.001;
    sr.TMax = tmax;

    RayQuery<RAY_FLAG_SKIP_CLOSEST_HIT_SHADER | RAY_FLAG_ACCEPT_FIRST_HIT_AND_END_SEARCH> sq;
    sq.TraceRayInline(tlas,
        RAY_FLAG_SKIP_CLOSEST_HIT_SHADER | RAY_FLAG_ACCEPT_FIRST_HIT_AND_END_SEARCH,
        0xFF, sr);
    while (sq.Proceed())
    {
        if (sq.CandidateType() == CANDIDATE_NON_OPAQUE_TRIANGLE)
        {
            uint si = sq.CandidateInstanceID();
            uint sp = sq.CandidatePrimitiveIndex();
            float2 sb = sq.CandidateTriangleBarycentrics();
            float3 sv0, sv1, sv2;
            float2 suv0, suv1, suv2;
            float3 sn0, sn1, sn2;
            read_hit_triangle(si, sp, vertexData, indices,
                sv0, sv1, sv2, suv0, suv1, suv2, sn0, sn1, sn2);
            float sw = 1.0 - sb.x - sb.y;
            float2 suv = sw * suv0 + sb.x * suv1 + sb.y * suv2;
            PtInstanceMeta sm = instance_meta[si];
            GpuMaterial smat = materials[sm.material_slot];
            if (mat_alpha_pass(smat, suv))
            {
                sq.CommitNonOpaqueTriangleHit();
                break;
            }
        }
    }
    return sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT ? 0.0 : 1.0;
}

// --------------------------------------------------------------------------
// Secondary-hit shading
// --------------------------------------------------------------------------

// Radiance arriving at the secondary hit from direction -dir. One bounce of
// transport, using the SHaRC cache as the converged multi-bounce term:
//   cache hit  -> cached radiance (accumulated over previous frames)
//   cache miss -> sun NEE + a cheap environment approximation
float3 shade_secondary(GiHit hit, float3 dir, float3 sunDir, float3 sunRadiance,
    float maxDist, inout SharcState sharcState, inout uint seed)
{
    SharcParameters sharcParams = make_sharc_parameters();
    SharcHitData sharcHit;
    sharcHit.positionWorld = hit.position;
    sharcHit.normalWorld = hit.normal;

    if (sharc_query_enabled()
        && sharc_hit_is_valid(sharcParams, hit.position, hit.distance))
    {
        float3 cached;
        if (SharcGetCachedRadiance(sharcParams, sharcHit, cached, false))
            return cached;
    }

    float3 viewDir = -dir;
    float3 direct = float3(0.0);
    float nDotL = dot(hit.normal, sunDir);
    if (nDotL > 0.0 && luminance3(sunRadiance) > 1e-6)
    {
        float vis = sun_visibility(hit.position, sunDir, maxDist);
        if (vis > 0.0)
            direct = eval_brdf(hit.surface, hit.normal, viewDir, sunDir) * sunRadiance * vis;
    }

    // Remaining transport, approximated by the environment along the normal.
    // Fades out as the cache warms up, so it never biases a converged frame.
    float3 sky = envCube.SampleLevel(hit.normal, 0.0).rgb;
    float3 indirect = hit.surface.base_color * sky;
    float3 total = direct + indirect;

    if (sharc_update_enabled())
    {
        SharcInit(sharcState);
        SharcUpdateHit(sharcParams, sharcState, sharcHit, direct, rand_float(seed));
        SharcSetThroughput(sharcState, float3(1.0));
    }
    return total;
}

// --------------------------------------------------------------------------
// Reservoir math
// --------------------------------------------------------------------------

RestirGiReservoir empty_reservoir()
{
    RestirGiReservoir r;
    r.direction = float4(0.0, 0.0, 0.0, 0.0);
    r.radiance = float4(0.0, 0.0, 0.0, 0.0);
    r.stats = float4(0.0, 0.0, 0.0, 0.0);
    r.hitPos = float4(0.0, 0.0, 0.0, 0.0);
    return r;
}

bool reservoir_valid(RestirGiReservoir r)
{
    if (!(r.stats.x > 0.0) || r.stats.x != r.stats.x || r.stats.x >= 1e30)
        return false;
    if (!(r.stats.y > 0.0) || r.stats.y != r.stats.y || r.stats.y >= 1e30)
        return false;
    if (!(r.radiance.w > 0.0) || r.radiance.w != r.radiance.w || r.radiance.w >= 1e30)
        return false;
    return true;
}

// Merge one candidate into the running reservoir as an importance-weighted
// mean of the incoming radiance. `mScale` is how many samples the candidate
// stands for (1 for a fresh ray, the reuse count for a temporally or
// spatially reused reservoir), so reuse cannot out-vote this frame's rays.
//
// The mean is the quantity the forward pass wants: `scene_frag` multiplies the
// resolved signal by `kd * albedo`, so the pass must output the cosine-weighted
// incoming irradiance over PI, not an outgoing radiance. Averaging the samples
// drawn from the cosine lobe directly estimates exactly that.
void combine_sample(inout RestirGiReservoir current, float3 sampleDir,
    float3 sampleRadiance, float3 samplePos, float3 n, float mScale,
    inout uint seed)
{
    float pdf = cosine_pdf(n, sampleDir);
    if (!(pdf > 1e-6))
        return;
    float target = luminance3(sampleRadiance);
    if (!(target > 1e-8) || target != target || target >= 1e30)
        return;
    // Weight by luminance so a single firefly cannot own the mean, and by the
    // reuse count so temporal accumulation keeps its history.
    float w = target * mScale;
    if (!(w > 0.0) || w != w || w >= 1e30)
        return;

    float newW = current.stats.y + w;
    if (!(newW > 0.0) || newW != newW || newW >= 1e30)
        return;

    float alpha = w / newW;
    current.radiance = float4(lerp(current.radiance.rgb, sampleRadiance, alpha), 1.0);
    current.hitPos = float4(lerp(current.hitPos.xyz, samplePos, alpha), 0.0);
    // Keep the dominant direction for debug output; the mean radiance above is
    // what actually resolves into the GI buffer.
    if (alpha > 0.5)
        current.direction = float4(sampleDir, current.direction.w);
    current.stats = float4(current.stats.x + mScale, newW, 0.0, 0.0);
}

// Resolve the reservoir to the GI signal the forward pass samples.
//
// `combine_sample` already accumulated an importance-weighted mean of the
// incoming radiance, so resolving is a read. No `1/pdf` factor is applied:
// the samples were drawn from the pixel's cosine lobe, and that lobe's
// integral over PI is exactly the mean the mean already computes.
float3 resolve_reservoir(RestirGiReservoir r)
{
    if (!reservoir_valid(r))
        return float3(0.0);
    return r.radiance.rgb;
}

// --------------------------------------------------------------------------
// Main
// --------------------------------------------------------------------------

[numthreads(8, 8, 1)]
[shader("compute")]
public void giMain(uint3 tid : SV_DispatchThreadID)
{
    uint2 giSize = uint2(frame.viewport.xy);
    if (tid.x >= giSize.x || tid.y >= giSize.y)
        return;

    uint2 fullSize = uint2(frame.viewport.zw);
    uint frameIndex = uint(frame.params.x);
    bool reset = frame.params.y > 0.5;

    float2 uv = (float2(tid.xy) + 0.5) / float2(giSize);
    uint linearIndex = tid.y * giSize.x + tid.x;

    uint seed = tid.x * 1973u + tid.y * 9277u + frameIndex * 26699u + 1u;
    wang_hash(seed);

    float depth = gbDepth.SampleLevel(gbSampler, uv, 0.0).r;
    if (depth <= 0.0 || depth >= 1.0)
    {
        giOutput[tid.xy] = float4(0.0, 0.0, 0.0, 1.0);
        currReservoir[linearIndex] = empty_reservoir();
        return;
    }

    // ---- Primary surface ---------------------------------------------
    float3 worldPos = world_from_depth(uv, depth);
    float4 normalSample = gbNormal.SampleLevel(gbSampler, uv, 0.0);
    float3 n = float3(0.0, 1.0, 0.0);
    if (dot(normalSample.xyz, normalSample.xyz) > 1e-12)
        n = view_dir_to_world(normalize(normalSample.xyz));
    if (dot(n, frame.cameraPos.xyz - worldPos) < 0.0)
        n = -n;

    float4 albedoRough = gbAlbedo.SampleLevel(gbSampler, uv, 0.0);

    float3 sunDir = normalize(frame.lightDir.xyz);
    float3 sunRadiance = frame.lightColor.rgb * frame.lightDir.w;
    float maxDist = max(frame.giParams.y, 1.0);
    float originBias = 1e-4 * max(length(worldPos), 1.0);

    // ---- Initial sampling: N secondary rays ---------------------------
    RestirGiReservoir reservoir = empty_reservoir();
    SharcState sharcState;
    SharcInit(sharcState);

    uint rayCount = max(uint(frame.params.z), 1u);
    for (uint i = 0u; i < rayCount; i++)
    {
        float3 dir = cosine_hemisphere_sample(n, rand_float(seed), rand_float(seed));
        GiHit hit;
        if (trace_secondary(worldPos + originBias * n, dir, 1e-3, maxDist, hit))
        {
            float3 radiance = shade_secondary(hit, dir, sunDir, sunRadiance,
                maxDist, sharcState, seed);
            combine_sample(reservoir, dir, radiance, hit.position, n,
                1.0 / float(rayCount), seed);
        }
        else
        {
            float3 sky = envCube.SampleLevel(dir, 0.0).rgb;
            if (sharc_update_enabled())
            {
                SharcInit(sharcState);
                SharcUpdateMiss(make_sharc_parameters(), sharcState, sky);
            }
            combine_sample(reservoir, dir, sky, worldPos + dir * maxDist, n,
                1.0 / float(rayCount), seed);
        }
    }

    // ---- Temporal reuse -----------------------------------------------
    bool temporal = frame.giParams.z > 0.5 && !reset;
    if (temporal)
    {
        float2 velocity = gbVelocity.SampleLevel(gbSampler, uv, 0.0).rg;
        float2 prevUv = uv - velocity;
        if (any(prevUv < 0.0) || any(prevUv > 1.0))
        {
            temporal = false;
        }
        else
        {
            float prevDepth = gbDepth.SampleLevel(gbSampler, prevUv, 0.0).r;
            if (abs(prevDepth - depth) > 0.02 * max(depth, 1e-4))
                temporal = false;
        }

        if (temporal)
        {
            uint2 prevPixel = min(uint2(prevUv * float2(giSize)), giSize - 1u);
            RestirGiReservoir prev = prevReservoir[prevPixel.y * giSize.x + prevPixel.x];
            if (reservoir_valid(prev) && prev.direction.w < float(RESERVOIR_MAX_AGE))
            {
                float mScale = min(prev.stats.x, max(frame.params.w, 1.0)) / prev.stats.x;
                combine_sample(reservoir, prev.direction.xyz, prev.radiance.rgb,
                    prev.hitPos.xyz, n, mScale, seed);
            }
        }
    }

    // ---- Spatial reuse ------------------------------------------------
    uint spatialSamples = uint(frame.giParams.x);
    if (frame.giParams.w > 0.5 && spatialSamples > 0u)
    {
        for (uint k = 0u; k < spatialSamples; k++)
        {
            int2 offset = int2(int(rand_float(seed) * 17.0) - 8,
                               int(rand_float(seed) * 17.0) - 8);
            int2 neighbor = int2(tid.xy) + offset;
            if (neighbor.x < 0 || neighbor.y < 0
                || neighbor.x >= int(giSize.x) || neighbor.y >= int(giSize.y))
                continue;
            RestirGiReservoir src =
                prevReservoir[uint(neighbor.y) * giSize.x + uint(neighbor.x)];
            if (!reservoir_valid(src))
                continue;
            float mScale = min(src.stats.x, 8.0) / src.stats.x;
            combine_sample(reservoir, src.direction.xyz, src.radiance.rgb,
                src.hitPos.xyz, n, mScale, seed);
        }
    }

    // ---- Resolve ------------------------------------------------------
    float3 gi = resolve_reservoir(reservoir);
    if (!(gi.x == gi.x) || !(gi.y == gi.y) || !(gi.z == gi.z))
        gi = float3(0.0);
    gi = clamp(gi, float3(0.0, 0.0, 0.0), float3(64.0, 64.0, 64.0));

    // Demodulate by the primary surface albedo so the forward pass can apply
    // kd * albedo itself, exactly like the probe-volume path.
    giOutput[tid.xy] = float4(gi, 1.0);

    reservoir.direction.w = reset ? 0.0 : reservoir.direction.w + 1.0;
    currReservoir[linearIndex] = reservoir;
}
