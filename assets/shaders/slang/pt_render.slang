// Real-time path-tracing renderer with ReSTIR DI — compute shader with
// temporal accumulation.
//
// Each frame traces ONE sample per pixel and accumulates the result across
// frames (1 sample/frame -> noise converges over time).
//
// Dispatch: ((width+15)/16, (height+15)/16, 1) with numthreads(16,16,1).
// Requires: VK_KHR_ray_query (RTX hardware).
//
// Output: The accumulated radiance ÷ sample count is written to
// the output image (PT_COLOR_H in the render graph), which PostPass tonemaps.
//
// Bindings:
//   set 0 (PT-local):
//     b0 RWTexture2D<float4> accumImage
//     b1 RWTexture2D<uint>   sampleCount
//     b2 AccelerationStructure tlas
//     b3 ByteAddressBuffer vertexData (combined world-space vertices)
//     b4 StructuredBuffer<uint> indices (combined index buffer)
//     b5 RWTexture2D<float4> outputImage
//     b6 StructuredBuffer<PtInstanceMeta> instance_meta (module-global)
//     b7 StructuredBuffer<GpuMaterial> materials (module-global)
//     b8 StructuredBuffer<PtAnalyticLight> ptLights (module-global)
//     b9 StructuredBuffer<PtEmissiveTri> ptEmissive (module-global)
//    b10 StructuredBuffer<ReSTIRReservoir> prevReservoir (temporal reuse)
//    b11 RWStructuredBuffer<ReSTIRReservoir> currReservoir (write for next frame)
//   set 1 (shared bindless): globalSamplers[] + bindlessSrvs[] (module-global)
// The b6/b7/b8/b9 + set 1 bindings are declared in path_integrator.slang (imported).

#include "path_integrator.slang"
#include "sharc/sharc_common.slang"

// ---- ReSTIR constants ----
static const uint RESTIR_M = 8u;               // RIS candidates per pixel
static const uint RESTIR_SPATIAL_SAMPLES = 4u; // spatial reuse neighbors
static const float RESTIR_TEMPORAL_M_CAP = 32.0;
static const float RESTIR_SPATIAL_M_CAP = 8.0;

// ---- Push constants ----
struct PtPush
{
    float4x4 inv_view_proj;  // 0: NDC -> world
    float4   camera_pos;     // 64: .xyz = camera pos, .w = exposure
    float4   light_dir;      // 80: xyz = dir TO light, w = pre-scaled radiance
    float4   light_color;    // 96: rgb = directional color, w = ambient factor
    uint4    params;         // 112: x=w, y=h, z=max_bounce, w=packed(frame|ortho<<30|reset<<31)
    float    ray_max_distance; // 128: max ray length
    uint     max_iterations;   // 132: 0 = accumulate forever
    uint     num_lights;       // 136: analytic lights in ptLights
    uint     num_emissive;     // 140: emissive triangles in ptEmissive
    // SHARC (RTXGI spatial hash radiance cache). sharc_flags packs both
    // switches and the deposit stride in one word: bit 0 = query the cache,
    // bit 1 = deposit into it, bits 8..15 = deposit stride (1 = every pixel).
    // Bit 0 is clear whenever gi_mode is not Sharc, in which case the cache
    // buffers are unbound and must not be touched.
    float    sharc_scene_scale;        // 144: hash-grid scene scale
    uint     sharc_capacity;           // 148: hash-grid entry count
    uint     sharc_flags;              // 152: bit 0 = query, bit 1 = update
    float    sharc_roughness_threshold;// 156: roughness clamped before caching
    float4   sharc_camera_pos;         // 160: .xyz = cache camera position
};

// ---- Descriptors (set 0) ----
[[vk::binding(0, 0)]]
[format("rgba32f")]
RWTexture2D<float4> accumImage;

[[vk::binding(1, 0)]]
[format("r32ui")]
RWTexture2D<uint> sampleCount;

[[vk::binding(2, 0)]]
RaytracingAccelerationStructure tlas;

[[vk::binding(3, 0)]]
ByteAddressBuffer vertexData;

[[vk::binding(4, 0)]]
StructuredBuffer<uint> indices;

