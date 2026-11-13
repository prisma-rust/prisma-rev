// Path integrator - shared multi-bounce path tracing core.
//
// Slang module imported by GI bakers, offline renderers, and the real-time PT
// pass. It only computes the radiance arriving at a point from a given
// direction by tracing paths through the scene. Consumers decide what to do
// with that radiance (SH projection, pixel color, ...).
//
// Geometry + materials are module-global bindings (so `TraceRadiance`'s
// signature stays small): one combined vertex/index buffer, one TLAS, a
// per-instance metadata SSBO (`PtInstanceMeta[]`), and the shared
// `GpuMaterial[]` SSBO + bindless texture table.
//
// BRDF model: aligned with truvis (`lib/realtime_rt/raygen_material.slangi` +
// `lib/lighting/pbr.slangi`). A metal-roughness surface is sampled as a mix of
// cosine-weighted Lambert diffuse and GGX specular, weighted by `roughness`.
// Metallic (1.0) kills the diffuse term and routes all energy through the
// specular lobe (F0 = base_color); non-metals keep a Lambert diffuse plus a
// dielectric specular highlight (F0 = 0.16 * 0.8^2). This is what stops
// metallic-tagged geometry from rendering as flat "black plastic" - the
// previous pure-Lambert integrator had no specular lobe, so metallic surfaces
// (whose diffuse term is zero) had nowhere to put their energy.
#pragma once

static const float PI = 3.14159265359;

// ---- Vertex layout constants (shared by all consumers) ----
// Matches prism-render `mesh::Vertex`, repr(C), 60 bytes.
public static const uint VERTEX_STRIDE   = 60u;
public static const uint POSITION_OFFSET = 0u;
public static const uint NORMAL_OFFSET   = 12u;
public static const uint UV_OFFSET       = 36u;

// ---- Descriptor bindings (consumers must declare the same sets) ----
// Set 0: per-consumer local bindings. b0..b5 are consumer-specific (accum/
// output images, TLAS, vertex/index buffers). b6 + b7 are shared here:
[[vk::binding(6, 0)]]
StructuredBuffer<PtInstanceMeta> instance_meta; // per-instance material slot + bases

[[vk::binding(7, 0)]]
StructuredBuffer<GpuMaterial> materials; // shared material SSBO

// Set 1: bindless texture table (shared with the rasterizer).
[[vk::binding(0, 1)]]
SamplerState globalSamplers[];
[[vk::binding(1, 1)]]
Texture2D bindlessSrvs[];

// b8: Analytic lights for multi-light NEE. Mirrors descriptor.rs PtAnalyticLight.
// Zeroed entries are safe to skip (color_params ~= 0 => no contribution).
static const uint PT_LIGHT_MAX = 64u;

struct PtAnalyticLight
{
    float4 position_kind;    // .xyz = world position; .w = kind (PtLightKind as float)
    float4 direction_params; // .xyz = direction (for spot/directional); .w = inner_angle cos
    float4 color_params;     // .rgb = radiance/color; .w = outer_angle cos / range / area
};

enum PtLightKind
{
    Directional = 0,
    Point       = 1,
    Spot        = 2,
    Area        = 3,
};

[[vk::binding(8, 0)]]
StructuredBuffer<PtAnalyticLight> ptLights;

// b9: Emissive triangle SSBO. Mirrors descriptor.rs PtEmissiveTri.
// Each entry is a world-space triangle with pre-computed radiance, used for
// explicit NEE from emissive surface geometry.
static const uint PT_EMISSIVE_MAX = 1024u;

struct PtEmissiveTri
{
    float4 v0;       // .xyz = vertex 0
    float4 v1;       // .xyz = vertex 1
    float4 v2;       // .xyz = vertex 2
    float4 normal;   // .xyz = shading normal (face normal)
    float4 radiance; // .rgb = pre-scaled emissive radiance
    float  area;     // precomputed triangle area (for PDF)
};

[[vk::binding(9, 0)]]
StructuredBuffer<PtEmissiveTri> ptEmissive;

// Set 2: IBL environment cubemap (HDR sky sampled on ray miss).
[[vk::binding(0, 2)]]
SamplerCube envCube;

static const uint SAMPLER_LINEAR_WRAP = 0u;

// ---- Pseudo-random number generation (Wang hash) ----
// Deterministic hash-based RNG seeded by the caller. Each call advances the
// seed and returns a uniform uint in [0, 2^32).
public uint wang_hash(inout uint seed)
{
    seed = (seed ^ 61u) ^ (seed >> 16u);
    seed *= 9u;
    seed = seed ^ (seed >> 4u);
    seed *= 0x27d4eb2du;
    seed = seed ^ (seed >> 15u);
    return seed;
}

// Float in [0, 1) from the current seed.
public float rand_float(inout uint seed)
{
    return float(wang_hash(seed)) / 4294967296.0;
}

// ---- Cosine-weighted hemisphere sampling ----
// Given a unit normal `n` and two uniform random numbers `u1, u2` in [0,1),
// returns a unit direction in the hemisphere oriented around `n`, distributed
// with pdf = cos(theta)/pi (Lambertian importance sampling).
public float3 cosine_hemisphere_sample(float3 n, float u1, float u2)
{
    float theta = acos(sqrt(max(1e-8, 1.0 - u1)));
    float phi = 6.283185307 * u2; // 2 * PI

    float sin_t, cos_t;
    sincos(theta, sin_t, cos_t);
    float sin_p, cos_p;
    sincos(phi, sin_p, cos_p);
    float3 local_dir = float3(sin_t * cos_p, sin_t * sin_p, cos_t);

    // Build orthonormal basis from the normal.
    float3 T = abs(n.y) < 0.999
        ? normalize(cross(n, float3(0.0, 1.0, 0.0)))
        : normalize(cross(n, float3(1.0, 0.0, 0.0)));
    float3 B = cross(n, T);

    return local_dir.x * T + local_dir.y * B + local_dir.z * n;
}

// Build an orthonormal tangent/bitangent basis from a unit normal `n`.
// Mirrors truvis `Sample::create_local_coord`.
void create_local_coord(float3 n, out float3 tangent, out float3 bitangent)
{
    float3 T = abs(n.y) < 0.999
        ? normalize(cross(n, float3(0.0, 1.0, 0.0)))
        : normalize(cross(n, float3(1.0, 0.0, 0.0)));
    bitangent = cross(n, T);
    tangent = T;
}

