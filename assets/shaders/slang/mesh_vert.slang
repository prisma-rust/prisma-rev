// Mesh vertex stage + Blinn-Phong fragment stage.
//
// Equivalent to shaders/mesh.vert + shaders/mesh.frag (GLSL). Compile with:
//   slangc mesh.slang -profile spirv_1_5 -target spirv \
//          -entry vertexMain -entry fragmentMain -o mesh.spv
// or per-stage to mesh.vert.spv / mesh.frag.spv (see compile.sh).
import common;

// set 0, binding 0 — shared per-frame UBO.
[[vk::binding(0, 0)]]
ConstantBuffer<FrameUBO> frame;

// set 0, binding 3 — bone palette storage buffer (#560 P0-3). `bone_offset`
// in the push constants selects the first matrix for this draw.
[[vk::binding(3, 0)]]
StructuredBuffer<float4x4> bones;

// set 0, binding 4 — previous-frame model matrices (#883 Phase 2). One entry
// per draw-list item; `prev_draw_index` selects this draw's entry. Separate from the
// bone palette so object motion and skinning can coexist.
[[vk::binding(4, 0)]]
StructuredBuffer<float4x4> prevModels;

// Push constant. MUST stay byte-identical to `PbrBindlessPush` in
// scene_frag.slang (both stages read the same block): model(64) |
// material_slot(4) | env_handle(4) | debug_flags(4) | bone_offset(4) |
// skin_mode(4) | instance_offset(4) | instance_count(4) |
// prev_draw_index(4) | reactive_mask(4) | splat_slot0..3(16) | splat_tiling(4).
struct MeshPush
{
    float4x4 model;
    uint     material_slot;
    uint     env_handle;
    uint     debug_flags;
    uint     bone_offset;
    uint     skin_mode;
    uint     instance_offset; // #491: first instance model in the bone SSBO
    uint     instance_count;  // #491: 0 = per-draw model
    uint     prev_draw_index; // #883 TAA: previous-model SSBO index
    uint     reactive_mask;   // #883 TAA: transparent draws drop history
    uint     splat_slot0;
    uint     splat_slot1;
    uint     splat_slot2;
    uint     splat_slot3;
    float    splat_tiling;
};
[[vk::push_constant]]
MeshPush push;

// Skinned vertex inputs live on a SECOND vertex buffer (binding 1), so the
// static-mesh pipeline never declares them (#560 P0-3).
struct SkinnedVertexIn
{
    float3 position : POSITION;
    float3 normal   : NORMAL;
    float3 color    : COLOR0;
    float2 uv       : TEXCOORD0;
    float4 tangent  : TANGENT;
    uint4  joints   : BLENDINDICES; // location 5
    float4 weights  : BLENDWEIGHT;  // location 6
};

// Blend up to four bone matrices for one vertex. Zero total weight falls back
// to joint 0 with weight 1 (a broken import must not collapse to the origin).
float4x4 blendBones(uint4 joints, float4 weights)
{
    float total = weights.x + weights.y + weights.z + weights.w;
    if (total <= 0.0001)
    {
        return bones[push.bone_offset + joints.x];
    }
    float4 w = weights / total;
    uint base = push.bone_offset;
    return bones[base + joints.x] * w.x
         + bones[base + joints.y] * w.y
         + bones[base + joints.z] * w.z
         + bones[base + joints.w] * w.w;
}

// ---------------------------------------------------------------------------
// Skin-weight heatmap (#560)
// ---------------------------------------------------------------------------
// `debug_flags` bit 15 (past the 15 PBR component bits) asks the forward pass
// to colour skinned vertices by their dominant bone instead of shading them,
// which is the only way to see whether the weights a glTF import produced are
// actually right (`docs/editor-egui.md` §4.3.1).
static const uint SKIN_WEIGHT_FLAG = 15u;

bool weightHeatmapOn() { return (push.debug_flags & (1u << SKIN_WEIGHT_FLAG)) != 0u; }

float3 hsv2rgb(float h, float s, float v)
{
    float3 k = abs(frac(h + float3(1.0, 2.0 / 3.0, 1.0 / 3.0)) * 6.0 - 3.0);
    return v * lerp(float3(1.0), saturate(k - 1.0), s);
}