[[vk::binding(5, 0)]]
[format("rgba32f")]
RWTexture2D<float4> outputImage;

[[vk::binding(10, 0)]]
StructuredBuffer<ReSTIRReservoir> prevReservoir;

[[vk::binding(11, 0)]]
RWStructuredBuffer<ReSTIRReservoir> currReservoir;

// SHARC cache buffers (written by this pass, resolved by SharcResolvePass
// before the next frame's trace). Only accessed when push.sharc_enabled != 0.
[[vk::binding(12, 0)]]
RWStructuredBuffer<HashGridKey> sharcHashEntries;

[[vk::binding(13, 0)]]
RWStructuredBuffer<SharcAccumulationData> sharcAccumulation;

[[vk::binding(14, 0)]]
RWStructuredBuffer<SharcPackedData> sharcResolved;

[[vk::push_constant]]
PtPush push;

// ---- params.w bit layout (mirrored by `PT_PARAMS_*` in `pt_pass.rs`) ----
static const uint PT_PARAMS_FRAME_MASK = 0x3FFFFFFFu; // bits 0..29: frame counter
static const uint PT_PARAMS_ORTHOGRAPHIC = 1u << 30u; // bit 30: orthographic camera
static const uint PT_PARAMS_RESET = 1u << 31u;        // bit 31: clear accumulation

// ---- SHARC ----
// Radiance is accumulated as `value * radianceScale` in fixed point, so the
// update and the resolve kernel must agree on the same scale (1000.0, as in
// SharcResolvePass). Material demodulation is disabled in sharc_common.slang,
// so the cache stores radiance directly and the sample weights stay 1.
static const float SHARC_RADIANCE_SCALE = 1000.0;
static const uint SHARC_FLAG_QUERY = 1u << 0u;
static const uint SHARC_FLAG_UPDATE = 1u << 1u;
static const uint SHARC_FLAG_STRIDE_SHIFT = 8u;
static const uint SHARC_FLAG_STRIDE_MASK = 0xFFu;

SharcParameters make_sharc_parameters()
{
    SharcParameters params;
    params.gridParameters.cameraPosition = push.sharc_camera_pos.xyz;
    params.gridParameters.logarithmBase = SHARC_GRID_LOGARITHM_BASE;
    params.gridParameters.sceneScale = push.sharc_scene_scale;
    params.gridParameters.levelBias = SHARC_GRID_LEVEL_BIAS;

    params.hashMapData.capacity = push.sharc_capacity;
    params.hashMapData.hashEntriesBuffer = sharcHashEntries;

    params.accumulationBuffer = sharcAccumulation;
    params.resolvedBuffer = sharcResolved;
    params.radianceScale = SHARC_RADIANCE_SCALE;
    params.enableAntiFireflyFilter = true;
    return params;
}

// ---- Ray generation ----
// NDC z = 0 is the near plane and z = 1 the far plane (Vulkan depth range
// [0,1]), so unprojecting both gives the ray through this pixel; near→far is
// the camera forward. A perspective camera starts every ray at the eye, while
// an orthographic camera casts parallel rays from the pixel's near-plane point.
void generate_ray(uint px, uint py, float jx, float jy,
    out float3 origin, out float3 dir)
{
    uint w = push.params.x;
    uint h = push.params.y;
    float ndc_x = ((float(px) + 0.5 + jx) / float(w)) * 2.0 - 1.0;
    float ndc_y = ((float(py) + 0.5 + jy) / float(h)) * 2.0 - 1.0;
    float4 world_near = mul(push.inv_view_proj, float4(ndc_x, ndc_y, 0.0, 1.0));
    float4 world_far = mul(push.inv_view_proj, float4(ndc_x, ndc_y, 1.0, 1.0));
    float3 near_pt = world_near.xyz / world_near.w;
    float3 far_pt = world_far.xyz / world_far.w;
    float3 forward = far_pt - near_pt;
    dir = normalize(forward);
    bool orthographic = (push.params.w & PT_PARAMS_ORTHOGRAPHIC) != 0u;
    origin = orthographic ? near_pt : push.camera_pos.xyz;
}