// ---- GGX specular utilities (mirror truvis lib/lighting/pbr.slangi) ----
// `roughness` here is the *real* roughness (alpha = art_roughness^2); callers
// square the material's art roughness before passing it in, matching truvis.

// Trowbridge-Reitz GGX NDF. `roughness` = real roughness (alpha).
float ndf_ggx(float n_dot_h, float roughness)
{
    float a = n_dot_h * roughness;
    float k = roughness / max(1.0 - n_dot_h * n_dot_h + a * a, 1e-7);
    return k * k * (1.0 / PI);
}

// Schlick Fresnel (RGB), `f0` = reflectance at normal incidence.
float3 fresnel_schlick(float v_dot_h, float3 f0)
{
    float f = pow(clamp(1.0 - v_dot_h, 0.0, 1.0), 5.0);
    return f + f0 * (1.0 - f);
}

// Scalar Fresnel for dielectrics (Schlick approximation).
// `cos_theta` = dot(normal, view_dir)  (positive, in [0,1]).
// `ior` = index of refraction (e.g. 1.45 for glass).
float fresnel_dielectric(float cos_theta, float ior)
{
    float f0 = (ior - 1.0) / (ior + 1.0);
    f0 = f0 * f0;
    return f0 + (1.0 - f0) * pow(1.0 - cos_theta, 5.0);
}

// Smith correlated visibility term (G / (4 * NdotV * NdotL) folded in).
float visibility_smith_ggx_correlated(float n_dot_v, float n_dot_l, float roughness)
{
    float a2 = roughness * roughness;
    float ggx_v = n_dot_l * sqrt(n_dot_v * n_dot_v * (1.0 - a2) + a2);
    float ggx_l = n_dot_v * sqrt(n_dot_l * n_dot_l * (1.0 - a2) + a2);
    return 0.5 / max(ggx_v + ggx_l, 1e-7);
}

// Importance-sample a GGX NDF, returning a half-vector `h` in world space
// around `n`. `roughness` = real roughness (alpha). PDF of the returned h is
// `D * n_dot_h / (4 * v_dot_h)`; callers convert to a light direction via
// `reflect(-view, h)` and divide throughput by that pdf.
float3 ggx_sample(inout uint seed, float roughness, float3 n)
{
    float3 tangent, bitangent;
    create_local_coord(n, tangent, bitangent);

    float2 r = float2(
        clamp(rand_float(seed), 0.001, 0.999),
        rand_float(seed));
    float a2 = roughness * roughness;
    float denom = (a2 - 1.0) * r.x + 1.0;
    float cos_theta_h = saturate(sqrt(max(0.0, (1.0 - r.x) / max(1e-7, denom))));
    float sin_theta_h = sqrt(max(0.0, 1.0 - cos_theta_h * cos_theta_h));
    float phi_h = r.y * 2.0 * PI;

    return tangent * (sin_theta_h * cos(phi_h))
         + bitangent * (sin_theta_h * sin(phi_h))
         + n * cos_theta_h;
}

// ---- Per-instance metadata (mirrors `PtInstanceMeta` in pt_pass.rs) ----
struct PtInstanceMeta
{
    uint material_slot;  // index into `materials[]`
    uint index_base;     // base index into the combined index buffer
    uint vertex_base;    // base vertex into the combined vertex buffer
    uint _pad;
};

// ---- GpuMaterial (mirrors scene_frag.slang / GpuMaterial, 112 bytes) ----
struct GpuMaterial
{
    float4 base_color;                  // offset 0
    float4 metallic_roughness_emissive; // offset 16: x=metallic, y=roughness, z=emissive, w=emissive_strength
    uint   albedo_idx;                  // offset 32
    uint   normal_idx;                  // offset 36
    uint   metallic_roughness_idx;      // offset 40
    uint   emissive_idx;                // offset 44
    float4 transmission_factor;         // offset 48
    float4 clearcoat;                   // offset 64
    uint   transmission_tex_idx;        // offset 80
    uint   occlusion_idx;               // offset 84
    float  normal_scale;                // offset 88
    float  occlusion_strength;          // offset 92
    float  alpha_cutoff;                // offset 96: MASK threshold
    uint   alpha_mode;                  // offset 100: 0=Opaque, 1=Mask, 2=Blend
    float2 alpha_pad;                   // offset 104: keeps the record 16-byte aligned
};

// ---- Read the three vertices of a hit triangle ----
// `instance_id` is the TLAS instanceCustomIndex (= PtInstanceMeta index);
// `prim_idx` is the primitive index within that instance's BLAS. Returns the
// triangle's three positions, UVs, and shading normals via out-params. Normals
// come from the vertex buffer (offset NORMAL_OFFSET=12) so the integrator can
// use the artist-authored smooth shading normal instead of the flat geometric
// normal from winding - this matches truvis, which interpolates `interp_normal`
// and only falls back to the geometric normal for degenerate triangles.
public void read_hit_triangle(
    uint instance_id, uint prim_idx,
    ByteAddressBuffer vertexData, StructuredBuffer<uint> indices,
    out float3 v0, out float3 v1, out float3 v2,
    out float2 uv0, out float2 uv1, out float2 uv2,
    out float3 n0, out float3 n1, out float3 n2)
{
    PtInstanceMeta meta = instance_meta[instance_id];
    uint i0 = indices[meta.index_base + prim_idx * 3 + 0];
    uint i1 = indices[meta.index_base + prim_idx * 3 + 1];
    uint i2 = indices[meta.index_base + prim_idx * 3 + 2];

    // Vertex addresses are absolute (combined buffer), so no +vertex_base
    // needed on the byte offset - the stored indices already index the
    // combined buffer (remapped at flatten time). But the byte offset uses
    // the vertex's position within the combined buffer directly.
    v0 = asfloat(vertexData.Load3(i0 * VERTEX_STRIDE + POSITION_OFFSET));
    v1 = asfloat(vertexData.Load3(i1 * VERTEX_STRIDE + POSITION_OFFSET));
    v2 = asfloat(vertexData.Load3(i2 * VERTEX_STRIDE + POSITION_OFFSET));
    uv0 = asfloat(vertexData.Load2(i0 * VERTEX_STRIDE + UV_OFFSET));
    uv1 = asfloat(vertexData.Load2(i1 * VERTEX_STRIDE + UV_OFFSET));
    uv2 = asfloat(vertexData.Load2(i2 * VERTEX_STRIDE + UV_OFFSET));
    n0 = asfloat(vertexData.Load3(i0 * VERTEX_STRIDE + NORMAL_OFFSET));
    n1 = asfloat(vertexData.Load3(i1 * VERTEX_STRIDE + NORMAL_OFFSET));
    n2 = asfloat(vertexData.Load3(i2 * VERTEX_STRIDE + NORMAL_OFFSET));
}

