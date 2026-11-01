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

// Push constant. MUST stay byte-identical to `PbrBindlessPush` in
// scene_frag.slang (both stages read the same block): model(64) |
// material_slot(4) | env_handle(4) | albedo_idx(4) | normal_idx(4) |
// debug_flags(4) | bone_offset(4) | skin_mode(4) | _pad(4) | _padding(12).
struct MeshPush
{
    float4x4 model;
    uint     material_slot;
    uint     env_handle;
    uint     albedo_idx;
    uint     normal_idx;
    uint     debug_flags;
    uint     bone_offset;
    uint     skin_mode;
    uint     _pad_92;
    uint3    _padding;
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

MeshVaryings finishVertex(
    float4 localPos, float3 localNormal,
    float3 color, float2 uv, float4 tangent)
{
    MeshVaryings out;
    float4 worldPos = mul(push.model, localPos);
    out.worldPos    = worldPos.xyz;
    // Normal transform via mat3(model) — assumes uniform scale.
    float3x3 normalMat = float3x3(push.model);
    out.worldNormal = normalize(mul(normalMat, localNormal));
    out.color       = color;
    out.uv          = uv;
    out.tangent     = tangent;
    out.clipPos     = mul(frame.viewProj, worldPos);
    return out;
}

[shader("vertex")]
MeshVaryings vertexMain(MeshVertexIn in)
{
    return finishVertex(float4(in.position, 1.0), in.normal, in.color, in.uv, in.tangent);
}

[shader("vertex")]
MeshVaryings vertexSkinnedMain(SkinnedVertexIn in)
{
    float4x4 skinMat = blendBones(in.joints, in.weights);
    float4 localPos = mul(skinMat, float4(in.position, 1.0));
    float3 localNormal = mul(float3x3(skinMat), in.normal);
    return finishVertex(localPos, localNormal, in.color, in.uv, in.tangent);
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
