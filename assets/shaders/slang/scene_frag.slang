// ForwardPass bindless PBR fragment stage.
//
// This is the RenderGraph-path counterpart of `bindless.slang` (legacy
// renderer): full PBR with material textures sampled through the bindless
// SRV table + material SSBO, with real IBL (irradiance cubemap, prefiltered
// environment map, and BRDF LUT) and the rasterized shadow map.
//
// Sets (must match `ForwardPass::ensure_pipeline`):
//   set 0, binding 0 - per-frame UBO (FrameUBO, now carries lightViewProj)
//   set 0, binding 1 - material SSBO (GpuMaterial[])
//   set 0, binding 2 - point-light SSBO (GpuLight[])
//   set 1, binding 0 - bindless SamplerState[4] (globalSamplers)
//   set 1, binding 1 - bindless Texture2D[] (bindlessSrvs)
//   set 2, binding 0 - IBL environment cubemap (SamplerCube, combined)
//   set 2, binding 1 - IBL diffuse irradiance cubemap (SamplerCube, combined)
//   set 2, binding 2 - IBL prefiltered environment map (SamplerCube, combined)
//   set 3, binding 0 - shadow map depth texture
//   set 3, binding 1 - shadow comparison sampler
//   set 4, binding 0 - previous-frame GTAO R8 visibility texture (Sampler2D, combined)
//                      Sampled only when PBR_FLAG_AO is set; 1-frame latency.
//   set 5, binding 0 - probe volume 3D texture (Texture3D, RGBA16F, integer Load)
//   set 5, binding 1 - probe volume grid metadata UBO (ProbeVolumeInfo)
// brdf LUT: registered in the bindless Texture2D[] table at slot pc.env_handle
//           (the env_handle push constant is repurposed for the BRDF LUT index).
// Push constants: PbrBindlessPush (96 bytes, mirrors Rust
//                  PbrBindlessPushConstants). lightViewProj lives in the
//                  UBO so the push constant stays under 128 bytes.
//
// MRT: the fragment stage now writes two color attachments:
//   SV_Target0 - linear HDR lit color (PostPass tonemaps this -> swapchain)
//   SV_Target1 - view-space normal (R16G16B16A16_SFLOAT, xyz unit, w=0)
//                consumed by the GTAO pass that runs after ForwardPass.
//
// Compile:
//   slangc scene_frag.slang -profile spirv_1_5 -target spirv \
//          -entry fragmentMain -stage fragment -fvk-use-entrypoint-name \
//          -o scene_frag.frag.spv
import common;
import gi;

// ---- set 0, binding 0 - per-frame UBO (unchanged) -------------------
[[vk::binding(0, 0)]]
ConstantBuffer<FrameUBO> frame;

// ---- set 0, binding 1 - material SSBO --------------------------------
// One record per material; the shader indexes by `pc.material_slot`.
// Layout must match Rust `GpuMaterial` (96 bytes, 16-byte aligned).
struct GpuMaterial
{
    float4 base_color;                          // offset 0
    float4 metallic_roughness_emissive;         // offset 16: x=metallic, y=roughness, z=emissive, w=emissive_strength
    uint   albedo_idx;                          // offset 32
    uint   normal_idx;                          // offset 36
    uint   metallic_roughness_idx;             // offset 40
    uint   emissive_idx;                        // offset 44
    // Second block (advanced PBR)
    float4 transmission_factor;                 // offset 48: x=transmission, y=ior, z=translucency, w=anisotropy
    float4 clearcoat;                           // offset 64: x=clearcoat, y=clearcoat_roughness, z=reserved, w=reserved
    uint   transmission_tex_idx;                // offset 80
    uint   occlusion_idx;                       // offset 84: R channel, attenuates IBL only
    float  normal_scale;                        // offset 88: normal map strength
    float  occlusion_strength;                  // offset 92: lerp factor for occlusion R into IBL
};
[[vk::binding(1, 0)]]
StructuredBuffer<GpuMaterial> materials;

// ---- set 0, binding 2 - local-light SSBO --------------------------------
// Up to LIGHT_MAX (8) local lights (point + spot). Layout matches Rust
// `GpuLight` (64 bytes): `.color.w` is the kind (0 = point, 1 = spot).
struct GpuLight
{
    float4 position;   // xyz = world pos, w = range (attenuation radius)
    float4 color;      // rgb = radiant intensity, w = kind (0 = point, 1 = spot)
    float4 direction;  // xyz = light forward, w = cos(outer half-angle)
    float4 params;     // x = cos(inner half-angle), yzw reserved
};
[[vk::binding(2, 0)]]
StructuredBuffer<GpuLight> pointLights;

// ---- set 1, binding 0 - bindless SAMPLER array (4 modes) ------------
[[vk::binding(0, 1)]]
SamplerState globalSamplers[];

// ---- set 1, binding 1 - bindless SAMPLED_IMAGE array -----------------
// Slot 0 is the magenta fallback; the rest hold user textures (albedo,
// normal, metallic-roughness, emissive, etc.).
[[vk::binding(1, 1)]]
Texture2D bindlessSrvs[];

// ---- set 2, binding 0 - IBL environment cubemap ----------------------
[[vk::binding(0, 2)]]
SamplerCube envCube;

// ---- set 2, binding 1 - IBL diffuse irradiance cubemap -----------------
[[vk::binding(1, 2)]]
SamplerCube irradianceCube;