// ---- Resolved surface material at a hit point ----
// Holds the PBR parameters the integrator actually uses. Albedo comes from the
// albedo texture (if bound) or the scalar base_color factor; metallic/roughness
// come from the MR texture (glTF ORM: B=metallic, G=roughness) or the scalar
// factor. `roughness` is the art-space value; callers square it to get the GGX
// alpha when sampling/evaluating the specular lobe (matches truvis).
struct SurfaceMaterial
{
    float3 base_color;
    float  alpha;       // base color alpha (cutout test: alpha < 0.5 = transparent)
    float  metallic;
    float  roughness;
    float3 emissive;    // emissive radiance (already scaled by strength)
    float  transmission; // 0 = opaque, 1 = fully transmissive (glass)
    float  ior;          // index of refraction (e.g. 1.45 for glass)
    float3 trans_color;  // transmission color (Beer's law absorption tint)
};

// Resolve the full PBR material at a hit. Mirrors truvis `MaterialAccess` +
// `closest_hit`: albedo texture is NOT multiplied by base_color (glTF assets
// with a missing baseColorFactor default it to white, so the multiply would be
// a no-op anyway, and truvis keeps the two paths separate). MR texture is
// sampled per glTF (B=metallic, G=roughness) when bound; otherwise the scalar
// Resolve just the albedo alpha at a UV. Used by the inline-ray-query cutout
// test in the proceed loop (where we don't need the full material yet, just
// enough to decide whether a candidate triangle is transparent). Mirrors the
// alpha path of `resolve_material`: texture alpha * scalar base_color.a.
float mat_albedo_alpha(GpuMaterial mat, float2 uv)
{
    float a = mat.base_color.a;
    if (mat.albedo_idx != 0xFFFFFFFFu)
    {
        float4 sampled = bindlessSrvs[NonUniformResourceIndex(mat.albedo_idx)]
            .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], uv, 0.0);
        a = sampled.a * mat.base_color.a;
    }
    return a;
}

// Visibility test for a traversal candidate. Opaque materials never depend on
// alpha (and normally arrive through `CommittedStatus()`, not as candidates);
// MASK materials compare against their own cutoff instead of a fixed 0.5.
bool mat_alpha_pass(GpuMaterial mat, float2 uv)
{
    if (mat.alpha_mode == 0u)
        return true;
    return mat_albedo_alpha(mat, uv) >= mat.alpha_cutoff;
}

// Resolve the full PBR material at a hit. Mirrors truvis `MaterialAccess` +
// `closest_hit`: albedo texture is NOT multiplied by base_color (glTF assets
// with a missing baseColorFactor default it to white, so the multiply would be
// a no-op anyway, and truvis keeps the two paths separate). MR texture is
// sampled per glTF (B=metallic, G=roughness) when bound; otherwise the scalar
// factor is used. Uses SampleLevel (mip 0) because RayQuery has no derivatives.
SurfaceMaterial resolve_material(GpuMaterial mat, float2 uv)
{
    SurfaceMaterial s;

    // Base color + alpha: texture (sRGB->linear on _SRGB done by hardware) or
    // scalar factor. No rgb multiply - matches truvis MaterialAccess. Alpha is
    // reused for the cutout test (resolved separately in the proceed loop, but
    // kept here so the hit branch can also consult it if needed).
    if (mat.albedo_idx != 0xFFFFFFFFu)
    {
        float4 sampled = bindlessSrvs[NonUniformResourceIndex(mat.albedo_idx)]
            .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], uv, 0.0);
        s.base_color = sampled.rgb;
        s.alpha = sampled.a * mat.base_color.a;
    }
    else
    {
        s.base_color = mat.base_color.rgb;
        s.alpha = mat.base_color.a;
    }

    // Metallic / roughness: glTF ORM texture (B=metallic, G=roughness) scaled
    // by the scalar factor, or the scalar factor alone. This is the fix for
    // Sponza-style assets whose scalar metallicFactor/roughnessFactor are
    // missing (glTF default 1.0) but whose real values live in the MR texture
    // - without this the integrator treated every surface as fully metallic
    // and (having no specular lobe) rendered metals as black plastic.
    float metallic_factor  = mat.metallic_roughness_emissive.x;
    float roughness_factor = mat.metallic_roughness_emissive.y;
    if (mat.metallic_roughness_idx != 0xFFFFFFFFu)
    {
        float3 mr = bindlessSrvs[NonUniformResourceIndex(mat.metallic_roughness_idx)]
            .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], uv, 0.0).rgb;
        s.metallic  = mr.b * metallic_factor;
        s.roughness = mr.g * roughness_factor;
    }
    else
    {
        s.metallic  = metallic_factor;
        s.roughness = roughness_factor;
    }
    s.metallic  = saturate(s.metallic);
    s.roughness = clamp(s.roughness, 0.04, 1.0);

    // Emissive: scalar intensity*strength, optionally modulated by an emissive
    // texture. The rasterizer applies this as `emissive_intensity * strength`
    // (with the texture replacing the scalar rgb when bound); we mirror that.
    float ei = mat.metallic_roughness_emissive.z;
    float es = mat.metallic_roughness_emissive.w;
    s.emissive = ei * es;
    if (mat.emissive_idx != 0xFFFFFFFFu)
    {
        float3 e = bindlessSrvs[NonUniformResourceIndex(mat.emissive_idx)]
            .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], uv, 0.0).rgb;
        s.emissive = e * ei * es;
    }

    // Transmission / IOR (glTF transmission extension).
    // transmission_factor.x = transmission (0-1), .y = ior, .z = translucency, .w = anisotropy.
    s.transmission = saturate(mat.transmission_factor.x);
    s.ior = max(mat.transmission_factor.y, 1.0);
    // Transmission color: base_color rgb is the tint at zero thickness
    // (glTF KHR_materials_transmission convention).
    s.trans_color = s.base_color;
    // Optionally sample a dedicated transmission texture (if present).
    if (mat.transmission_tex_idx != 0xFFFFFFFFu && s.transmission > 0.0)
    {
        float3 t = bindlessSrvs[NonUniformResourceIndex(mat.transmission_tex_idx)]
            .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], uv, 0.0).rgb;
        s.trans_color *= t;
    }

    return s;
}