// ---- Trace a ray and return hit data ----
bool trace_primary_ray(float3 origin, float3 dir, float ray_max_distance,
    out float3 hit_pos, out float3 hit_normal, out SurfaceMaterial surf,
    out PtInstanceMeta meta, out float2 hit_uv, out float3 view_dir)
{
    RayDesc ray;
    ray.Origin = origin;
    ray.Direction = dir;
    ray.TMin = 0.01;
    ray.TMax = ray_max_distance;

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
            if (mat_albedo_alpha(c_mat, cuv) >= 0.5)
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
    hit_pos = origin + dir * q.CommittedRayT();

    float3 e1 = v1 - v0;
    float3 e2 = v2 - v0;
    float3 geo_normal = cross(e1, e2);
    if (dot(geo_normal, geo_normal) < 1e-12) return false;
    geo_normal *= rsqrt(dot(geo_normal, geo_normal));

    hit_normal = normalize(w0 * n0 + bary.x * n1 + bary.y * n2);
    if (dot(hit_normal, dir) > 0.0)
        hit_normal = -hit_normal;

    view_dir = -dir;

    meta = instance_meta[instance_id];
    GpuMaterial mat = materials[meta.material_slot];
    hit_uv = w0 * uv0 + bary.x * uv1 + bary.y * uv2;
    surf = resolve_material(mat, hit_uv);
    return true;
}