// ---- set 2, binding 2 - IBL prefiltered environment map -----------------
[[vk::binding(2, 2)]]
SamplerCube prefilteredCube;

// ---- set 3 - rasterized shadow map (depth) + comparison sampler ------
[[vk::binding(0, 3)]]
Texture2D<float> shadowMap;
[[vk::binding(1, 3)]]
SamplerComparisonState shadowSampler;

// ---- set 4, binding 0 - previous-frame GTAO visibility (R8_UNORM) ----
// Half-resolution R8 texture produced by the GTAO pass the previous frame.
// Sampled with a linear sampler so the half-res AO is smoothly upsampled to
// full resolution here. 1.0 = unoccluded, 0.0 = fully occluded. Only applied
// to the IBL diffuse + specular terms when PBR_FLAG_AO is set (bit 14).
[[vk::binding(0, 4)]]
Sampler2D aoTex;

// ---- set 5, binding 0 - probe volume 3D texture (RGBA16F) ----
// Regular grid of order-2 SH probes; coefficient c occupies depth slices
// [c*dims.z, (c+1)*dims.z). Sampled with integer Load + manual trilinear in
// gi.slang (no hardware sampler — avoids cross-coefficient bleed).
[[vk::binding(0, 5)]]
Texture3D probeVolume;

// ---- set 5, binding 1 - probe volume grid metadata UBO ----
[[vk::binding(1, 5)]]
ConstantBuffer<ProbeVolumeInfo> probeInfo;

// ---- push constants -------------------------------------------------
// Byte layout mirrors Rust `PbrBindlessPushConstants` (96 bytes):
//   model(64) | material_slot(4) | env_handle(4) | albedo_idx(4) | normal_idx(4)
//   | debug_flags(4) | bone_offset(4) | skin_mode(4) | _pad(4) | _padding(12)
struct PbrBindlessPush
{
    float4x4 model;
    uint     material_slot;
    uint     env_handle;
    uint     albedo_idx;
    uint     normal_idx;
    uint     debug_flags;   // PBR component toggle bitmask (15 bits)
    uint     bone_offset;   // #560 P0-3: first bone palette matrix for this draw
    uint     skin_mode;     // 0 = static mesh, 1 = skinned (reads set 0 binding 3)
    uint     _pad_92;
    uint3    _padding;
};
[[vk::push_constant]]
PbrBindlessPush pc;

// PBR component toggle bits. Bit set = component active; bit unset = component
// uses its neutral value. flags=0 -> raw baseColor (unlit). Order matches the
// keyboard map in app.rs (keys 1-9, 0, Shift+1..Shift+4).
static const uint PBR_FLAG_DIRECT       = 0u;  // 1: direct lighting (NdotL factor)
static const uint PBR_FLAG_SHADOW       = 1u;  // 2: shadow map attenuation
static const uint PBR_FLAG_SPECULAR     = 2u;  // 3: specular (direct + IBL)
static const uint PBR_FLAG_METALLIC     = 3u;  // 4: real metallic (else 0)
static const uint PBR_FLAG_ROUGHNESS    = 4u;  // 5: real roughness (else 0.5)
static const uint PBR_FLAG_DIFFUSE_IBL  = 5u;  // 6: IBL diffuse irradiance
static const uint PBR_FLAG_SPECULAR_IBL = 6u;  // 7: IBL specular (prefiltered+LUT)
static const uint PBR_FLAG_MULTILIGHT   = 7u;  // 8: extra point lights
static const uint PBR_FLAG_AO           = 8u;  // 9: GTAO ambient occlusion (attenuates IBL diffuse+specular)
static const uint PBR_FLAG_EMISSIVE     = 9u;  // 0: emissive
static const uint PBR_FLAG_TRANSMISSION = 10u; // Shift+1
static const uint PBR_FLAG_TRANSLUCENCY = 11u; // Shift+2
static const uint PBR_FLAG_ANISOTROPY   = 12u; // Shift+3
static const uint PBR_FLAG_CLEARCOAT    = 13u; // Shift+4
static const uint PBR_FLAG_GI           = 14u; // Shift+5: probe-volume indirect diffuse


bool flag(uint b) { return (pc.debug_flags & (1u << b)) != 0u; }

// Whether a PBR component should be *computed* this frame. In normal render
// mode (`debug_flags == 0`) every component is on; in isolate mode only the
// single selected component is computed (the rest stay at their neutral 0 so
// they contribute nothing, and the isolate branch in fragmentMain emits just
// the selected one as a grayscale visualization).
bool on(uint b) { return pc.debug_flags == 0u || flag(b); }

static const float PI          = 3.14159265359;
static const float PREFILTER_MAX_LOD = 4.0; // PREFILTERED_MIP_LEVELS - 1

static const uint SAMPLER_LINEAR_WRAP  = 0u;
static const uint SAMPLER_LINEAR_CLAMP = 1u;
static const uint SAMPLER_NEAREST      = 2u;
static const uint SAMPLER_SHADOW       = 3u;

float3 fresnel_schlick(float cos_theta, float3 f0)
{
    return f0 + (1.0 - f0) * pow(clamp(1.0 - cos_theta, 0.0, 1.0), 5.0);
}

float3 fresnel_schlick_roughness(float cos_theta, float3 f0, float roughness)
{
    return f0 + (max(float3(1.0 - roughness), f0) - f0) * pow(clamp(1.0 - cos_theta, 0.0, 1.0), 5.0);
}