// ---- BRDF evaluation (mirrors truvis RtMaterialSampler::eval_brdf) ----
// Returns (diffuse + specular) * NdotL for the given surface, view, and light
// directions. `view_dir` points from the surface to the camera; `light_dir`
// points from the surface to the light. Does NOT include throughput or light
// pdf - callers compose those.
//
// Diffuse: (1 - metallic) * base_color / PI  (metals have no diffuse).
// Specular: GGX (D*G*F) with F0 = lerp(0.16*0.8^2, base_color, metallic).
//   - 0.16*0.8^2 ~= 0.1024 is truvis's dielectric F0 constant (0.8 reflectance).
float3 eval_brdf(SurfaceMaterial s, float3 n, float3 view_dir, float3 light_dir)
{
    float n_dot_l = max(dot(n, light_dir), 0.0);
    if (n_dot_l <= 0.0)
        return float3(0.0);

    // Diffuse lobe (zero for metals).
    float3 diffuse_color = (1.0 - s.metallic) * s.base_color;
    float3 diffuse = diffuse_color / PI;

    // Specular lobe (GGX). real_roughness = art^2 (truvis convention).
    float real_roughness = s.roughness * s.roughness;
    float3 f0 = lerp(float3(0.16 * 0.8 * 0.8), s.base_color, s.metallic);
    float3 h = normalize(view_dir + light_dir);
    float n_dot_v = max(dot(n, view_dir), 0.0);
    float n_dot_h = max(dot(n, h), 0.0);
    float v_dot_h = max(dot(view_dir, h), 0.0);
    float D = ndf_ggx(n_dot_h, real_roughness);
    float3 F = fresnel_schlick(v_dot_h, f0);
    float V = visibility_smith_ggx_correlated(n_dot_v, n_dot_l, real_roughness);
    float3 specular = F * D * V;

    return (diffuse + specular) * n_dot_l;
}

// BRDF PDF for a sampled direction, used for the bounce throughput = brdf_cos /
// pdf division. Mirrors truvis RtMaterialSampler::eval_brdf_pdf: a mixture of
// the diffuse (cosine hemisphere) and specular (GGX NDF) pdfs, weighted by
// `prob_diffuse = roughness` (smoother surfaces favor the specular lobe).
float eval_brdf_pdf(SurfaceMaterial s, float3 n, float3 view_dir, float3 light_dir)
{
    float prob_diffuse = s.roughness;
    // Diffuse pdf: cos(theta) / PI.
    float diffuse_pdf = max(dot(n, light_dir), 0.0) / PI;
    // Specular pdf: D * n_dot_h / (4 * v_dot_h).
    float3 h = normalize(view_dir + light_dir);
    float real_roughness = s.roughness * s.roughness;
    float n_dot_h = max(dot(n, h), 0.0);
    float v_dot_h = max(dot(view_dir, h), 0.0);
    float D = ndf_ggx(n_dot_h, real_roughness);
    float specular_pdf = D * n_dot_h / max(4.0 * v_dot_h, 1e-7);
    return prob_diffuse * diffuse_pdf + (1.0 - prob_diffuse) * specular_pdf;
}

// ---- MIS helpers ----
// NEE pdf for a directional (push-constant) sun light. Always sampled when
// the light exists (deterministic direction), so pdf_sun = 1.0.
float nee_pdf_sun() { return 1.0; }

// Power-heuristic MIS weight with beta=2.
// `p_a` = pdf of the evaluating strategy (NEE).
// `p_b` = pdf of the alternative strategy (BRDF sampling).
float mis_weight(float p_a, float p_b)
{
    float pa2 = p_a * p_a;
    float pb2 = p_b * p_b;
    return pa2 / max(pa2 + pb2, 1e-20);
}

// ---- ReSTIR DI structures and helpers ----
// Per-pixel reservoir for temporal+spatial direct-light resampling.
// light_idx 0 = sun, 1..PT_LIGHT_MAX = analytic lights.
// For the current implementation, emissive triangles are not included in
// ReSTIR; they still use the standard NEE loop.
struct ReSTIRReservoir
{
    uint  light_idx;
    float M;          // Number of proposals represented by this reservoir.
    float W;          // Sum of unnormalised RIS weights.
    float target_pdf; // Unnormalised target at the reservoir's shading point.
};

// ---- ReSTIR: resolve light direction + radiance + distance from index ---
// Index 0 = sun (from push constants). Index 1.. = ptLights[li-1].
void restir_resolve_light(
    uint light_idx, float3 hit_pos,
    out float3 L_dir, out float3 L_radiance, out float L_dist2,
    float3 sun_dir, float3 sun_radiance)
{
    if (light_idx == 0u)
    {
        // Sun from push constants
        L_dir = sun_dir;
        L_radiance = sun_radiance;
        L_dist2 = 1e10; // effectively infinite
    }
    else
    {
        uint li = light_idx - 1u;
        if (li >= PT_LIGHT_MAX) { L_dir = float3(0,0,1); L_radiance = 0; L_dist2 = 1e10; return; }
        PtAnalyticLight al = ptLights[li];
        L_radiance = al.color_params.rgb;
        uint kind = uint(al.position_kind.w);
        if (kind == uint(PtLightKind::Directional))
        {
            L_dir = -normalize(al.direction_params.xyz);
            L_dist2 = 1e10;
        }
        else
        {
            L_dir = al.position_kind.xyz - hit_pos;
            L_dist2 = dot(L_dir, L_dir);
            float dist = sqrt(L_dist2);
            L_dir = L_dir / max(dist, 1e-8);
            // Distance falloff for point/spot
            if (kind == uint(PtLightKind::Point) || kind == uint(PtLightKind::Spot))
            {
                float range = al.color_params.w;
                if (range > 0.0 && dist > range) { L_radiance = 0; return; }
                L_radiance /= max(L_dist2, 1e-8);
            }
            else if (kind == uint(PtLightKind::Area))
            {
                float radius = al.direction_params.w;
                float solid_angle = PI * radius * radius / max(L_dist2, 1e-8);
                L_radiance *= clamp(solid_angle, 0.0, 1.0);
            }
        }
    }
}