// ---- Compute entry point ----
[shader("compute")]
[numthreads(16, 16, 1)]
void ptMain(uint3 tid : SV_DispatchThreadID)
{
    uint w = push.params.x;
    uint h = push.params.y;
    if (tid.x >= w || tid.y >= h)
        return;

    uint2 pixel = uint2(tid.x, tid.y);

    // Unpack the frame counter and the per-frame flags from params.w
    // (bit 30 = orthographic camera, bit 31 = reset accumulation).
    uint frame_count = push.params.w & PT_PARAMS_FRAME_MASK;
    uint reset = (push.params.w >> 31u) & 1u;

    // ---- Reset accumulation on demand ----
    if (reset != 0u)
    {
        accumImage[pixel] = float4(0.0, 0.0, 0.0, 0.0);
        sampleCount[pixel] = 0u;
    }

    // ---- Freeze when max iterations reached ----
    uint sample_n = sampleCount[pixel];
    if (push.max_iterations > 0u && sample_n >= push.max_iterations)
    {
        float3 resolved = sample_n > 0u ? accumImage[pixel].rgb / float(sample_n) : float3(0.0);
        resolved *= push.camera_pos.w;
        outputImage[pixel] = float4(resolved, 1.0);
        return;
    }

    // ---- RNG ----
    uint seed = tid.x * 73856093u + tid.y * 19349663u + frame_count * 2567483615u;
    float jx = rand_float(seed) - 0.5;
    float jy = rand_float(seed) - 0.5;

    float3 origin, dir;
    generate_ray(tid.x, tid.y, jx, jy, origin, dir);

    float3 total_radiance = float3(0.0, 0.0, 0.0);
    float3 throughput = float3(1.0, 1.0, 1.0);

    float3 sun_radiance = push.light_color.rgb * push.light_dir.w;

    // SHARC state for this path. Declared unconditionally so the bounce loop
    // reads one flag, but only bound to the cache buffers when enabled.
    bool sharc_on = (push.sharc_flags & SHARC_FLAG_QUERY) != 0u;
    // RTXGI's update pass renders at 1/downscale resolution, so only 1/N of
    // the pixels' worth of samples reach the cache per frame. The unified
    // pipeline has one dispatch, so the same density is obtained by letting
    // one pixel in every N-by-N block deposit.
    bool sharc_update = (push.sharc_flags & SHARC_FLAG_UPDATE) != 0u;
    uint sharc_stride = max((push.sharc_flags >> SHARC_FLAG_STRIDE_SHIFT) & SHARC_FLAG_STRIDE_MASK, 1u);
    if (sharc_update && sharc_stride > 1u && (tid.x % sharc_stride) != 0u)
        sharc_update = false;
    if (sharc_update && sharc_stride > 1u && (tid.y % sharc_stride) != 0u)
        sharc_update = false;
    SharcParameters sharc_params = make_sharc_parameters();
    SharcState sharc_state;
    SharcInit(sharc_state);

    // ---- Bounce 0: primary ray + ReSTIR direct light ----
    float3 hit_pos, hit_normal;
    SurfaceMaterial surf;
    PtInstanceMeta meta;
    float2 hit_uv;
    float3 view_dir;

    if (trace_primary_ray(origin, dir, push.ray_max_distance,
        hit_pos, hit_normal, surf, meta, hit_uv, view_dir))
    {
        // ---- Emissive at hit ----
        total_radiance += throughput * surf.emissive;

        // ---- ReSTIR DI ----
        // RIS initial sampling
        ReSTIRReservoir reservoir = restir_ris_init(
            surf, hit_normal, view_dir, hit_pos,
            push.light_dir.xyz, sun_radiance,
            push.num_lights, push.num_emissive,
            RESTIR_M, seed);

        uint linear_idx = pixel.y * push.params.x + pixel.x;

        // Reuse only completed reservoirs from the previous frame. The old
        // groupshared path placed a group barrier inside divergent control
        // flow (sky pixels, edge threads and frozen pixels can return early),
        // which is undefined and can expose partially written reservoirs.
        if (reset == 0u && frame_count > 0u)
        {
            ReSTIRReservoir prev = prevReservoir[linear_idx];
            reservoir = restir_combine_at_point(
                reservoir, prev, surf, hit_normal, view_dir, hit_pos,
                push.light_dir.xyz, sun_radiance, push.num_lights,
                RESTIR_TEMPORAL_M_CAP, seed);

            // Spatial reuse from random previous-frame neighbors. Each source
            // is re-evaluated against the current material, normal and point.
            [unroll]
            for (uint si = 0u; si < RESTIR_SPATIAL_SAMPLES; si++)
            {
                int2 offset = int2(
                    int(rand_float(seed) * 17.0) - 8,
                    int(rand_float(seed) * 17.0) - 8);
                int2 nbr_pixel = clamp(int2(pixel) + offset,
                    int2(0, 0), int2(int(w) - 1, int(h) - 1));
                uint nbr_idx = uint(nbr_pixel.y) * w + uint(nbr_pixel.x);
                ReSTIRReservoir nbr_r = prevReservoir[nbr_idx];
                reservoir = restir_combine_at_point(
                    reservoir, nbr_r, surf, hit_normal, view_dir, hit_pos,
                    push.light_dir.xyz, sun_radiance, push.num_lights,
                    RESTIR_SPATIAL_M_CAP, seed);
            }
        }

        // Write reservoir for next frame's temporal reuse
        currReservoir[linear_idx] = reservoir;

        // ---- ReSTIR-selected direct light ----
        float restir_weight = restir_resampling_weight(reservoir);
        if (restir_weight > 0.0)
        {
            float3 L_dir, L_rad;
            float dist2;
            restir_resolve_light(reservoir.light_idx, hit_pos, L_dir, L_rad, dist2,
                push.light_dir.xyz, sun_radiance);

            float shadow = 1.0;
            if (dot(L_rad, L_rad) > 1e-10 && dot(hit_normal, L_dir) > 0.0)
            {
                // Shadow ray
                RayDesc sr;
                sr.Origin = hit_pos + 0.0001 * L_dir;
                sr.Direction = L_dir;
                sr.TMin = 0.001;
                float light_distance = sqrt(max(dist2, 0.0));
                sr.TMax = dist2 >= 1e9 ? push.ray_max_distance
                    : min(push.ray_max_distance,
                        max(light_distance - 0.001, 0.001));
                RayQuery<RAY_FLAG_SKIP_CLOSEST_HIT_SHADER> sq;
                sq.TraceRayInline(tlas, RAY_FLAG_SKIP_CLOSEST_HIT_SHADER, 0xFF, sr);
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
                        if (mat_albedo_alpha(smat, suv) >= 0.5)
                        {
                            sq.CommitNonOpaqueTriangleHit();
                            break;
                        }
                    }
                }
                if (sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
                    shadow = 0.0;

                float3 brdf_re = eval_brdf(surf, hit_normal, view_dir, L_dir);
                total_radiance += throughput * brdf_re * L_rad * shadow
                    * restir_weight;
            }
        }

        // ---- Emissive triangle NEE (bounce 0, independent of ReSTIR) ----
        if (push.num_emissive > 0u)
        {
            uint ei = uint(rand_float(seed) * float(push.num_emissive));
            ei = min(ei, push.num_emissive - 1u);
            PtEmissiveTri et = ptEmissive[ei];
            float r1 = sqrt(rand_float(seed));
            float r2 = rand_float(seed);
            float b1 = 1.0 - r1;
            float b2 = r2 * r1;
            float b0 = 1.0 - b1 - b2;
            float3 et_pt = b0 * et.v0.xyz + b1 * et.v1.xyz + b2 * et.v2.xyz;
            float3 et_n = et.normal.xyz;
            float3 to_l = et_pt - hit_pos;
            float l_dist2 = dot(to_l, to_l);
            float l_dist = sqrt(l_dist2);
            float3 L_dir_et = to_l / max(l_dist, 1e-8);
            float cos_light_et = abs(dot(et_n, -L_dir_et));
            float p_nee_et = (1.0 / max(float(push.num_emissive) * et.area, 1e-8))
                * l_dist2 / max(cos_light_et, 1e-8);

            float et_shadow = 1.0;
            if (dot(hit_normal, L_dir_et) > 0.0 && cos_light_et > 1e-4)
            {
                RayDesc sr;
                sr.Origin = hit_pos + 0.0001 * L_dir_et;
                sr.Direction = L_dir_et;
                sr.TMin = 0.001;
                sr.TMax = max(l_dist - 0.001, 0.001);
                RayQuery<RAY_FLAG_SKIP_CLOSEST_HIT_SHADER> sq;
                sq.TraceRayInline(tlas, RAY_FLAG_SKIP_CLOSEST_HIT_SHADER, 0xFF, sr);
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
                        if (mat_albedo_alpha(smat, suv) >= 0.5)
                        {
                            sq.CommitNonOpaqueTriangleHit();
                            break;
                        }
                    }
                }
                if (sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
                    et_shadow = 0.0;
            }
            float3 et_brdf = eval_brdf(surf, hit_normal, view_dir, L_dir_et);
            float p_brdf_et = eval_brdf_pdf(surf, hit_normal, view_dir, L_dir_et);
            total_radiance += throughput * et_brdf * et.radiance.rgb * et_shadow
                * p_nee_et / max(p_nee_et * p_nee_et + p_brdf_et * p_brdf_et, 1e-20);
        }

        // ---- Russian roulette + sample next direction (bounce 0 → bounce 1) ----
        float p_rr = max(max(throughput.r, throughput.g), throughput.b);
        if (p_rr >= 1e-4)
        {
            float surv = min(p_rr, 1.0);
            if (rand_float(seed) < surv)
            {
                throughput /= surv;

                // Sample next direction
                if (surf.transmission > 0.0 && surf.metallic < 0.5)
                {
                    float cos_theta = max(dot(hit_normal, view_dir), 0.0);
                    float F = fresnel_dielectric(cos_theta, surf.ior);
                    if (rand_float(seed) < F)
                    {
                        float3 refl_dir = reflect(-view_dir, hit_normal);
                        dir = refl_dir;
                        origin = hit_pos + 0.0001 * dir;
                    }
                    else
                    {
                        float eta = 1.0 / surf.ior;
                        float3 refr_dir = refract(dir, hit_normal, eta);
                        if (dot(refr_dir, refr_dir) < 0.001)
                        {
                            dir = reflect(-view_dir, hit_normal);
                            origin = hit_pos + 0.0001 * dir;
                        }
                        else
                        {
                            dir = normalize(refr_dir);
                            origin = hit_pos - 0.0001 * dir;
                            throughput *= surf.trans_color / max(1.0 - F, 1e-7);
                        }
                    }
                }
                else
                {
                    float prob_diffuse = surf.roughness;
                    float3 new_dir;
                    if (rand_float(seed) < prob_diffuse)
                    {
                        float u1 = rand_float(seed);
                        float u2 = rand_float(seed);
                        new_dir = cosine_hemisphere_sample(hit_normal, u1, u2);
                    }
                    else
                    {
                        float real_roughness = surf.roughness * surf.roughness;
                        float3 h = ggx_sample(seed, real_roughness, hit_normal);
                        new_dir = reflect(-view_dir, h);
                        if (dot(hit_normal, new_dir) <= 0.0) { p_rr = 0; } // kill path
                    }
                    if (dot(hit_normal, new_dir) > 0.0)
                    {
                        float brdf_pdf = eval_brdf_pdf(surf, hit_normal, view_dir, new_dir);
                        if (brdf_pdf > 1e-7)
                        {
                            float3 brdf_cos = eval_brdf(surf, hit_normal, view_dir, new_dir);
                            throughput *= brdf_cos / brdf_pdf;
                            dir = new_dir;
                            origin = hit_pos + 0.0001 * new_dir;
                        }
                        else { p_rr = 0; }
                    }
                    else { p_rr = 0; }
                }
            }
            else { p_rr = 0; }
        }
        else { p_rr = 0; }

        // The query rejects cache hits whose roughness footprint is smaller
        // than a voxel, so it needs the accumulated roughness of the path so
        // far. The primary hit counts as one diffuse bounce.
        // Roughness accumulated over the path so far; the cache lookup needs it
        // to size the cone the remaining bounces would cover. RTXGI clamps the
        // per-hit roughness to the threshold before it feeds this accumulator,
        // which is what keeps very rough paths from querying voxels far smaller
        // than their footprint.
        float sharc_roughness_prev = 0.0;
        if (sharc_on)
            sharc_roughness_prev += surf.metallic < 0.5
                ? 1.0
                : max(push.sharc_roughness_threshold, surf.roughness);

        // ---- Bounces 1+ (indirect, no NEE) ----
        for (uint bounce = 1u; bounce < push.params.z; bounce++)
        {
            if (p_rr < 1e-4) break;

            // Trace ray
            RayDesc ray;
            ray.Origin = origin;
            ray.Direction = dir;
            ray.TMin = 0.01;
            ray.TMax = push.ray_max_distance;

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
                    if (mat_albedo_alpha(c_mat, cuv) >= 0.5)
                        q.CommitNonOpaqueTriangleHit();
                }
            }

            if (q.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
            {
                uint iid = q.CommittedInstanceID();
                uint pid = q.CommittedPrimitiveIndex();
                float3 v0, v1, v2;
                float2 uv0, uv1, uv2;
                float3 n0, n1, n2;
                read_hit_triangle(iid, pid, vertexData, indices,
                    v0, v1, v2, uv0, uv1, uv2, n0, n1, n2);

                float2 b = q.CommittedTriangleBarycentrics();
                float wb = 1.0 - b.x - b.y;

                float3 e1 = v1 - v0;
                float3 e2 = v2 - v0;
                float3 gn = cross(e1, e2);
                if (dot(gn, gn) < 1e-12) break;
                gn *= rsqrt(dot(gn, gn));

                float3 hn = normalize(wb * n0 + b.x * n1 + b.y * n2);
                if (dot(hn, dir) > 0.0) hn = -hn;

                float3 vw_dir = -dir;
                float3 hp = origin + dir * q.CommittedRayT();
                float2 huv = wb * uv0 + b.x * uv1 + b.y * uv2;

                PtInstanceMeta m = instance_meta[iid];
                GpuMaterial mat = materials[m.material_slot];
                SurfaceMaterial hit_surf = resolve_material(mat, huv);

                // Emissive
                total_radiance += throughput * hit_surf.emissive;

                float3 seg_direct = float3(0.0, 0.0, 0.0);

                // ---- Bounce NEE: sample directional sun at indirect hit ----
                // Without NEE in bounce 1+, shadow regions get indirect light
                // only when the BRDF-sampled direction accidentally hits a
                // sun-lit surface — high variance produces colored noise.
                {
                    float bn_shadow = 1.0;
                    float3 bn_dir = push.light_dir.xyz;
                    if (dot(hn, bn_dir) > 0.0)
                    {
                        RayDesc sr;
                        sr.Origin = hp + 0.0001 * bn_dir;
                        sr.Direction = bn_dir;
                        sr.TMin = 0.001;
                        sr.TMax = push.ray_max_distance;
                        RayQuery<RAY_FLAG_SKIP_CLOSEST_HIT_SHADER> sq;
                        sq.TraceRayInline(tlas, RAY_FLAG_SKIP_CLOSEST_HIT_SHADER, 0xFF, sr);
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
                                if (mat_albedo_alpha(smat, suv) >= 0.5)
                                {
                                    sq.CommitNonOpaqueTriangleHit();
                                    break;
                                }
                            }
                        }
                        if (sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
                            bn_shadow = 0.0;
                    }
                    float3 bn_brdf = eval_brdf(hit_surf, hn, vw_dir, bn_dir);
                    float p_brdf_bn = eval_brdf_pdf(hit_surf, hn, vw_dir, bn_dir);
                    float p_nee_bn = 1.0; // sun is deterministic
                    // `seg_direct` is the same term without throughput: SHARC
                    // propagates radiometric radiance and carries throughput in
                    // its own sample weights.
                    float3 nee_direct = bn_brdf * sun_radiance * bn_shadow
                        * p_nee_bn / max(p_nee_bn * p_nee_bn + p_brdf_bn * p_brdf_bn, 1e-20);
                    total_radiance += throughput * nee_direct;
                    seg_direct += nee_direct;
                }

                // ---- SHARC: query the cache, then feed this segment back ----
                if (sharc_on)
                {
                    SharcHitData sharc_hit;
                    sharc_hit.positionWorld = hp;
                    float3 sharc_normal = gn;
                    if (dot(sharc_normal, dir) > 0.0) sharc_normal = -sharc_normal;
                    sharc_hit.normalWorld = sharc_normal;

                    // Only trust the cache once a path segment is longer than
                    // the voxel it lands in and its roughness footprint covers
                    // that voxel; otherwise the lookup is aliased.
                    float hit_distance = q.CommittedRayT();
                    float voxel_size = HashGridGetVoxelSize(
                        HashGridGetLevel(hp, sharc_params.gridParameters),
                        sharc_params.gridParameters);
                    float roughness_prev = min(sharc_roughness_prev, 0.99);
                    float alpha = roughness_prev * roughness_prev;
                    float footprint = hit_distance
                        * sqrt(0.5 * alpha * alpha / max(1.0 - alpha * alpha, 1e-6));
                    bool valid_hit = reset == 0u && frame_count > 0u
                        && hit_distance > voxel_size * 1.7320508
                        && footprint > voxel_size;

                    if (valid_hit)
                    {
                        float3 cached_radiance;
                        if (SharcGetCachedRadiance(sharc_params, sharc_hit, cached_radiance, false))
                        {
                            // The cache answers the remaining bounces; keep the
                            // path radiometrically consistent by adding it with
                            // the current throughput.
                            total_radiance += throughput * cached_radiance;
                            break;
                        }
                    }

                    // Not in the cache yet: deposit this segment's direct
                    // lighting so a later frame can terminate here.
                    if (sharc_update)
                    {
                        SharcUpdateHit(sharc_params, sharc_state, sharc_hit, seg_direct, rand_float(seed));
                        sharc_roughness_prev += hit_surf.metallic < 0.5
                            ? 1.0
                            : max(push.sharc_roughness_threshold, hit_surf.roughness);
                    }
                }

                // Russian roulette
                float p = max(max(throughput.r, throughput.g), throughput.b);
                if (bounce + 1u >= push.params.z) break;
                if (p < 1e-4) break;

                float sv = min(p, 1.0);
                float rr = rand_float(seed);
                if (rr >= sv) break;
                throughput /= sv;

                // Next direction
                if (hit_surf.transmission > 0.0 && hit_surf.metallic < 0.5)
                {
                    float ct = max(dot(hn, vw_dir), 0.0);
                    float F = fresnel_dielectric(ct, hit_surf.ior);
                    if (rand_float(seed) < F)
                    {
                        dir = reflect(-vw_dir, hn);
                        origin = hp + 0.0001 * dir;
                    }
                    else
                    {
                        float eta = 1.0 / hit_surf.ior;
                        float3 rd = refract(dir, hn, eta);
                        if (dot(rd, rd) < 0.001)
                        {
                            dir = reflect(-vw_dir, hn);
                            origin = hp + 0.0001 * dir;
                        }
                        else
                        {
                            dir = normalize(rd);
                            origin = hp - 0.0001 * dir;
                            throughput *= hit_surf.trans_color / max(1.0 - F, 1e-7);
                        }
                    }
                }
                else
                {
                    float pd = hit_surf.roughness;
                    float3 nd;
                    if (rand_float(seed) < pd)
                    {
                        float u1 = rand_float(seed);
                        float u2 = rand_float(seed);
                        nd = cosine_hemisphere_sample(hn, u1, u2);
                    }
                    else
                    {
                        float rr2 = hit_surf.roughness * hit_surf.roughness;
                        float3 h = ggx_sample(seed, rr2, hn);
                        nd = reflect(-vw_dir, h);
                        if (dot(hn, nd) <= 0.0) { p_rr = 0; break; }
                    }
                    if (dot(hn, nd) <= 0.0) { break; }
                    float bpdf = eval_brdf_pdf(hit_surf, hn, vw_dir, nd);
                    if (bpdf <= 1e-7) break;
                    throughput *= eval_brdf(hit_surf, hn, vw_dir, nd) / bpdf;
                    dir = nd;
                    origin = hp + 0.0001 * nd;
                }
            }
            else
            {
                // Miss: env map. The sky is the far-field terminator that lets
                // the cache propagate radiance back along the whole path.
                float3 sky_radiance = envCube.SampleLevel(dir, 0.0).rgb;
                if (sharc_on && sharc_update)
                    SharcUpdateMiss(sharc_params, sharc_state, sky_radiance);
                total_radiance += throughput * sky_radiance;
                break;
            }
        }
    }
    else
    {
        // ---- Primary ray missed: HDR sky ----
        total_radiance += throughput * envCube.SampleLevel(dir, 0.0).rgb;

        // Do not leave a stale geometry reservoir behind a sky pixel. Such a
        // reservoir can otherwise leak across moving silhouettes via reuse.
        ReSTIRReservoir empty_reservoir;
        empty_reservoir.light_idx = 0u;
        empty_reservoir.M = 0.0;
        empty_reservoir.W = 0.0;
        empty_reservoir.target_pdf = 0.0;
        uint linear_idx = pixel.y * push.params.x + pixel.x;
        currReservoir[linear_idx] = empty_reservoir;
    }

    // Reject invalid transport values before they poison the persistent
    // accumulation buffer. Negative RGB is non-physical for this integrator.
    bool finite_radiance = all(total_radiance == total_radiance)
        && all(abs(total_radiance) < float3(1e30));
    if (!finite_radiance)
        total_radiance = float3(0.0);
    total_radiance = max(total_radiance, float3(0.0));

    // ---- Firefly clamp: prevent individual bright samples from creating
    // persistent sparkles in the accumulation. The bound 50.0 corresponds to
    // ~140× a typical diffuse-sun bounce (0.36), so it only clips outliers.
    float ff_luma = dot(total_radiance, float3(0.2126, 0.7152, 0.0722));
    if (ff_luma > 50.0)
        total_radiance *= 50.0 / ff_luma;

    // ---- Accumulate ----
    float3 prev = accumImage[pixel].rgb;
    uint   count = sampleCount[pixel];

    float3 new_accum = prev + total_radiance;
    uint   new_count = count + 1u;

    accumImage[pixel] = float4(new_accum, 0.0);
    sampleCount[pixel] = new_count;

    float3 result = new_count > 0u ? new_accum / float(new_count) : float3(0.0);
    result *= push.camera_pos.w;
    outputImage[pixel] = float4(result, 1.0);
}
