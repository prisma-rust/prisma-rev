// Selection outline (inverted hull).
//
// Draws the selected mesh a second time with front faces culled and vertices
// pushed out along their world-space normals. The expanded back faces only
// survive the depth test outside the original silhouette, producing a clean
// outline that is correctly occluded by other scene geometry (#487).
import common;

[[vk::binding(0, 0)]]
ConstantBuffer<FrameUBO> frame;

struct OutlinePush
{
    float4x4 model;
    float4   color;
    float    thickness; // world units
};
[[vk::push_constant]]
OutlinePush push;

[shader("vertex")]
float4 vertexMain(MeshVertexIn in) : SV_Position
{
    float4 world = mul(push.model, float4(in.position, 1.0));
    // Matches mesh_vert: uniform-scale normal transform is enough for the
    // editor's selection hull.
    float3x3 normalMat = float3x3(push.model);
    float3 n = normalize(mul(normalMat, in.normal));
    world.xyz += n * max(push.thickness, 0.0);
    return mul(frame.viewProj, world);
}

[shader("fragment")]
float4 fragmentMain() : SV_Target
{
    return push.color;
}