// Quick light direction only (for visibility checks during spatial reuse).
void restir_resolve_dir(uint light_idx, float3 hit_pos, out float3 L_dir,
    float3 sun_dir)
{
    if (light_idx == 0u) { L_dir = sun_dir; return; }
    uint li = light_idx - 1u;
    if (li >= PT_LIGHT_MAX) { L_dir = float3(0,0,1); return; }
    PtAnalyticLight al = ptLights[li];
    uint kind = uint(al.position_kind.w);
    if (kind == uint(PtLightKind::Directional))
        L_dir = -normalize(al.direction_params.xyz);
    else
    {
        L_dir = al.position_kind.xyz - hit_pos;
        float dist = max(length(L_dir), 1e-8);
        L_dir /= dist;
    }
}

// RIS target function: approximate luminance of BRDF * radiance contribution.
// Used for both the RIS weight and the stored target_pdf of the selected light.
float restir_target_pdf(SurfaceMaterial s, float3 n, float3 view_dir,
    float3 L_dir, float3 L_radiance)
{
    float3 brdf = eval_brdf(s, n, view_dir, L_dir);
    return max(dot(brdf * L_radiance, float3(0.2126, 0.7152, 0.0722)), 0.0);
}

// RIS initial sampling: evaluate M candidate lights, build reservoir.
// M is the RIS candidate count (e.g. 8).
ReSTIRReservoir restir_ris_init(
    SurfaceMaterial s, float3 n, float3 view_dir, float3 hit_pos,
    float3 sun_dir, float3 sun_radiance, uint num_lights, uint num_emissive,
    uint M, inout uint seed)
{
    ReSTIRReservoir r;
    r.light_idx = 0; r.M = 0; r.W = 0; r.target_pdf = 0;
    uint total = num_lights + 1u; // 0 = sun, 1..num_lights = analytic
    float p_init = 1.0 / max(float(total), 1.0);

    for (uint i = 0u; i < M; i++)
    {
        // M counts every proposal, including zero-contribution candidates.
        // Counting only accepted candidates biases the reservoir toward bright
        // lights and makes the final normalisation unstable in shadows.
        r.M += 1.0;

        // Random candidate
        uint li = uint(rand_float(seed) * float(total));
        li = min(li, total - 1u);

        float3 L_dir, L_rad;
        float dist2;
        restir_resolve_light(li, hit_pos, L_dir, L_rad, dist2, sun_dir, sun_radiance);

        float n_dot_l = dot(n, L_dir);
        if (n_dot_l <= 0.0 || dot(L_rad, L_rad) < 1e-10) continue;

        float target = restir_target_pdf(s, n, view_dir, L_dir, L_rad);
        if (!(target > 0.0) || target != target || target >= 1e30) continue;

        // RIS weight update
        float w = target / max(p_init, 1e-38);
        if (!(w > 0.0) || w != w || w >= 1e30) continue;
        r.W += w;
        if (r.W != r.W || r.W >= 1e30)
        {
            r.W = 0.0;
            r.target_pdf = 0.0;
            continue;
        }

        if (rand_float(seed) < w / max(r.W, 1e-38))
        {
            r.light_idx = li;
            r.target_pdf = target;
        }
    }
    return r;
}

// Re-evaluate a temporal/spatial reservoir at the current shading point before
// merging it. A source reservoir's stored target belongs to its old point and
// cannot be copied directly. source_m_cap also prevents unbounded history
// growth while preserving the source reservoir's average weight.
ReSTIRReservoir restir_combine_at_point(
    ReSTIRReservoir current, ReSTIRReservoir source,
    SurfaceMaterial s, float3 n, float3 view_dir, float3 hit_pos,
    float3 sun_dir, float3 sun_radiance, uint num_lights,
    float source_m_cap, inout uint seed)
{
    if (source.light_idx > num_lights ||
        !(source.M > 0.0) || source.M != source.M || source.M >= 1e30 ||
        !(source.W > 0.0) || source.W != source.W || source.W >= 1e30 ||
        !(source.target_pdf > 0.0) || source.target_pdf != source.target_pdf ||
        source.target_pdf >= 1e30)
        return current;

    float3 L_dir, L_rad;
    float dist2;
    restir_resolve_light(source.light_idx, hit_pos, L_dir, L_rad, dist2,
        sun_dir, sun_radiance);
    if (dot(n, L_dir) <= 0.0 || dot(L_rad, L_rad) < 1e-10)
        return current;

    float target_current = restir_target_pdf(s, n, view_dir, L_dir, L_rad);
    if (!(target_current > 0.0) || target_current != target_current ||
        target_current >= 1e30)
        return current;

    float represented_m = min(source.M, source_m_cap);
    float history_scale = represented_m / source.M;
    float candidate_w = target_current * (source.W / source.target_pdf) * history_scale;
    if (!(candidate_w > 0.0) || candidate_w != candidate_w || candidate_w >= 1e30)
        return current;

    float combined_w = current.W + candidate_w;
    if (combined_w != combined_w || combined_w >= 1e30)
        return current;

    current.M += represented_m;
    current.W = combined_w;
    if (rand_float(seed) < candidate_w / max(combined_w, 1e-38))
    {
        current.light_idx = source.light_idx;
        current.target_pdf = target_current;
    }
    return current;
}

// Final RIS normalisation. This is a multiplier on the selected sample's RGB
// contribution, not a directional PDF and therefore must not be added to the
// BRDF PDF.
float restir_resampling_weight(ReSTIRReservoir r)
{
    if (!(r.M > 0.0) || !(r.W > 0.0) || !(r.target_pdf > 0.0))
        return 0.0;
    float weight = r.W / max(r.M * r.target_pdf, 1e-38);
    if (weight != weight || weight >= 1e30)
        return 0.0;
    return max(weight, 0.0);
}