// Hue = dominant joint (golden angle, so neighbouring bones never share a
// colour), value = that joint's weight. Heavily blended vertices therefore come
// out dimmer, and a vertex with no weights (static geometry, or a broken
// import) is neutral grey so it is clearly not part of the rig.
float3 weightHeat(uint4 joints, float4 weights)
{
    float total = weights.x + weights.y + weights.z + weights.w;
    if (total <= 0.0001)
    {
        return float3(0.16, 0.16, 0.18);
    }
    float4 w = weights / total;
    uint dominant = joints.x;
    float best = w.x;
    if (w.y > best) { best = w.y; dominant = joints.y; }
    if (w.z > best) { best = w.z; dominant = joints.z; }
    if (w.w > best) { best = w.w; dominant = joints.w; }

    float hue = frac(float(dominant) * 0.61803398875);
    return hsv2rgb(hue, 0.85, 0.35 + 0.65 * best);
}

MeshVaryings finishVertex(
    float4x4 model, float4x4 prevModel, float4 localPos, float3 localNormal,
    float3 color, float2 uv, float4 tangent, float3 skinHeat)
{
    MeshVaryings out;
    float4 worldPos = mul(model, localPos);
    out.worldPos    = worldPos.xyz;
    // Normal transform via mat3(model) — assumes uniform scale.
    float3x3 normalMat = float3x3(model);
    out.worldNormal = normalize(mul(normalMat, localNormal));
    out.color       = color;
    out.uv          = uv;
    out.tangent     = tangent;
    out.skinHeat    = skinHeat;
    out.clipPos     = mul(frame.viewProj, worldPos);

    // #883 Phase 2: object + camera motion. Current and previous positions are
    // projected independently, so camera movement and rigid-object movement
    // both produce a correct screen-space delta. Skinned/instanced draws pass
    // the current model as `prevModel` (camera-only motion for those).
    float4 prevWorld = mul(prevModel, localPos);
    float4 prevClip  = mul(frame.prevViewProj, prevWorld);
    float2 curUv  = out.clipPos.xy / max(out.clipPos.w, 1e-5) * 0.5 + 0.5;
    float2 prevUv = prevClip.xy / max(prevClip.w, 1e-5) * 0.5 + 0.5;
    out.velocity = (prevClip.w > 1e-5) ? (curUv - prevUv) : float2(0.0, 0.0);
    return out;
}

[shader("vertex")]
MeshVaryings vertexMain(MeshVertexIn in, uint instanceID : SV_InstanceID)
{
    // #491: static instances reuse the bone SSBO (binding 3) instead of adding
    // another descriptor. `instance_count == 0` keeps the per-draw push model.
    float4x4 model = push.model;
    float4x4 prevModel = model;
    if (push.instance_count > 0u)
    {
        // Instances move as a group; previous per-instance matrices are not
        // packed yet, so use the current model (camera-only motion).
        model = bones[push.instance_offset + instanceID];
        prevModel = model;
    }
    else
    {
        uint prevIndex = min(push.prev_draw_index, 4095u);
        prevModel = prevModels[prevIndex];
    }
    return finishVertex(
        model, prevModel, float4(in.position, 1.0), in.normal, in.color, in.uv,
        in.tangent, float3(0.0, 0.0, 0.0));
}

[shader("vertex")]
MeshVaryings vertexSkinnedMain(SkinnedVertexIn in)
{
    float4x4 skinMat = blendBones(in.joints, in.weights);
    float4 localPos = mul(skinMat, float4(in.position, 1.0));
    float3 localNormal = mul(float3x3(skinMat), in.normal);
    float3 heat = weightHeatmapOn() ? weightHeat(in.joints, in.weights) : float3(0.0, 0.0, 0.0);
    return finishVertex(
        push.model, push.model, localPos, localNormal, in.color, in.uv,
        in.tangent, heat);
}

static const float SHININESS = 32.0;

[shader("fragment")]
float4 fragmentMain(MeshVaryings in) : SV_Target
{
    float3 N = normalize(in.worldNormal);
    float3 L = normalize(frame.lightDirection.xyz);
    float3 V = normalize(frame.cameraPosition.xyz - in.worldPos);
    float3 H = normalize(L + V);

    float intensity      = frame.lightDirection.w;
    float ambient_factor = frame.lightColor.w;

    float3 ambient = ambient_factor * in.color;

    float NdotL   = max(dot(N, L), 0.0);
    float3 diffuse = NdotL * frame.lightColor.rgb * in.color * intensity;

    float NdotH    = max(dot(N, H), 0.0);
    float3 specular = pow(NdotH, SHININESS) * frame.lightColor.rgb * intensity;

    return float4(ambient + diffuse + specular, 1.0);
}