float distribution_ggx(float3 n, float3 h, float roughness)
{
    float a = roughness * roughness;
    float a2 = a * a;
    float n_dot_h = max(dot(n, h), 0.0);
    float n_dot_h2 = n_dot_h * n_dot_h;
    float denom = n_dot_h2 * (a2 - 1.0) + 1.0;
    return a2 / max(PI * denom * denom, 1e-5);
}

float geometry_schlick_ggx(float n_dot_v, float roughness)
{
    float r = roughness + 1.0;
    float k = (r * r) / 8.0;
    return n_dot_v / (n_dot_v * (1.0 - k) + k);
}

float geometry_smith(float3 n, float3 v, float3 l, float roughness)
{
    float n_dot_v = max(dot(n, v), 0.0);
    float n_dot_l = max(dot(n, l), 0.0);
    return geometry_schlick_ggx(n_dot_v, roughness) * geometry_schlick_ggx(n_dot_l, roughness);
}

/// Project a world position into the shadow map and return a visibility factor
/// in [0,1] (1.0 = fully lit, 0.0 = fully shadowed) using a comparison sampler
/// with 3x3 (9-tap) PCF. Returns 1.0 outside the light's frustum (treated as
/// lit). `lightViewProj` comes from the per-frame UBO.
///
/// `n` is the surface normal, used for a normal bias: the world position is
/// offset along the normal before projection so the receiver samples a texel
/// away from its own silhouette, reducing self-shadow acne without the
/// peter-panning that a pure depth bias causes.
float sample_shadow(float3 world_pos, float3 n)
{
    // Normal bias: push the receiver slightly along the geometric normal so it
    // samples a neighbour texel instead of its own (acne-prone) silhouette.
    // The amount is small (world units) - tuned for the ~1-unit scene scale.
    float3 biased_pos = world_pos + n * 0.02;
    float4 light_space = mul(frame.lightViewProj, float4(biased_pos, 1.0));
    if (light_space.w <= 0.0)
        return 1.0;
    float3 proj = light_space.xyz / light_space.w;
    // NDC [-1,1] -> UV [0,1]. The shadow map was rendered with an
    // orthographic projection that has NO y-flip (unlike the camera's
    // perspective, which flips via p[1][1] = -inv_tan). In Vulkan the
    // framebuffer origin is top-left, so NDC y=-1 already maps to v=0 with no
    // extra flip. Flipping here would mirror the lookup vertically and offset
    // every shadow. Both occluder and receiver use the same lightViewProj, so
    // they must sample the same texel: uv = proj.xy * 0.5 + 0.5.
    float2 uv = float2(proj.x * 0.5 + 0.5, proj.y * 0.5 + 0.5);
    float depth = proj.z; // orthographic light projection -> already [0,1]
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0 || depth > 1.0)
        return 1.0;
    // 3x3 PCF: average 9 comparison samples in a texel-spaced grid. Each
    // SampleCmpLevelZero returns 1.0 (pass) or 0.0 (fail) with hardware 2x2
    // bilinear filtering, so the 9-tap average gives a soft shadow edge.
    // The texel step comes from the resource, not from a guess: the pass
    // happens to render 2048² today (`SHADOW_MAP_SIZE` in shadow_map_pass.rs),
    // and a hardcoded 1/2048 here would silently blur/sharpen the moment that
    // resolution changes.
    uint2 shadow_extent;
    shadowMap.GetDimensions(shadow_extent.x, shadow_extent.y);
    float2 texel = 1.0 / float2(max(shadow_extent.x, 1u), max(shadow_extent.y, 1u));
    float sum = 0.0;
    for (int y = -1; y <= 1; y++)
    {
        for (int x = -1; x <= 1; x++)
        {
            float2 offset = float2(float(x), float(y)) * texel;
            sum += shadowMap.SampleCmpLevelZero(shadowSampler, uv + offset, depth);
        }
    }
    // `Light.shadowStrength`: 0 = fully lit, 1 = full shadow.
    // (`lerp` 在 0 时直接返回 1.0，所以 shadows = None 的灯不会留下脏阴影。)
    return lerp(1.0, sum / 9.0, saturate(frame.shadowStrength));
}

// ---- IBL sampling (real resources). ----
// irradianceCube (set 2 binding 1), prefilteredCube (set 2 binding 2),
// brdfLUT (bindless Texture2D[] at pc.env_handle, sampled with LINEAR_CLAMP).
float3 sample_irradiance(float3 n)
{
    return irradianceCube.SampleLevel(n, 0.0).rgb;
}
float3 sample_specular(float3 r, float roughness)
{
    // Roughness maps to LOD. The prefiltered env map was convolved per-mip with
    // roughness = (mip/(MAX))^2 (Epic split-sum spec), so to find the mip whose
    // convolved roughness matches the surface roughness we invert: lod = sqrt(r)
    // * MAX. This keeps the sampling and generation sides in sync.
    return prefilteredCube.SampleLevel(r, sqrt(roughness) * PREFILTER_MAX_LOD).rgb;
}
float2 sample_brdf_lut(float n_dot_v, float roughness)
{
    // BRDF LUT is registered in the bindless texture table at slot pc.env_handle.
    // Sample with LINEAR_CLAMP addressing.
    return bindlessSrvs[NonUniformResourceIndex(pc.env_handle)]
        .SampleLevel(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_CLAMP)], float2(n_dot_v, roughness), 0.0).rg;
}