// ---- Core path tracing function ----
// Trace a path starting from `origin` in direction `dir`, accumulating
// direct-sun + emissive + sky contributions through up to `max_bounce`
// inter-reflections with Russian-roulette termination. The surface BRDF is a
// metal-roughness mix of Lambert diffuse and GGX specular (aligned with
// truvis); geometry/materials are module globals.
//
// Parameters:
//   origin           - world-space ray origin (probe position or camera)
//   dir              - unit direction of the ray
//   light_dir        - unit direction TO the directional light (sun)
//   light_color      - radiance of the directional light (pre-scaled by intensity)
//   max_bounce       - maximum path depth (1 = only direct sun at first hit)
//   ray_max_distance - max world-space length of primary + shadow rays
//   seed             - inout RNG seed (per-path, advanced on each random call)
//
// Returns the total radiance arriving at `origin` from direction `dir`.
public float3 TraceRadiance(
    float3 origin,
    float3 dir,
    RaytracingAccelerationStructure tlas,
    ByteAddressBuffer vertexData,
    StructuredBuffer<uint> indices,
    float3 light_dir,
    float3 light_color,
    uint num_lights,
    uint num_emissive,
    uint max_bounce,
    float ray_max_distance,
    inout uint seed)
{
    float3 total_radiance = float3(0.0, 0.0, 0.0);
    float3 throughput = float3(1.0, 1.0, 1.0);
    float3 pos = origin;

    for (uint bounce = 0u; bounce < max_bounce; bounce++)
    {
        // Trace ray against TLAS.
        RayDesc ray;
        ray.Origin = pos;
        ray.Direction = dir;
        ray.TMin = 0.01;
        ray.TMax = ray_max_distance;

        // Inline ray query WITHOUT RAY_FLAG_FORCE_OPAQUE: this surfaces every
        // triangle as a non-opaque candidate so we can alpha-test it in the
        // proceed loop. PrismaRev has no any-hit shader (ray-query only), so
        // alpha-masked / blend materials (e.g. Sponza dirt_decal) must be
        // culled here - otherwise their transparent texels render as solid
        // black geometry. Opaque materials (alpha >= 0.5) are committed and
        // become the closest hit; cutout texels are skipped so the ray
        // continues to whatever is behind them.
        RayQuery<RAY_FLAG_NONE> q;
        q.TraceRayInline(tlas, RAY_FLAG_NONE, 0xFF, ray);
        while (q.Proceed())
        {
            if (q.CandidateType() == CANDIDATE_NON_OPAQUE_TRIANGLE)
            {
                // Resolve the candidate's albedo alpha at its barycentric UV
                // and commit only if it passes the cutout threshold. This
                // mirrors the rasterizer's `if (sampled.a < 0.5) discard;`.
                uint cand_inst = q.CandidateInstanceID();
                uint cand_prim = q.CandidatePrimitiveIndex();
                float2 cand_bary = q.CandidateTriangleBarycentrics();
                float3 cv0, cv1, cv2;
                float2 cuv0, cuv1, cuv2;
                float3 cn0, cn1, cn2;
                read_hit_triangle(cand_inst, cand_prim, vertexData, indices,
                    cv0, cv1, cv2, cuv0, cuv1, cuv2, cn0, cn1, cn2);
                float cw = 1.0 - cand_bary.x - cand_bary.y;
                float2 cand_uv = cw * cuv0 + cand_bary.x * cuv1 + cand_bary.y * cuv2;
                PtInstanceMeta cand_meta = instance_meta[cand_inst];
                GpuMaterial cand_mat = materials[cand_meta.material_slot];
                float cand_alpha = mat_albedo_alpha(cand_mat, cand_uv);
                if (cand_alpha >= 0.5)
                {
                    q.CommitNonOpaqueTriangleHit();
                }
            }
        }

        if (q.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
        {
            // Instance index -> per-instance metadata -> material slot.
            uint instance_id = q.CommittedInstanceID();
            uint prim_idx = q.CommittedPrimitiveIndex();

            float3 v0, v1, v2;
            float2 uv0, uv1, uv2;
            float3 n0, n1, n2;
            read_hit_triangle(instance_id, prim_idx, vertexData, indices,
                v0, v1, v2, uv0, uv1, uv2, n0, n1, n2);

            float2 bary = q.CommittedTriangleBarycentrics();
            float w0 = 1.0 - bary.x - bary.y;
            float3 hit_pos = pos + dir * q.CommittedRayT();

            // Geometric normal from winding - used only as a degenerate-triangle
            // guard and to flip the shading normal to face the incoming ray.
            float3 edge1 = v1 - v0;
            float3 edge2 = v2 - v0;
            float3 geo_normal = cross(edge1, edge2);
            float tri_area2 = dot(geo_normal, geo_normal);
            if (tri_area2 < 1e-12)
                break; // degenerate triangle, terminate this path
            geo_normal = geo_normal * rsqrt(tri_area2);

            // Shading normal: interpolate the artist-authored vertex normals and
            // flip to face the incoming ray (faceforward). Mirrors truvis
            // `closest_hit`, which uses `faceforward(origin_normal, -ray_dir,
            // origin_normal)` so double-sided / thin-shell geometry (Sponza
            // walls) shades correctly from either side.
            float3 hit_normal = normalize(w0 * n0 + bary.x * n1 + bary.y * n2);
            if (dot(hit_normal, dir) > 0.0)
                hit_normal = -hit_normal;

            // View direction (surface -> camera/ray-origin).
            float3 view_dir = -dir;

            // Resolve the full PBR material (albedo + metallic + roughness +
            // emissive) at the hit UV.
            PtInstanceMeta meta = instance_meta[instance_id];
            GpuMaterial mat = materials[meta.material_slot];
            float2 hit_uv = w0 * uv0 + bary.x * uv1 + bary.y * uv2;
            SurfaceMaterial surf = resolve_material(mat, hit_uv);

            // Emissive contribution at this vertex (scaled by current throughput).
            total_radiance += throughput * surf.emissive;

            // Direct sun (NEE): Lo = BRDF * radiance * NdotL * shadow. The BRDF
            // already includes NdotL, so we multiply radiance * shadow only.
            float shadow = 1.0;
            if (dot(hit_normal, light_dir) > 0.0)
            {
                RayDesc shadow_ray;
                // Offset along the light direction (truvis convention) rather
                // than the normal - reduces self-intersection on thin shells
                // where the normal offset can still leave the origin inside
                // the geometry.
                shadow_ray.Origin = hit_pos + 0.0001 * light_dir;
                shadow_ray.Direction = light_dir;
                shadow_ray.TMin = 0.001;
                shadow_ray.TMax = ray_max_distance;
                // Same alpha-cutout test as the primary ray: a decal's
                // transparent texels must not cast shadow, otherwise the
                // cutout geometry that lets light through the decal would
                // still be shadowed by it.
                RayQuery<RAY_FLAG_SKIP_CLOSEST_HIT_SHADER> sq;
                sq.TraceRayInline(tlas, RAY_FLAG_SKIP_CLOSEST_HIT_SHADER, 0xFF, shadow_ray);
                while (sq.Proceed())
                {
                    if (sq.CandidateType() == CANDIDATE_NON_OPAQUE_TRIANGLE)
                    {
                        uint s_inst = sq.CandidateInstanceID();
                        uint s_prim = sq.CandidatePrimitiveIndex();
                        float2 s_bary = sq.CandidateTriangleBarycentrics();
                        float3 sv0, sv1, sv2;
                        float2 suv0, suv1, suv2;
                        float3 sn0, sn1, sn2;
                        read_hit_triangle(s_inst, s_prim, vertexData, indices,
                            sv0, sv1, sv2, suv0, suv1, suv2, sn0, sn1, sn2);
                        float sw = 1.0 - s_bary.x - s_bary.y;
                        float2 s_uv = sw * suv0 + s_bary.x * suv1 + s_bary.y * suv2;
                        PtInstanceMeta s_meta = instance_meta[s_inst];
                        GpuMaterial s_mat = materials[s_meta.material_slot];
                        if (mat_alpha_pass(s_mat, s_uv))
                        {
                            // First opaque-enough blocker ends the shadow query.
                            sq.CommitNonOpaqueTriangleHit();
                            break;
                        }
                    }
                }
                if (sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
                    shadow = 0.0;
            }
            // Sun NEE contribution (directional light from push constants).
            // MIS power-heuristic: w_mis = p_nee / (p_nee^2 + p_brdf^2).
            // p_nee_sun = 1.0 (deterministic direction, always sampled).
            float3 brdf_cos = eval_brdf(surf, hit_normal, view_dir, light_dir);
            float p_brdf_sun = eval_brdf_pdf(surf, hit_normal, view_dir, light_dir);
            float p_nee_sun = 1.0;
            total_radiance += throughput * brdf_cos * light_color * shadow
                * p_nee_sun / max(p_nee_sun * p_nee_sun + p_brdf_sun * p_brdf_sun, 1e-20);

            // Multi-light NEE: iterate over all SSBO analytic lights.
            // Zeroed (unused) entries contribute < 1e-8 and are skipped early.
            for (uint li = 0u; li < PT_LIGHT_MAX; li++)
            {
                PtAnalyticLight al = ptLights[li];
                float4 al_color = al.color_params;
                // Skip empty entries (color ~= 0 and params ~= 0)
                if (dot(al_color.rgb, al_color.rgb) < 1e-8 && abs(al_color.w) < 1e-8)
                    continue;

                // Resolve light direction and squared distance per type.
                float3 L_dir;
                float L_dist2 = 1.0;
                float3 L_radiance = al_color.rgb;
                uint kind = uint(al.position_kind.w);

                if (kind == uint(PtLightKind::Directional))
                {
                    L_dir = -normalize(al.direction_params.xyz);
                    // Directional: no distance falloff; radiance baked into color_params.rgb
                }
                else if (kind == uint(PtLightKind::Point) || kind == uint(PtLightKind::Spot))
                {
                    L_dir = al.position_kind.xyz - hit_pos;
                    L_dist2 = dot(L_dir, L_dir);
                    float range = al_color.w;
                    float dist = sqrt(L_dist2);
                    L_dir = L_dir / max(dist, 1e-8);

                    // Spot light cone check
                    if (kind == uint(PtLightKind::Spot))
                    {
                        float3 spot_dir = normalize(al.direction_params.xyz);
                        float cos_ang = dot(-L_dir, spot_dir);
                        float inner_cos = al.direction_params.w;
                        float outer_cos = al_color.w;
                        if (cos_ang < outer_cos)
                            continue; // outside spot cone
                        // Smooth falloff between inner and outer cone
                        float t = (cos_ang - outer_cos) / max(inner_cos - outer_cos, 1e-8);
                        L_radiance *= clamp(t, 0.0, 1.0);
                    }

                    // Distance culling + inverse-square falloff
                    if (range > 0.0 && dist > range)
                        continue;
                    float falloff = 1.0 / max(L_dist2, 1e-8);
                    L_radiance *= falloff;
                }
                else if (kind == uint(PtLightKind::Area))
                {
                    float3 center = al.position_kind.xyz;
                    float radius = al.direction_params.w;
                    L_dir = center - hit_pos;
                    L_dist2 = dot(L_dir, L_dir);
                    float dist = sqrt(L_dist2);
                    L_dir = L_dir / max(dist, 1e-8);
                    // Solid-angle approx for disk of radius r at distance d: pi * r^2 / d^2
                    float solid_angle = PI * radius * radius / max(L_dist2, 1e-8);
                    L_radiance *= clamp(solid_angle, 0.0, 1.0);
                }
                else
                {
                    continue; // unknown kind, skip
                }

                // Shadow ray for this light
                float al_shadow = 1.0;
                if (dot(hit_normal, L_dir) > 0.0)
                {
                    RayDesc sr;
                    sr.Origin = hit_pos + 0.0001 * L_dir;
                    sr.Direction = L_dir;
                    sr.TMin = 0.001;
                    sr.TMax = ray_max_distance;
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
                            if (mat_alpha_pass(smat, suv))
                            {
                                sq.CommitNonOpaqueTriangleHit();
                                break;
                            }
                        }
                    }
                    if (sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
                        al_shadow = 0.0;
                }

                // Analytic lights (point/spot/area) are not in the TLAS, and
                // point/spot are delta distributions: BRDF sampling can never
                // hit them, so NEE is the only strategy and there is nothing
                // to MIS against. The old `nee_pdf_point` fed the
                // light-selection probability 1/N into the power heuristic as
                // if it were a per-steradian pdf; a correct solid-angle pdf
                // for an area-sampled light needs the d^2 / cos_light
                // Jacobian, which a delta light does not have. That unit
                // mismatch scaled every analytic light down by
                // p_nee^2 / (p_nee^2 + p_brdf^2) (about 6% for a single
                // light), so the estimator now takes the raw contribution.
                float3 al_brdf = eval_brdf(surf, hit_normal, view_dir, L_dir);
                total_radiance += throughput * al_brdf * L_radiance * al_shadow;
            }

            // ---- Emissive triangle NEE (binding 9) ----
            // Uniformly pick one emissive triangle, uniformly sample its
            // surface, and evaluate direct lighting with MIS.
            if (num_emissive > 0u)
            {
                uint ei = uint(rand_float(seed) * float(num_emissive));
                ei = min(ei, num_emissive - 1u);
                PtEmissiveTri et = ptEmissive[ei];

                // Uniformly sample a point on the triangle via barycentric coords
                float r1 = sqrt(rand_float(seed));
                float r2 = rand_float(seed);
                float b1 = 1.0 - r1;
                float b2 = r2 * r1;
                float b0 = 1.0 - b1 - b2;
                float3 et_pt = b0 * et.v0.xyz + b1 * et.v1.xyz + b2 * et.v2.xyz;
                float3 et_n = et.normal.xyz;

                float3 to_light = et_pt - hit_pos;
                float dist2 = dot(to_light, to_light);
                float dist = sqrt(dist2);
                float3 L_dir = to_light / max(dist, 1e-8);
                float cos_light = abs(dot(et_n, -L_dir));

                // PDF: p(point) = 1 / (num_emissive * area);
                // Jacobian to solid angle: d^2 / |cos_light|
                float p_nee_et = (1.0 / max(float(num_emissive) * et.area, 1e-8))
                               * dist2 / max(cos_light, 1e-8);

                float et_shadow = 1.0;
                if (dot(hit_normal, L_dir) > 0.0 && cos_light > 1e-4)
                {
                    RayDesc sr;
                    sr.Origin = hit_pos + 0.0001 * L_dir;
                    sr.Direction = L_dir;
                    sr.TMin = 0.001;
                    sr.TMax = max(dist - 0.001, 0.001);
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
                            if (mat_alpha_pass(smat, suv))
                            {
                                sq.CommitNonOpaqueTriangleHit();
                                break;
                            }
                        }
                    }
                    if (sq.CommittedStatus() == COMMITTED_TRIANGLE_HIT)
                        et_shadow = 0.0;
                }

                float3 et_brdf = eval_brdf(surf, hit_normal, view_dir, L_dir);
                float p_brdf_et = eval_brdf_pdf(surf, hit_normal, view_dir, L_dir);
                total_radiance += throughput * et_brdf * et.radiance.rgb * et_shadow
                    * p_nee_et / max(p_nee_et * p_nee_et + p_brdf_et * p_brdf_et, 1e-20);
            }

            // Russian roulette termination.
            float p = max(max(throughput.r, throughput.g), throughput.b);
            if (bounce + 1u >= max_bounce)
                break;
            if (p < 1e-4)
                break;

            float surv = min(p, 1.0);
            float rr = rand_float(seed);
            if (rr >= surv)
                break;
            throughput /= surv;

            // Sample next direction. For transmissive surfaces (glass) use
            // Fresnel split between mirror reflection and refraction; for
            // opaque surfaces use the standard diffuse/GGX BRDF sampling.
            if (surf.transmission > 0.0 && surf.metallic < 0.5)
            {
                // ---- Glass: Fresnel reflection / refraction ----
                float cos_theta = max(dot(hit_normal, view_dir), 0.0);
                float F = fresnel_dielectric(cos_theta, surf.ior);

                if (rand_float(seed) < F)
                {
                    // Reflection: mirror direction. PDF = F (Russian roulette
                    // selection probability), cancels F term from mirror BRDF.
                    float3 refl_dir = reflect(-view_dir, hit_normal);
                    dir = refl_dir;
                    pos = hit_pos + 0.0001 * dir;
                    // throughput unchanged (F * throughput / F = throughput)
                }
                else
                {
                    // Refraction: Snell's law (air → material).
                    // I = dir (toward surface), N = hit_normal (facing outward).
                    float eta_out_in = 1.0 / surf.ior;
                    float3 refr_dir = refract(dir, hit_normal, eta_out_in);

                    // Total internal reflection check
                    if (dot(refr_dir, refr_dir) < 0.001)
                    {
                        float3 refl_dir = reflect(-view_dir, hit_normal);
                        dir = refl_dir;
                        pos = hit_pos + 0.0001 * dir;
                    }
                    else
                    {
                        // Transmit through the surface
                        dir = normalize(refr_dir);
                        pos = hit_pos - 0.0001 * dir; // offset inside material
                        // Thin-surface Beer's law: trans_color tints the
                        // transmitted radiance. Divide by (1 - F) to
                        // compensate for Russian roulette selection probability.
                        throughput *= surf.trans_color / max(1.0 - F, 1e-7);
                    }
                }
            }
            else
            {
                // ---- Opaque surface: diffuse/GGX BRDF sampling ----
                float prob_diffuse = surf.roughness;
                float3 new_dir;
                float brdf_pdf;
                if (rand_float(seed) < prob_diffuse)
                {
                    float u1 = rand_float(seed);
                    float u2 = rand_float(seed);
                    new_dir = cosine_hemisphere_sample(hit_normal, u1, u2);
                    brdf_pdf = eval_brdf_pdf(surf, hit_normal, view_dir, new_dir);
                }
                else
                {
                    float real_roughness = surf.roughness * surf.roughness;
                    float3 h = ggx_sample(seed, real_roughness, hit_normal);
                    new_dir = reflect(-view_dir, h);
                    // Reject directions below the surface (can happen with rough
                    // GGX samples on near-grazing angles).
                    if (dot(hit_normal, new_dir) <= 0.0)
                        break;
                    brdf_pdf = eval_brdf_pdf(surf, hit_normal, view_dir, new_dir);
                }

                if (brdf_pdf <= 1e-7)
                    break;

                float3 brdf_cos_new = eval_brdf(surf, hit_normal, view_dir, new_dir);
                throughput *= brdf_cos_new / brdf_pdf;

                // Offset origin along the new direction (truvis convention) to
                // avoid self-intersection on the next TraceRay.
                pos = hit_pos + 0.0001 * new_dir;
                dir = new_dir;
            }
        }
        else
        {
            // Missed the scene: HDR environment map (IBL envCube).
            total_radiance += throughput * envCube.SampleLevel(dir, 0.0).rgb;
            break;
        }
    }

    return total_radiance;
}
