// Rasterized shadow map — depth-only pass (the rasterized fallback for the
// hybrid adaptive shadow system described in docs/DESIGN.md §2.3).
//
// When `VK_KHR_ray_query` is unavailable the renderer falls back to a classic
// depth-only shadow map: the scene is rendered from the light's point of view
// into a depth texture, which the lighting pass samples (with a comparison
// sampler) to decide lit vs shadowed.
//
// This module holds both stages:
//   vertexMain          — transform by model then light-space view-projection
//   vertexSkinnedMain   — same, after blending up to four bone matrices (#560)
//   fragmentMain        — empty (the pipeline has zero color attachments)
//
// Compile (see compile.sh):
//   slangc shadowmap.slang -profile spirv_1_5 -target spirv \
//          -entry vertexMain   -stage vertex   -o shadowmap.vert.spv
//   slangc shadowmap.slang -profile spirv_1_5 -target spirv \
//          -entry fragmentMain -stage fragment -o shadowmap.frag.spv
import common;

// set 0, binding 3 — bone palette (#560 P0-3c). The shadow pass keeps its own
// copy of the frame's palette so skinned casters can be drawn here too.
[[vk::binding(3, 0)]]
StructuredBuffer<float4x4> bones;

// Push constant: model (per-instance world transform) + light-space
// view-projection + the palette slice for this draw. 144 bytes.
struct ShadowPush
{
    float4x4 model;
    float4x4 lightViewProj;
    uint     bone_offset;
    uint3    _pad;
};
[[vk::push_constant]]
ShadowPush push;

// Skinned vertices live on a second vertex buffer (binding 1); the static
// pipeline never declares these attributes.
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

[shader("vertex")]
float4 vertexMain(MeshVertexIn in, uint instanceID : SV_InstanceID) : SV_Position
{
    // #491: static shadow casters can share one instanced draw. Instance
    // matrices are packed after the bone palette in the same SSBO; `_pad.y`
    // is the instance count, so 0 keeps the per-draw push model.
    float4x4 model = push.model;
    if (push._pad.y > 0u)
        model = bones[push._pad.x + instanceID];
    float4 worldPos = mul(model, float4(in.position, 1.0));
    return mul(push.lightViewProj, worldPos);
}

[shader("vertex")]
float4 vertexSkinnedMain(SkinnedVertexIn in) : SV_Position
{
    float4x4 skinMat = blendBones(in.joints, in.weights);
    float4 worldPos = mul(push.model, mul(skinMat, float4(in.position, 1.0)));
    return mul(push.lightViewProj, worldPos);
}


[shader("fragment")]
void fragmentMain()
{
    // Depth-only: the rasterizer writes depth from clipPos; no color output.
}