// ---- Multi-light (point lights). Reads from the pointLights SSBO (set 0
// binding 2). The active light count is passed via frame.cameraPosition.w.
// Point lights are NOT shadow-attenuated (only the directional light is).
float3 eval_point_lights(float3 n, float3 v, float3 world_pos, float3 albedo,
                         float3 f0, float metallic, float roughness, float shadow)
{
    uint count = uint(frame.cameraPosition.w);
    float3 result = float3(0.0);
    for (uint i = 0u; i < count; i++)
    {
        GpuLight L = pointLights[i];
        float3 to_light = L.position.xyz - world_pos;
        float  dist = length(to_light);
        float3 l = to_light / max(dist, 1e-4);
        float  atten = 1.0 / max(dist * dist, 1e-4);
        // Range cutoff (smooth).
        float r = L.position.w;
        atten *= saturate(1.0 - (dist / r));

        // Spot cone: smooth falloff between the inner and outer half angles.
        // `l` points surface -> light, so `-l` is the light -> surface direction
        // we compare against the light's forward axis (`transform.forward`).
        if (L.color.w > 0.5)
        {
            float cos_angle = dot(-l, normalize(L.direction.xyz));
            float denom = max(L.params.x - L.direction.w, 1e-4);
            float spot = saturate((cos_angle - L.direction.w) / denom);
            spot *= spot;
            if (spot <= 0.0) continue;
            atten *= spot;
        }
        // Point-light units are physical: `L.color.rgb` carries luminous
        // intensity in candela (color * intensity, packed on the CPU side).
        // Inverse-square attenuation (1/dist^2) converts candela to lux
        // (illuminance), and the BRDF divides by PI for radiance. `exposure`
        // scales the final value for tone-mapping control.
        float3 radiance = L.color.rgb * (atten / PI);
        float3 h = normalize(v + l);
        float n_dot_l = max(dot(n, l), 0.0);
        if (n_dot_l <= 0.0) continue;

        float ndf = distribution_ggx(n, h, roughness);
        float g = geometry_smith(n, v, l, roughness);
        float3 f = fresnel_schlick(max(dot(h, v), 0.0), f0);
        float3 specular = (ndf * g * f) / (4.0 * max(dot(n,v),0.0) * n_dot_l + 1e-4);
        float3 kD = (float3(1.0) - f) * (1.0 - metallic);
        float3 diffuse = kD * albedo / PI;
        result += (diffuse + specular) * radiance * n_dot_l;
    }
    return result;
}

// ---- Advanced material BRDFs (gated by flag bits). Each function reads
// frame globals (frame.lightDirection, frame.lightColor) for the directional
// light, or uses the `l`/`radiance` parameters where already provided.
// All functions return 0.0 when the feature flag is off (the caller gates
// them with `if (flag(PBR_FLAG_*))`).

/// Thin-surface transmission approximation.
/// Computes light passing through the surface: when the material is
/// transparent, light from the back side bleeds through.
/// Uses frame.lightDirection and frame.lightColor internally.
float3 eval_transmission(GpuMaterial mat, MeshVaryings in, float3 v, float3 n)
{
    float3 light_dir = normalize(frame.lightDirection.xyz);
    float3 radiance = frame.lightColor.rgb * frame.lightDirection.w;
    float metallic = mat.metallic_roughness_emissive.x;
    float3 base_color = mat.base_color.rgb;
    float transmission = mat.transmission_factor.x;

    // Back-face lighting: dot(-n, light_dir) instead of dot(n, light_dir)
    float n_dot_l_back = max(dot(-n, light_dir), 0.0);

    // Transmission contribution: factor * (1 - metallic) * base_color * radiance * back_NdotL
    return transmission * (1.0 - metallic) * base_color * radiance * n_dot_l_back;
}

/// Translucency / subsurface scattering approximation using wrap-lighting.
/// Softens the shadow terminator and allows light to bleed through thin
/// geometry. Uses frame light globals internally.
float3 eval_translucency(GpuMaterial mat, float3 n, float3 v, float3 l, float3 radiance)
{
    float3 base_color = mat.base_color.rgb;
    float translucency = mat.transmission_factor.z;

    // Wrap-lighting: spread the light around the terminator.
    // wrap = translucency amount, so at translucency=0 it's standard diffuse.
    float wrap = translucency;
    float n_dot_l_wrap = (max(dot(n, l) + wrap, 0.0) / (1.0 + wrap));

    // Subsurface color bleeds through; use base_color * radiance.
    return translucency * base_color * radiance * n_dot_l_wrap;
}

/// Anisotropic GGX specular lobe.
/// Uses the tangent direction to build a TBN basis and computes
/// an anisotropic NDF that stretches the specular highlight along
/// the tangent direction based on mat.transmission_factor.w (anisotropy).
float3 eval_anisotropy(GpuMaterial mat, float3 n, float3 v, float3 l, float3 h,
                       float roughness, float3 f0, float3 radiance, float shadow,
                       float3 tangent)
{
    float anisotropy = mat.transmission_factor.w;
    if (anisotropy <= 0.0)
        return float3(0.0);

    // Build TBN from the world normal and tangent.
    float3 t = normalize(tangent);
    // Reject degenerate tangent by projecting onto the normal plane
    if (dot(t, t) < 0.001)
        t = normalize(cross(n, float3(0.0, 1.0, 0.0)));
    t = normalize(t - n * dot(n, t));
    float3 b = cross(n, t);

    // Transform half-vector to tangent space
    float3 h_ts = float3(dot(h, t), dot(h, b), dot(h, n));

    // Anisotropic roughness: stretch along tangent (ax) vs bitangent (ay)
    float ax = max(roughness * (1.0 + anisotropy), 0.001);
    float ay = max(roughness / (1.0 + anisotropy), 0.001);
    float ax2 = ax * ax;
    float ay2 = ay * ay;

    // Anisotropic NDF (GGX / Trowbridge-Reitz anisotropic)
    float hx2 = h_ts.x * h_ts.x;
    float hy2 = h_ts.y * h_ts.y;
    float hz2 = h_ts.z * h_ts.z;
    float denom = hx2 / ax2 + hy2 / ay2 + hz2;
    float d = 1.0 / (PI * ax * ay * denom * denom);

    // Geometry term (isotropic approximation)
    float n_dot_v = max(dot(n, v), 0.0);
    float n_dot_l = max(dot(n, l), 0.0);
    float g = geometry_smith(n, v, l, roughness);

    // Fresnel (same as isotropic)
    float3 f = fresnel_schlick(max(dot(h, v), 0.0), f0);

    // Specular BRDF
    float3 specular = (d * g * f) / (4.0 * n_dot_v * n_dot_l + 1e-4);
    return specular * radiance * n_dot_l * shadow;
}

/// Clearcoat: a second dielectric GGX specular lobe on top of the base
/// material. F0 is fixed at 0.04 (polyurethane/clear coat). The clearcoat
/// roughness is separate from the base roughness.
float3 eval_clearcoat(GpuMaterial mat, float3 n, float3 v, float3 l, float3 h,
                      float3 radiance, float shadow)
{
    float clearcoat_factor = mat.clearcoat.x;
    float clearcoat_roughness = mat.clearcoat.y;
    if (clearcoat_factor <= 0.0)
        return float3(0.0);

    float n_dot_v = max(dot(n, v), 0.0);
    float n_dot_l = max(dot(n, l), 0.0);
    if (n_dot_l <= 0.0 || n_dot_v <= 0.0)
        return float3(0.0);

    // Clearcoat uses a fixed F0 of 0.04 (dielectric)
    float3 f0_cc = float3(0.04);

    // Use the clearcoat roughness for the NDF and geometry
    float ndf_cc = distribution_ggx(n, h, clearcoat_roughness);
    float g_cc = geometry_smith(n, v, l, clearcoat_roughness);
    float3 f_cc = fresnel_schlick(max(dot(h, v), 0.0), f0_cc);

    // Clearcoat specular lobe
    float3 specular_cc = (ndf_cc * g_cc * f_cc) / (4.0 * n_dot_v * n_dot_l + 1e-4);
    return clearcoat_factor * specular_cc * radiance * n_dot_l * shadow;
}

// ---- MRT output struct -------------------------------------------------
// SV_Target0 = HDR lit color (post-tonemap, written to the swapchain).
// SV_Target1 = view-space normal (xyz unit, w=0), sampled by the GTAO pass.
//              View-space normal = (float3x3)frame.view * worldNormal. The
//              upper-left 3x3 of a rigid view matrix is orthonormal so the
//              result stays unit-length after normalize().
struct SceneOutput
{
    float4 color     : SV_Target0;
    float4 viewNormal: SV_Target1;
};

[shader("fragment")]
SceneOutput fragmentMain(MeshVaryings in)
{
    // ---- Material -------------------------------------------------------
    GpuMaterial mat = materials[pc.material_slot];

    // Base color: prefer the texture if a valid slot was bound, else use
    // the scalar. Texture index 0xFFFFFFFF means "no texture".
    float3 albedo;
    if (mat.albedo_idx != 0xFFFFFFFFu)
    {
        float4 sampled = bindlessSrvs[NonUniformResourceIndex(mat.albedo_idx)]
            .Sample(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], in.uv);
        // Alpha-test for cut-out foliage / alpha-masked geometry (Sponza
        // plants): discard texels below the threshold so they don't render
        // as opaque quads. Opaque materials have alpha = 1 and pass through.
        if (sampled.a < 0.5)
        {
            discard;
        }
        // The albedo texture is created as R8G8B8A8_SRGB, so the hardware
        // performs the sRGB->linear conversion on sample. `sampled.rgb` is
        // already linear - no manual pow() needed.
        albedo = sampled.rgb;
    }
    else
    {
        albedo = mat.base_color.rgb;
    }

    // Metallic / roughness: same pattern, packed (B=metallic, G=roughness
    // per glTF) when a texture is present. Flag-gated: when the component
    // is off, use its neutral value (metallic=0, roughness=0.5).
    float metallic;
    float roughness;
    if (mat.metallic_roughness_idx != 0xFFFFFFFFu)
    {
        float4 mr = bindlessSrvs[NonUniformResourceIndex(mat.metallic_roughness_idx)]
            .Sample(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], in.uv);
        metallic  = mr.b * mat.metallic_roughness_emissive.x;
        roughness = mr.g * mat.metallic_roughness_emissive.y;
    }
    else
    {
        metallic  = mat.metallic_roughness_emissive.x;
        roughness = mat.metallic_roughness_emissive.y;
    }
    if (!flag(PBR_FLAG_METALLIC))  metallic   = 0.0;       // neutral: dielectric
    if (!flag(PBR_FLAG_ROUGHNESS)) roughness  = 0.5;       // neutral: mid roughness
    roughness = clamp(roughness, 0.04, 1.0);

    // Normal: tangent-space normal map if bound. There is no dedicated toggle
    // bit for the normal map among the 14 PBR components, so it is applied
    // whenever a valid slot is bound (neutral fallback = geometric normal).
    // TBN uses the glTF/MikkTSpace convention: bitangent = cross(N, T) * w,
    // where w is the tangent's handedness sign (tangent.w), so mirrored UVs
    // produce the correct bitangent direction.
    float3 n;
    if (mat.normal_idx != 0xFFFFFFFFu)
    {
        float3 n_ts = bindlessSrvs[NonUniformResourceIndex(mat.normal_idx)]
            .Sample(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], in.uv).xyz;
        n_ts = n_ts * 2.0 - 1.0;
        // normal_scale: scale the xy (tangent/bitangent) components before
        // normalize so the bump strength is adjustable. 1.0 = verbatim.
        n_ts.xy *= mat.normal_scale;
        float3 t = normalize(in.tangent.xyz);
        float  t_sign = in.tangent.w < 0.0 ? -1.0 : 1.0;
        float3 b = cross(in.worldNormal, t) * t_sign;
        n = normalize(n_ts.x * t + n_ts.y * b + n_ts.z * in.worldNormal);
    }
    else
    {
        n = normalize(in.worldNormal);
    }

    float3 v = normalize(frame.cameraPosition.xyz - in.worldPos);

    // Screen-space UV for sampling screen-space textures (the GTAO visibility
    // target). SV_Position.xy is the framebuffer pixel coordinate with a
    // top-left origin, which matches the GTAO pass's UV convention
    // (uv.y = 0 at the top). NOTE: the AO texture is screen-space, so it must
    // be sampled with this UV - NOT the mesh texture coordinate `in.uv`.
    float2 screen_uv = in.clipPos.xy / float2(frame.viewportW, frame.viewportH);

    float3 f0 = lerp(float3(0.04), albedo, metallic);

    // ---- Direct lighting (single directional, Cook-Torrance PBR) ----
    // Lo = (kD * albedo/PI + specular) * radiance * NdotL * shadow
    //   kD = (1 - F) * (1 - metallic)   (energy conservation: kD + kS <= 1)
    //   specular = D*G*F / (4 * NdotV * NdotL)
    // DIRECT gates the whole direct lobe; SPECULAR gates only the specular
    // part (off -> pure diffuse). SHADOW attenuates. Neutral (off) = 0.
    //
    // Light units are physical: `lightDirection.w` is the directional light's
    // illuminance in lux. Converting illuminance (lux) to radiance requires
    // dividing by PI (a Lambertian surface reflects lux/PI as radiance).
    // `exposure` scales the result so the app can keep real-world lux values
    // while controlling final image brightness.
    float3 light_dir = normalize(frame.lightDirection.xyz);
    float3 h         = normalize(v + light_dir);
    float  n_dot_l   = max(dot(n, light_dir), 0.0);
    float  n_dot_v   = max(dot(n, v), 0.0);
    float3 radiance  = frame.lightColor.rgb * frame.lightDirection.w;

    float  ndf = distribution_ggx(n, h, roughness);
    float  g   = geometry_smith(n, v, light_dir, roughness);
    float3 f   = fresnel_schlick(max(dot(h, v), 0.0), f0);
    float3 specular = (ndf * g * f) / (4.0 * n_dot_v * n_dot_l + 1e-4);
    float3 kD_direct = (float3(1.0) - f) * (1.0 - metallic);

    // Shadow (rasterized). 1.0 = lit, 0.0 = shadowed. Neutral = 1.0.
    float shadow = on(PBR_FLAG_SHADOW) ? sample_shadow(in.worldPos, n) : 1.0;

    float3 direct = float3(0.0);
    if (on(PBR_FLAG_DIRECT))
    {
        // Diffuse lobe (always part of direct when DIRECT is on).
        float3 diffuse = kD_direct * albedo / PI;
        // Specular lobe gated by SPECULAR (neutral = omitted).
        float3 spec_term = on(PBR_FLAG_SPECULAR) ? specular : float3(0.0);
        direct = (diffuse + spec_term) * radiance * n_dot_l * shadow;
    }

    // ---- IBL (image-based). Gated by AMBIENT_IBL master flag. ----
    // `frame.lightColor.w` is the ambient/IBL intensity factor (driven by the
    // inspector "Ambient (IBL)" slider; default 1.0). Scaling both IBL terms by
    // it lets the env contribution be tuned without regenerating the cubemaps.
    //
    // GTAO: when PBR_FLAG_AO (bit 14) is set, sample the previous frame's
    // half-res R8 visibility texture and attenuate both IBL terms. This is the
    // core fix for "IBL doesn't account for occlusion" - corners/crevices now
    // receive less ambient bounce. 1-frame latency is invisible in practice.
    float ibl_intensity = frame.lightColor.w;
    float3 f_ibl  = fresnel_schlick_roughness(max(dot(n, v), 0.0), f0, roughness);
    float3 kd_ibl = (float3(1.0) - f_ibl) * (1.0 - metallic);
    float3 diffuse_ibl  = float3(0.0);
    float3 specular_ibl = float3(0.0);
    float3 gi_diffuse   = float3(0.0); // captured for isolate-mode viz (line 739)

    // ---- GI (probe volume) replaces IBL when active. ----
    // When PBR_FLAG_GI is on (normal mode or GI isolate), the baked probe
    // volume provides indirect diffuse + specular, replacing the environment
    // cubemap entirely. The IBL env paths are skipped to avoid wasted sampling.
    //
    // AO & occlusion are applied *after* this branch (below) so they correctly
    // attenuate whichever indirect source is active — previously they were
    // applied to IBL values that GI then overwrote, discarding the occlusion.
    if (on(PBR_FLAG_GI))
    {
        // — Diffuse: probes at surface normal (irradiance SH, A_l factors) —
        // E(n) = ∑ c_lm * A_l * Y_lm(n) where A_l = [PI, 2*PI/3, PI/4]
        // are the Ramamoorthi & Hanrahan cosine-convolution factors applied
        // by EvalSH9Irradiance inside SampleProbeVolumeIrradiance.
        // Lambertian BRDF = kd * albedo / PI, so we divide E(n) by PI:
        //   gi_diffuse = kd * (E(n) / PI) * albedo
        float3 gi_irradiance = SampleProbeVolumeIrradiance(
            probeVolume, probeInfo, in.worldPos, n);
        gi_diffuse = kd_ibl * (gi_irradiance / PI) * albedo;
        diffuse_ibl = ibl_intensity * gi_diffuse;

        // — Specular: probes at reflection direction (radiance SH, no A_l) —
        // L(r) = ∑ c_lm * Y_lm(r) — direct SH reconstruction of incident
        // radiance at the reflection direction, feeding the split-sum
        // specular approximation. No further PI division needed since the
        // split-sum formula and BRDF LUT already contain the correct
        // Fresnel/roughness factors.
        if (on(PBR_FLAG_SPECULAR_IBL))
        {
            float3 r = reflect(-v, n);
            float3 gi_radiance = SampleProbeVolumeRadiance(
                probeVolume, probeInfo, in.worldPos, r);
            float2 brdf = sample_brdf_lut(max(dot(n, v), 0.0), roughness);
            specular_ibl = ibl_intensity * gi_radiance * (f_ibl * brdf.x + brdf.y);
        }
    }
    // ---- IBL (environment cubemap, only when GI is inactive). ----
    // Legacy fallback: sample the env irradiance and prefiltered cubemaps.
    // Only reached when GI is off (isolate mode or GI resources unavailable).
    else
    {
        // Diffuse IBL: real irradiance cube (set 2 binding 1).
        if (on(PBR_FLAG_DIFFUSE_IBL))
        {
            float3 irradiance = sample_irradiance(n);
            diffuse_ibl = ibl_intensity * kd_ibl * irradiance * albedo;
        }
        // Specular IBL: prefiltered env + BRDF LUT (set 2 binding 2/3).
        if (on(PBR_FLAG_SPECULAR_IBL))
        {
            float3 r = reflect(-v, n);
            float3 prefiltered = sample_specular(r, roughness);
            float2 brdf = sample_brdf_lut(max(dot(n, v), 0.0), roughness);
            specular_ibl = ibl_intensity * prefiltered * (f_ibl * brdf.x + brdf.y);
        }
    }

    // ---- AO & occlusion (attenuate whichever indirect source is active). ----
    // GTAO visibility: 1.0 = unoccluded, 0.0 = fully occluded. Apply only
    // to indirect terms (not direct lighting — direct already has its own
    // shadow map). Linear sampling smooths the half-res AO up to full res.
    if (on(PBR_FLAG_AO))
    {
        float ao = aoTex.SampleLevel(screen_uv, 0.0).r;
        // Soften: lerp between unoccluded (1.0) and the raw AO so the darkest
        // corners never lose all ambient light. 0.8 = AO at fully occluded
        // (ao~0) retains 20 % of the indirect energy.
        float ao_soft = lerp(1.0, ao, 0.8);
        diffuse_ibl  *= ao_soft;
        specular_ibl *= ao_soft;
    }
    // Material occlusion map (glTF occlusionTexture, R channel). Also affects
    // indirect only (never direct light), and is blended by `occlusion_strength`:
    // mix(1.0, tex.r, strength) so strength=0 ignores the map. Combined with
    // GTAO multiplicatively (both attenuate indirect light).
    if (mat.occlusion_idx != 0xFFFFFFFFu)
    {
        float occ_tex = bindlessSrvs[NonUniformResourceIndex(mat.occlusion_idx)]
            .Sample(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], in.uv).r;
        float occ = lerp(1.0, occ_tex, mat.occlusion_strength);
        diffuse_ibl  *= occ;
        specular_ibl *= occ;
    }

    // ---- Minimum ambient floor ----
    // After all IBL modifications (AO, occlusion, GI), ensure shadowed /
    // occluded areas never go to pure black. 8 % of albedo at default exposure
    // (1.0) gives dark stone ~6 sRGB (barely visible), and at max exposure
    // (5.0) gives ~28 sRGB (dark but clearly non-black).
    diffuse_ibl = max(diffuse_ibl, 0.08 * albedo);

    // ---- Emissive (gated). Neutral = 0. ----
    float3 emissive = float3(0.0);
    if (on(PBR_FLAG_EMISSIVE))
    {
        float emissive_intensity = mat.metallic_roughness_emissive.z;
        float emissive_strength  = mat.metallic_roughness_emissive.w;
        emissive = emissive_intensity * emissive_strength;
        if (mat.emissive_idx != 0xFFFFFFFFu)
        {
            float3 e = bindlessSrvs[NonUniformResourceIndex(mat.emissive_idx)]
                .Sample(globalSamplers[NonUniformResourceIndex(SAMPLER_LINEAR_WRAP)], in.uv).rgb;
            emissive = e * emissive_intensity * emissive_strength;
        }
    }

    // ---- Multi-light: extra hard-coded point lights (gated). ----
    float3 multi_light = float3(0.0);
    if (on(PBR_FLAG_MULTILIGHT))
    {
        multi_light = eval_point_lights(n, v, in.worldPos, albedo, f0, metallic,
                                        roughness, shadow);
    }

    // ---- Advanced material terms (gated, neutral = 0). ----
    float3 transmission = float3(0.0);
    float3 translucency = float3(0.0);
    float3 aniso_spec   = float3(0.0);
    float3 clearcoat    = float3(0.0);
    if (on(PBR_FLAG_TRANSMISSION)) transmission = eval_transmission(mat, in, v, n);
    if (on(PBR_FLAG_TRANSLUCENCY)) translucency = eval_translucency(mat, n, v, light_dir, radiance);
    if (on(PBR_FLAG_ANISOTROPY))   aniso_spec   = eval_anisotropy(mat, n, v, light_dir, h, roughness, f0, radiance, shadow, in.tangent.xyz);
    if (on(PBR_FLAG_CLEARCOAT))    clearcoat    = eval_clearcoat(mat, n, v, light_dir, h, radiance, shadow);

    // ---- Compose (linear). ----
    // `debug_flags` doubles as a single-select PBR component isolator: when a
    // flag is set (by the app's digit keys Digit1..9/0/Shift+1..4), render
    // ONLY that one component as a grayscale visualization so each PBR term
    // can be eyeballed in isolation (e.g. Digit9 -> AO grayscale tells you
    // immediately whether GTAO is producing non-trivial values). When no flag
    // is set (default), render the full PBR lit scene with all features on.
    if (pc.debug_flags != 0u)
    {
        float3 isolated = float3(0.0);
        if (flag(PBR_FLAG_DIRECT))       isolated = direct;
        else if (flag(PBR_FLAG_SHADOW))  isolated = shadow.xxx;        // 1=lit, 0=shadowed
        else if (flag(PBR_FLAG_SPECULAR))isolated = specular * radiance * n_dot_l; // direct specular lobe
        else if (flag(PBR_FLAG_METALLIC))isolated = metallic.xxx;
        else if (flag(PBR_FLAG_ROUGHNESS))isolated = roughness.xxx;
        else if (flag(PBR_FLAG_DIFFUSE_IBL)) isolated = diffuse_ibl;
        else if (flag(PBR_FLAG_SPECULAR_IBL))isolated = specular_ibl;
        else if (flag(PBR_FLAG_MULTILIGHT))  isolated = multi_light;
        else if (flag(PBR_FLAG_AO))      isolated = aoTex.SampleLevel(screen_uv, 0.0).rrr; // 1=unoccluded, 0=occluded
        else if (flag(PBR_FLAG_EMISSIVE))isolated = emissive;
        else if (flag(PBR_FLAG_TRANSMISSION)) isolated = transmission;
        else if (flag(PBR_FLAG_TRANSLUCENCY)) isolated = translucency;
        else if (flag(PBR_FLAG_ANISOTROPY))   isolated = aniso_spec;
        else if (flag(PBR_FLAG_CLEARCOAT))    isolated = clearcoat;
        else if (flag(PBR_FLAG_GI))           isolated = gi_diffuse;
        SceneOutput o;
        o.color = float4(isolated, 1.0);
        float3 vn = normalize(mul((float3x3)frame.view, n));
        o.viewNormal = float4(vn, 0.0);
        return o;
    }

    // No component isolated: full PBR compose (all features on).
    // `direct` already carries the albedo via kD*albedo/PI, so raw albedo is
    // NOT added again -- otherwise shadowed faces (NdotL~0) would stay bright
    // and direct-light shading would look flat.
    float3 color = direct;
    color += diffuse_ibl + specular_ibl;   // Indirect: GI (when active) or IBL fallback
    color += multi_light + emissive;       // multilight + emissive
    color += transmission + translucency + aniso_spec + clearcoat;

    // Overall brightness (exposure). Applied here so it uniformly scales every
    // light component (direct, IBL, point lights, emissive, ...). The PostPass
    // tonemaps the result before outputting to the swapchain.
    color *= frame.exposure;

    // Output linear HDR color. Tonemapping (Reinhard / ACES) is applied by
    // the PostPass (post.slang), which samples this HDR attachment and writes
    // the sRGB-encoded result to the swapchain. Keeping the scene output
    // linear HDR lets future post effects (bloom, TAA) consume it without
    // lossy pre-tonemap encoding.
    SceneOutput o;
    o.color = float4(color, 1.0);
    // View-space normal for the GTAO pass: rotate the world-space shading
    // normal into view space. alpha=0 so the GTAO shader can distinguish a
    // written texel from a cleared one if needed.
    float3 vn = normalize(mul((float3x3)frame.view, n));
    o.viewNormal = float4(vn, 0.0);
    return o;
}
